use std::collections::BTreeMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::http::{StatusCode, header};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::stream;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};

use crate::mimo_account::valid_cookie;
use crate::provider_http::Delivery;
use crate::provider_index::js_trim;
use crate::providers::web_login::{ManagedPage, identity};
use crate::storage::Storage;

pub(super) const APP: &str = "https://gemini.google.com/app";
const SID: &str = "__Secure-1PSID";
const SID_TS: &str = "__Secure-1PSIDTS";
const INTERVAL: Duration = Duration::from_millis(400);
const READY: &str = r#"(() => {
    const text = (document.body?.innerText || '').slice(0, 3000).toLowerCase();
    if (text.includes('used all of your requests') || text.includes('out of quota')) return 'quota';
    if ([...document.querySelectorAll('.ql-editor, [contenteditable="true"]')].some(e => e.getClientRects().length)) return 'ready';
    if (text.includes('chat with gemini') || text.includes('something went wrong')) return 'sign-in';
    return 'loading';
})()"#;
const REPLY: &str = r#"(() => {
    const messages = [...document.querySelectorAll('model-response, message-content')];
    return {count:messages.length,text:(messages.at(-1)?.innerText || '').trim(),
        generating:!!document.querySelector('[data-test-id="thinking-overlay-content"], button[aria-label*="Stop" i]')};
})()"#;
const IMAGE: &str = r#"(() => {
    if (document.querySelector('[data-test-id="thinking-overlay-content"], button[aria-label*="Stop" i]')) return null;
    const messages = [...document.querySelectorAll('model-response, message-content')];
    const images = [...(messages.at(-1)?.querySelectorAll('img') || [])];
    const image = images.reverse().find(i => i.complete && i.naturalWidth >= 100 && i.naturalHeight >= 100 && i.naturalWidth*i.naturalHeight <= 40000000 && !(i.currentSrc || i.src).includes('gstatic'));
    if (!image) return null;
    try {
        const canvas = document.createElement('canvas');
        canvas.width = image.naturalWidth; canvas.height = image.naturalHeight;
        canvas.getContext('2d').drawImage(image,0,0);
        const data = canvas.toDataURL('image/png');
        return data.startsWith('data:image/png;base64,') && data.length < 28*1024*1024 ? data.slice(22) : null;
    } catch { return null; }
})()"#;

pub(crate) struct Failure {
    pub status: StatusCode,
    pub message: String,
}

impl Failure {
    fn new(status: StatusCode, message: &str) -> Self {
        Self {
            status,
            message: message.to_owned(),
        }
    }

    fn bad(message: &str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn unavailable(message: &str) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }
}

pub(crate) fn supports(account: &Value) -> bool {
    account["provider"] == "gemini-web"
        && matches!(account["authType"].as_str(), Some("cookie" | "apikey"))
}

