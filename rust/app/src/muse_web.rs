use std::convert::Infallible;
use std::sync::LazyLock;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{StatusCode, header};
use futures_util::stream;
use reqwest::{Client, Response, ResponseBuilderExt, Url};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};

use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_http::{Delivery, http_client};
use crate::provider_index::js_trim;
use crate::providers::web_login::MusePage;
use crate::storage::Storage;

#[path = "muse_media.rs"]
mod media;
pub(super) use media::{image, video};

const ORIGIN: &str = "https://muse.ai";
pub(super) const THREAD: &str = "https://muse.ai/thread/new";
const SESSION: &str = "https://muse.ai/api/session";
const FIXTURE: &str = "http://127.0.0.1:52989/muse-web/api/session";
const TIMEOUT: Duration = Duration::from_secs(15);
const QUOTA_HINTS: &[&str] = &[
    "out of credits",
    "usage limit",
    "limit reached",
    "token limit",
];

const READY: &str = r#"(() => {
  const hydration = document.querySelector('[data-hatch-shell-hydration-state]');
  const head = document.body?.innerText || '';
  return { ready: document.readyState === 'complete' && !!document.querySelector('textarea')
    && (!hydration || hydration.getAttribute('data-hatch-shell-hydration-state') === 'hydrated')
    && !head.includes('Connecting...'), head: head.slice(0, 800) };
})()"#;

const CHAT_STATE: &str = r#"(() => {
  const bubbles = Array.from(document.querySelectorAll('div[class*="hatch-agent-bubble-bg"]'));
  return { agentCount: bubbles.length, lastText: (bubbles.at(-1)?.innerText || '').trim(),
    generating: !!document.querySelector('button[aria-label*="Stop" i]'),
    approval: !!document.querySelector('[data-testid="hatch-inline-approval-card"]'),
    tail: (document.body?.innerText || '').slice(-600) };
})()"#;

const SEND: &str = r#"(() => {
  const button = Array.from(document.querySelectorAll('button,[role=button]'))
    .find(b => b.offsetParent !== null && /send|发送/i.test((b.getAttribute('aria-label') || '') + ' ' + (b.getAttribute('data-testid') || '')));
  if (!button || button.disabled) return false;
  button.click();
  return true;
})()"#;

static CLIENT: LazyLock<Option<Client>> = LazyLock::new(|| http_client(TIMEOUT).build().ok());

pub(super) struct Failure {
    pub status: StatusCode,
    pub message: String,
}

impl Failure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn timed_out(message: &'static str) -> Self {
        Self::new(StatusCode::GATEWAY_TIMEOUT, message)
    }
}

pub(super) fn supports(account: &Value) -> bool {
    account["provider"] == "muse-web"
        && matches!(account["authType"].as_str(), Some("cookie" | "apikey"))
}

fn text_messages(body: &Value) -> bool {
    body["messages"].as_array().is_none_or(|messages| {
        messages.iter().all(|message| match &message["content"] {
            Value::Null | Value::String(_) => true,
            Value::Array(parts) => parts.iter().all(|part| {
                part.is_null()
                    || matches!(
                        part["type"].as_str(),
                        Some("text" | "input_text" | "output_text")
                    )
            }),
            _ => false,
        })
    })
}

pub(super) async fn complete(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    body: &Value,
    stream: bool,
) -> Result<Response, Failure> {
    for (field, reason) in [
        ("tools", "Muse Web does not support OpenAI function tools"),
        (
            "functions",
            "Muse Web does not support legacy function tools",
        ),
    ] {
        let declarations = &body[field];
        if !declarations.is_null()
            && declarations
                .as_array()
                .is_none_or(|items| !items.is_empty())
        {
            return Err(Failure::new(StatusCode::BAD_REQUEST, reason));
        }
    }
    if !text_messages(body) {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "Muse Web native chat supports text messages",
        ));
    }
    let prompt = prompt(body)
        .ok_or_else(|| Failure::new(StatusCode::BAD_REQUEST, "No user message found"))?;
    let (mut page, fixture) = open(storage, settings, account).await?;
    let cadence = Duration::from_millis(if fixture { 15 } else { 300 });
    let base = page.evaluate(CHAT_STATE).await.map_err(Failure::upstream)?;
    check_thread(&base)?;
    send(&mut page, &prompt, cadence).await?;
    let mut turn = Turn {
        page,
        cadence,
        started: Instant::now(),
        base_count: base["agentCount"].as_u64().unwrap_or_default(),
        base_text: base["lastText"].as_str().unwrap_or_default().to_owned(),
        emitted: String::new(),
        stable: 0,
    };
    let now = chrono::Utc::now();
    let id = format!("chatcmpl-muw-{}", now.timestamp_millis());
    let created = now.timestamp();
    let destination = Url::parse(THREAD).expect("fixed Muse thread URL");
    if stream {
        let model = model.to_owned();
        let output = stream::unfold(
            (Some(turn), id, model),
            move |(mut turn, id, model)| async move {
                let event = turn.as_mut()?.next().await;
                let data = match event {
                    Ok(Some(delta)) => {
                        sse(chunk(&id, &model, created, json!({"content":delta}), None))
                    }
                    Ok(None) => {
                        turn = None;
                        sse(chunk(&id, &model, created, json!({}), Some("stop")))
                            + "data: [DONE]\n\n"
                    }
                    Err(error) => {
                        turn = None;
                        sse(json!({"error":{"message":error.message,"type":"api_error"}}))
                            + "data: [DONE]\n\n"
                    }
                };
                Some((Ok::<_, Infallible>(Bytes::from(data)), (turn, id, model)))
            },
        );
        return axum::http::Response::builder()
            .url(destination)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(reqwest::Body::wrap_stream(output))
            .map(Into::into)
            .map_err(|_| Failure::upstream("Could not create the Muse stream"));
    }
    while turn.next().await?.is_some() {}
    let body = json!({"id":id,"object":"chat.completion","created":created,"model":model,
        "choices":[{"index":0,"message":{"role":"assistant","content":turn.emitted},"finish_reason":"stop","logprobs":null}],
        "usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}});
    axum::http::Response::builder()
        .url(destination)
        .header(header::CONTENT_TYPE, "application/json")
        .body(reqwest::Body::from(body.to_string()))
        .map(Into::into)
        .map_err(|_| Failure::upstream("Could not create the Muse response"))
}

async fn open(
    storage: &Storage,
    settings: &Value,
    account: &Value,
) -> Result<(MusePage, bool), Failure> {
    let route = Delivery::select(storage, settings, account, SESSION)
        .await
        .map_err(|_| Failure::upstream("Muse Web connection is unavailable"))?;
    if matches!(&route, Delivery::Tunnel(_)) {
        return Err(Failure::upstream(
            "Muse Web browser cannot use the configured proxy",
        ));
    }
    let fixture = route.use_fixture();
    let cookies = renew(&route, account).await?;
    let mut page = MusePage::open(&cookies).await.map_err(Failure::upstream)?;
    let cadence = Duration::from_millis(if fixture { 15 } else { 300 });
    ready(&mut page, cadence).await?;
    Ok((page, fixture))
}

fn prompt(body: &Value) -> Option<String> {
    let turns: Vec<_> = body["messages"]
        .as_array()?
        .iter()
        .filter_map(|message| {
            let role = message["role"]
                .as_str()
                .filter(|role| !role.is_empty())
                .unwrap_or("user");
            let text = match &message["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| part.get("text"))
                    .filter(|text| truthy(text))
                    .map(js_text)
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            let text = js_trim(&text);
            (!text.is_empty()).then(|| (role, text.to_owned()))
        })
        .collect();
    match turns.as_slice() {
        [] => None,
        [("user", text)] => Some(text.clone()),
        _ => {
            let mut blocks: Vec<_> = turns
                .iter()
                .map(|(role, text)| {
                    let label = match *role {
                        "system" | "developer" => "System",
                        "user" => "User",
                        "assistant" => "Assistant",
                        "tool" => "Tool",
                        other => other,
                    };
                    format!("[{label}]\n{text}")
                })
                .collect();
            blocks.push("[Instruction]\nContinue the conversation above and reply as the Assistant to the last User message. Output only the reply.".into());
            Some(blocks.join("\n\n"))
        }
    }
}

async fn renew(route: &Delivery, account: &Value) -> Result<Vec<(String, String)>, Failure> {
    let credential = [
        &account["apiKey"],
        &account["cookie"],
        &account["providerSpecificData"]["cookie"],
    ]
    .into_iter()
    .filter_map(Value::as_str)
    .map(js_trim)
    .find(|value| !value.is_empty())
    .unwrap_or_default();
    let raw = credential
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
        .map_or(credential, |_| js_trim(&credential[7..]));
    let mut jar = Vec::new();
    if let Ok(json) = serde_json::from_str::<Value>(raw)
        && let Some(cookies) = json.as_array().or_else(|| json["cookies"].as_array())
    {
        for cookie in cookies {
            if let (Some(name), Some(value)) = (cookie["name"].as_str(), cookie["value"].as_str()) {
                store_cookie(&mut jar, name, value);
            }
        }
    } else {
        for (name, value) in raw.split(';').filter_map(|item| item.split_once('=')) {
            store_cookie(&mut jar, js_trim(name), js_trim(value));
        }
    }
    if !jar.iter().any(|(name, _)| name == "hatch_sess") {
        return Err(Failure::new(
            StatusCode::UNAUTHORIZED,
            "Missing hatch_sess cookie",
        ));
    }
    let raw = jar
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    let mut cookie = header::HeaderValue::from_str(&raw)
        .map_err(|_| Failure::new(StatusCode::UNAUTHORIZED, "Invalid Muse session cookies"))?;
    cookie.set_sensitive(true);
    let client = route
        .client(
            CLIENT
                .as_ref()
                .ok_or_else(|| Failure::upstream("Muse session HTTP client is unavailable"))?,
            TIMEOUT,
        )
        .ok_or_else(|| Failure::upstream("Muse session HTTP client is unavailable"))?;
    let endpoint = if route.use_fixture() {
        FIXTURE
    } else {
        SESSION
    };
    let response = client
        .get(endpoint)
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, ORIGIN)
        .header(header::REFERER, THREAD)
        .header(header::ACCEPT, "application/json")
        .header(header::USER_AGENT, "9Router")
        .send()
        .await
        .map_err(|_| Failure::upstream("Muse session endpoint is unavailable"))?;
    if matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        return Err(Failure::new(
            StatusCode::UNAUTHORIZED,
            "Muse rejected the saved session",
        ));
    }
    if !response.status().is_success() {
        return Err(Failure::upstream(format!(
            "Muse session endpoint returned HTTP {}",
            response.status().as_u16()
        )));
    }
    for header in response.headers().get_all(header::SET_COOKIE) {
        if let Ok(header) = header.to_str()
            && let Some((name, value)) = header
                .split(';')
                .next()
                .and_then(|pair| pair.split_once('='))
        {
            store_cookie(&mut jar, js_trim(name), js_trim(value));
        }
    }
    Ok(jar)
}