fn cookies(account: &Value) -> Result<Vec<(String, String)>, Failure> {
    let details = &account["providerSpecificData"];
    let take = |value: &Value| {
        value
            .as_str()
            .map(js_trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let raw = take(&account["apiKey"])
        .or_else(|| take(&account["cookie"]))
        .or_else(|| take(&details["cookie"]));
    let mut pairs = BTreeMap::new();
    if let Some(raw) = raw {
        let normalized = identity::normalize("gemini-web", &raw);
        for (name, value) in normalized
            .split(';')
            .filter_map(|part| part.trim().split_once('='))
        {
            if matches!(name, SID | SID_TS) && valid_cookie(value) {
                pairs.insert(name.to_owned(), value.to_owned());
            }
        }
        if pairs.is_empty() && raw.trim_end_matches('=').find('=').is_none() && valid_cookie(&raw) {
            pairs.insert(SID.to_owned(), raw);
        }
    } else {
        for name in [SID, SID_TS] {
            if let Some(raw) = take(&details[name]) {
                let value = raw
                    .strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix('='))
                    .unwrap_or(&raw);
                if valid_cookie(value) {
                    pairs.insert(name.to_owned(), value.to_owned());
                }
            }
        }
    }
    if !pairs.contains_key(SID) {
        return Err(Failure::new(
            StatusCode::UNAUTHORIZED,
            "Missing Gemini cookies",
        ));
    }
    Ok(pairs.into_iter().collect())
}

async fn open(
    storage: &Storage,
    settings: &Value,
    account: &Value,
) -> Result<(ManagedPage, Delivery), Failure> {
    let delivery = Delivery::select(storage, settings, account, APP)
        .await
        .map_err(|_| Failure::unavailable("Gemini Web cannot use the configured route"))?;
    if matches!(delivery, Delivery::Tunnel(_)) {
        return Err(Failure::unavailable(
            "Gemini Web browser requires direct access or explicit proxy bypass",
        ));
    }
    let cookies = cookies(account)?;
    let mut page = ManagedPage::open_gemini(&cookies)
        .await
        .map_err(Failure::unavailable)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match page
            .evaluate(READY)
            .await
            .ok()
            .as_ref()
            .and_then(Value::as_str)
        {
            Some("ready") => return Ok((page, delivery)),
            Some("quota") => {
                return Err(Failure::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Gemini Web account quota is exhausted",
                ));
            }
            Some("sign-in") => {
                return Err(Failure::new(
                    StatusCode::UNAUTHORIZED,
                    "Gemini Web session could not open the composer",
                ));
            }
            _ => sleep(INTERVAL).await,
        }
    }
    Err(Failure::unavailable("Gemini Web composer did not load"))
}

async fn submit(
    storage: &Storage,
    account: &Value,
    expected: &Delivery,
    page: &mut ManagedPage,
    prompt: &str,
) -> Result<(), Failure> {
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(|| Failure::unavailable("Gemini Web route is unavailable"))?;
    if !matches!(Delivery::select(storage, &settings, account, APP).await, Ok(delivery) if delivery == *expected)
    {
        return Err(Failure::unavailable(
            "Gemini Web route changed before submission",
        ));
    }
    page.submit_gemini(prompt)
        .await
        .map_err(Failure::unavailable)
}