fn store_cookie(jar: &mut Vec<(String, String)>, name: &str, value: &str) {
    if name.is_empty()
        || value.is_empty()
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "path" | "domain" | "expires" | "max-age" | "secure" | "httponly" | "samesite"
        )
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_\x60|~".contains(&byte))
        || !value
            .bytes()
            .all(|byte| matches!(byte,0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e))
    {
        return;
    }
    if let Some((_, saved)) = jar.iter_mut().find(|(saved, _)| saved == name) {
        *saved = value.to_owned();
    } else {
        jar.push((name.to_owned(), value.to_owned()));
    }
}

async fn ready(page: &mut MusePage, cadence: Duration) -> Result<(), Failure> {
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut last = Value::Null;
    while Instant::now() < deadline {
        if let Ok(state) = page.evaluate(READY).await {
            if state["ready"] == true {
                return Ok(());
            }
            last = state;
        }
        sleep(cadence).await;
    }
    let head = last["head"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if [
        "log in",
        "sign in",
        "create an account",
        "use another account",
    ]
    .into_iter()
    .any(|hint| head.contains(hint))
    {
        return Err(Failure::new(
            StatusCode::UNAUTHORIZED,
            "Muse redirected to sign-in",
        ));
    }
    Err(Failure::timed_out("Muse thread did not become ready"))
}

async fn send(page: &mut MusePage, prompt: &str, cadence: Duration) -> Result<(), Failure> {
    let text =
        serde_json::to_string(prompt).map_err(|_| Failure::upstream("Invalid Muse prompt"))?;
    let expression = format!(
        r#"(() => {{
      const input = document.querySelector('textarea');
      if (!input) return false;
      input.focus();
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set.call(input,{text});
      input.dispatchEvent(new Event('input',{{bubbles:true}}));
      return true;
    }})()"#
    );
    if page
        .evaluate(&expression)
        .await
        .map_err(Failure::upstream)?
        != true
    {
        return Err(Failure::upstream("Muse composer was unavailable"));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if page.evaluate(SEND).await.map_err(Failure::upstream)? == true {
            return Ok(());
        }
        sleep(cadence).await;
    }
    Err(Failure::upstream("Muse send button was unavailable"))
}

fn check_thread(state: &Value) -> Result<(), Failure> {
    if !state.is_object() {
        return Err(Failure::upstream("Muse thread navigated away"));
    }
    if state["approval"] == true {
        return Err(Failure::new(
            StatusCode::FORBIDDEN,
            "Muse Web requires approval in the provider interface",
        ));
    }
    Ok(())
}

struct Turn {
    page: MusePage,
    cadence: Duration,
    started: Instant,
    base_count: u64,
    base_text: String,
    emitted: String,
    stable: usize,
}

impl Turn {
    async fn next(&mut self) -> Result<Option<String>, Failure> {
        loop {
            let elapsed = self.started.elapsed();
            if elapsed > Duration::from_secs(300) {
                return Err(Failure::timed_out("Muse reply timed out"));
            }
            if self.emitted.is_empty() && elapsed > Duration::from_secs(45) {
                return Err(Failure::timed_out("Muse returned no assistant reply"));
            }
            sleep(self.cadence).await;
            let state = self
                .page
                .evaluate(CHAT_STATE)
                .await
                .map_err(Failure::upstream)?;
            check_thread(&state)?;
            let text = state["lastText"].as_str().unwrap_or_default();
            let tail = state["tail"].as_str().unwrap_or_default();
            let generating = state["generating"] == true;
            let low = tail.to_ascii_lowercase();
            if !generating
                && QUOTA_HINTS
                    .iter()
                    .find(|hint| low.contains(**hint))
                    .is_some_and(|hint| text.to_ascii_lowercase().contains(*hint))
            {
                return Err(Failure::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Muse reports that this account is out of quota",
                ));
            }
            let fresh = state["agentCount"].as_u64().unwrap_or_default() > self.base_count
                || (!text.is_empty() && text != self.base_text);
            if !fresh || text.is_empty() {
                if elapsed > Duration::from_secs(15)
                    && ["Still sending", "Connecting..."]
                        .into_iter()
                        .any(|hint| tail.contains(hint))
                {
                    return Err(Failure::timed_out("Muse workspace is still connecting"));
                }
                continue;
            }
            if text != self.emitted {
                let delta = text.strip_prefix(&self.emitted).unwrap_or(text).to_owned();
                self.emitted = text.to_owned();
                self.stable = 0;
                return Ok(Some(delta));
            }
            self.stable += 1;
            if (!generating && self.stable >= 4) || self.stable >= 12 {
                return Ok(None);
            }
        }
    }
}

fn chunk(id: &str, model: &str, created: i64, delta: Value, reason: Option<&str>) -> Value {
    json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,
        "choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
}

fn sse(value: Value) -> String {
    format!("data: {value}\n\n")
}