async fn verify_mode(page: &mut ManagedPage, model: &str, thinking: bool) -> Result<(), Failure> {
    let mode = match model {
        "gemini-3.1-pro" => None,
        "gemini-3.7-flash" => Some(r"\bflash\b(?!.*\blite\b)"),
        "gemini-3.1-flash-lite" => Some(r"flash.*\blite\b"),
        _ => return Err(Failure::bad("Unknown Gemini Web model")),
    };
    if let Some(pattern) = mode {
        choose(page,r#"[data-test-id="bard-mode-menu-button"], button[aria-haspopup="menu"][aria-label*="model" i]"#,
            r#"[data-test-id="bard-mode-menu-button"] .mode-title, [data-test-id="bard-mode-menu-button"]"#,pattern).await?;
    }
    if thinking {
        choose(page,r#"[data-test-id="deep-think-toggle"],button[aria-label*="extended thinking" i],button[aria-label*="deep think" i]"#,
            r#"[data-test-id="deep-think-toggle"][aria-pressed="true"],[data-test-id="deep-think-toggle"].is-active"#,"thinking|deep think").await?;
    }
    Ok(())
}

async fn choose(
    page: &mut ManagedPage,
    control: &str,
    indicator: &str,
    pattern: &str,
) -> Result<(), Failure> {
    let check = format!(
        r#"(() => [...document.querySelectorAll({})].some(e => e.getClientRects().length && new RegExp({},'i').test((e.innerText || '') + ' ' + (e.getAttribute('aria-label') || ''))))()"#,
        json!(indicator),
        json!(pattern)
    );
    if page.evaluate(&check).await.ok() == Some(Value::Bool(true)) {
        return Ok(());
    }
    let click = format!(
        r#"(() => {{const nodes=[...document.querySelectorAll({})].filter(e => e.getClientRects().length && !e.disabled && e.getAttribute('aria-disabled') !== 'true'); if (nodes.length !== 1) return false; nodes[0].click(); return true;}})()"#,
        json!(control)
    );
    if page.evaluate(&click).await.ok() != Some(Value::Bool(true)) {
        return Err(Failure::bad("Gemini Web mode control is unavailable"));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if page.evaluate(&check).await.ok() == Some(Value::Bool(true)) {
            return Ok(());
        }
        sleep(INTERVAL).await;
    }
    Err(Failure::bad(
        "Gemini Web could not confirm the selected mode",
    ))
}

fn prompt(body: &Value) -> Result<String, Failure> {
    for field in ["tools", "functions"] {
        if !body[field].is_null() && !body[field].as_array().is_some_and(Vec::is_empty) {
            return Err(Failure::bad("Gemini Web does not support function tools"));
        }
    }
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| Failure::bad("No user message found"))?;
    let turns = messages
        .iter()
        .filter_map(|message| Some((message["role"].as_str()?, message["content"].as_str()?)))
        .filter(|(_, text)| !js_trim(text).is_empty())
        .collect::<Vec<_>>();
    let index = turns
        .iter()
        .rposition(|(role, _)| *role == "user")
        .ok_or_else(|| Failure::bad("No user message found"))?;
    let system = turns
        .iter()
        .filter(|(role, _)| *role == "system")
        .map(|(_, text)| *text)
        .collect::<Vec<_>>()
        .join("\n\n");
    let prior = turns[..index]
        .iter()
        .filter_map(|(role, text)| match *role {
            "assistant" => Some(format!("Assistant: {text}")),
            "user" => Some(format!("User: {text}")),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut parts = Vec::new();
    if !system.is_empty() {
        parts.push(format!("System:\n{system}"));
    }
    if !prior.is_empty() {
        parts.push(format!("Previous conversation:\n{}", prior.join("\n\n")));
        parts.push(format!("Current user message:\n{}", turns[index].1));
    } else {
        parts.push(turns[index].1.to_owned());
    }
    let text = parts.join("\n\n");
    if text.len() > 4 * 1024 * 1024 {
        return Err(Failure::bad("Gemini Web prompt is too large"));
    }
    Ok(text)
}

pub(crate) async fn complete(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    body: &Value,
    streaming: bool,
) -> Result<reqwest::Response, Failure> {
    let prompt = prompt(body)?;
    let model = model
        .rsplit('/')
        .next()
        .filter(|model| !model.is_empty())
        .unwrap_or("gemini-3.1-pro");
    let thinking = body["reasoning_effort"]
        .as_str()
        .map(js_trim)
        .filter(|effort| !effort.is_empty())
        .is_some_and(|effort| !matches!(effort.to_ascii_lowercase().as_str(), "none" | "minimal"));
    let (mut page, delivery) = open(storage, settings, account).await?;
    verify_mode(&mut page, model, thinking).await?;
    let baseline = page.evaluate(REPLY).await.map_err(Failure::unavailable)?;
    submit(storage, account, &delivery, &mut page, &prompt).await?;
    let mut turn = Turn {
        page,
        baseline,
        started: Instant::now(),
        previous: String::new(),
        emitted: String::new(),
        stable: 0,
        done: false,
        id: format!("chatcmpl-gweb-{}", uuid::Uuid::new_v4()),
        model: model.to_owned(),
        created: chrono::Utc::now().timestamp(),
    };
    if streaming {
        let chunks = stream::unfold(turn, |mut turn| async move {
            if turn.done {
                return None;
            }
            let value = match turn.next().await {
                Ok(Some(delta)) => format!(
                    "data: {}\n\n",
                    turn.chunk(json!({"content":delta}), Value::Null)
                ),
                Ok(None) => {
                    turn.done = true;
                    format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        turn.chunk(json!({}), json!("stop"))
                    )
                }
                Err(failure) => {
                    turn.done = true;
                    format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({"error":{"message":failure.message}})
                    )
                }
            };
            Some((Ok::<_, Infallible>(value.into_bytes()), turn))
        });
        return response("text/event-stream", reqwest::Body::wrap_stream(chunks));
    }
    while turn.next().await?.is_some() {}
    response("application/json",json!({"id":turn.id,"created":turn.created,"model":turn.model,"object":"chat.completion",
       "choices":[{"index":0,"message":{"role":"assistant","content":turn.emitted},"finish_reason":"stop","logprobs":null}],
       "usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0},
    }).to_string())
}

fn response<T: Into<reqwest::Body>>(
    content_type: &str,
    body: T,
) -> Result<reqwest::Response, Failure> {
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body.into())
        .map(reqwest::Response::from)
        .map_err(|_| Failure::unavailable("Could not build Gemini Web response"))
}

struct Turn {
    page: ManagedPage,
    baseline: Value,
    started: Instant,
    previous: String,
    emitted: String,
    stable: u8,
    done: bool,
    id: String,
    model: String,
    created: i64,
}

impl Turn {
    fn chunk(&self, delta: Value, finish: Value) -> Value {
        json!({"id":self.id,"model":self.model,"created":self.created,"object":"chat.completion.chunk",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    }

    async fn next(&mut self) -> Result<Option<String>, Failure> {
        loop {
            if self.started.elapsed() > Duration::from_secs(60)
                || self.previous.is_empty() && self.started.elapsed() > Duration::from_secs(30)
            {
                return Err(Failure::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "Gemini Web reply timed out",
                ));
            }
            sleep(INTERVAL).await;
            let state = self
                .page
                .evaluate(REPLY)
                .await
                .map_err(Failure::unavailable)?;
            let text = state["text"].as_str().unwrap_or_default();
            if text.is_empty()
                || state["count"].as_u64() <= self.baseline["count"].as_u64()
                    && state["text"] == self.baseline["text"]
            {
                continue;
            }
            if text == self.previous {
                self.stable = self.stable.saturating_add(1);
            } else {
                self.previous = text.to_owned();
                self.stable = 0;
            }
            let generating = state["generating"] == true;
            if (!generating || self.stable >= 20) && text != self.emitted {
                let delta = text.strip_prefix(&self.emitted).unwrap_or(text).to_owned();
                self.emitted = text.to_owned();
                return Ok(Some(delta));
            }
            if !generating && self.stable >= 5 || self.stable >= 20 {
                return Ok(None);
            }
        }
    }
}

pub(crate) async fn image(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    body: &Value,
) -> Result<Value, Failure> {
    let prompt = body["prompt"]
        .as_str()
        .map(js_trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| Failure::bad("prompt is required"))?;
    let n = body["n"].as_u64().unwrap_or(1);
    if !(1..=4).contains(&n) {
        return Err(Failure::bad(
            "Gemini Web supports 1 to 4 images per request",
        ));
    }
    let size = body["size"]
        .as_str()
        .filter(|size| !size.is_empty())
        .map(|size| format!(" (aspect ratio {size})"))
        .unwrap_or_default();
    let prompt = format!("Generate an image{size}: {prompt}");
    if prompt.len() > 4 * 1024 * 1024 {
        return Err(Failure::bad("Gemini Web prompt is too large"));
    }
    let mut data = Vec::new();
    for _ in 0..n {
        let (mut page, delivery) = open(storage, settings, account).await?;
        submit(storage, account, &delivery, &mut page, &prompt).await?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let encoded = loop {
            if Instant::now() >= deadline {
                return Err(Failure::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "Gemini Web image did not become available",
                ));
            }
            sleep(Duration::from_secs(1)).await;
            match page.evaluate(IMAGE).await.map_err(Failure::unavailable)? {
                Value::String(encoded) => break encoded,
                _ => continue,
            }
        };
        if STANDARD.decode(&encoded).ok().is_none_or(|bytes| {
            bytes.len() > 20 * 1024 * 1024 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        }) {
            return Err(Failure::unavailable("Invalid Gemini Web image artifact"));
        }
        data.push(json!({"b64_json":encoded,"mime_type":"image/png"}));
    }
    Ok(json!({"created":chrono::Utc::now().timestamp(),"data":data}))
}
