use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use futures_util::stream;
use regex::Regex;
use reqwest::{
    Body, Client, Method, RequestBuilder, Response, ResponseBuilderExt, StatusCode, Url, header,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio::time::Instant;

use crate::chat_input::truthy;
use crate::provider_http::{Delivery, http_client};
use crate::provider_index::js_trim;
use crate::sse_reader::{ReadLine, SseReader};
use crate::storage::Storage;

#[path = "deepseek_web_pow.rs"]
mod pow;
#[path = "deepseek_web_tools.rs"]
mod tools;

use tools::ClientTools;

const ORIGIN: &str = "https://chat.deepseek.com";
const CURRENT: &str = "/api/v0/users/current";
const CREATE: &str = "/api/v0/chat_session/create";
const DELETE: &str = "/api/v0/chat_session/delete";
const CHALLENGE: &str = "/api/v0/chat/create_pow_challenge";
const COMPLETION: &str = "/api/v0/chat/completion";
const TIMEOUT: Duration = Duration::from_secs(120);
const DRAIN: Duration = Duration::from_millis(750);
const JSON_LIMIT: usize = 256 * 1024;
const RESPONSE_LIMIT: usize = 16 * 1024 * 1024;
const SESSION_LIMIT: usize = 100;
type SessionSlot = Arc<AsyncMutex<Option<String>>>;
static SESSIONS: Mutex<VecDeque<([u8; 32], SessionSlot)>> = Mutex::new(VecDeque::new());

static CITATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[citation:(\d+)\]").expect("static DeepSeek citation"));
static SEARCH_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(SEARCH|WEB_SEARCH|SEARCHING)\s*").expect("static DeepSeek marker")
});
static IMAGE_MARKDOWN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[.*?\]\(.*?\)").expect("static image markdown"));

pub(super) struct Failure {
    pub status: StatusCode,
    pub message: String,
}

impl Failure {
    fn new(status: StatusCode, message: &'static str) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn upstream(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "DeepSeek Web session is missing, invalid or expired",
        )
    }
}

pub(super) fn supports(account: &Value) -> bool {
    user_token(account).is_some()
}

pub(super) fn completion_url() -> String {
    format!("{ORIGIN}{COMPLETION}")
}

fn user_token(account: &Value) -> Option<String> {
    if account["provider"] != "deepseek-web"
        || !matches!(
            account["authType"].as_str(),
            Some("cookie" | "apikey" | "api_key" | "oauth")
        )
    {
        return None;
    }
    let raw = [&account["apiKey"], &account["accessToken"]]
        .into_iter()
        .filter_map(Value::as_str)
        .map(js_trim)
        .find(|value| !value.is_empty())?;
    let parsed = serde_json::from_str::<Value>(raw).ok();
    let token = parsed
        .as_ref()
        .and_then(|body| body["value"].as_str())
        .unwrap_or(raw);
    authorization(token).map(|_| token.to_owned())
}

fn authorization(token: &str) -> Option<header::HeaderValue> {
    if token.is_empty()
        || token.len() > 8192
        || !token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'+' | b'/' | b'=')
        })
    {
        return None;
    }
    let mut value = header::HeaderValue::from_str(&format!("Bearer {token}")).ok()?;
    value.set_sensitive(true);
    Some(value)
}

fn request(
    client: &Client,
    method: Method,
    path: &'static str,
    token: &header::HeaderValue,
) -> RequestBuilder {
    client
        .request(method, format!("{ORIGIN}{path}"))
        .header(header::AUTHORIZATION, token.clone())
        .header(header::USER_AGENT, "9Router")
        .header(header::ACCEPT, "application/json, text/event-stream")
}

async fn send(builder: RequestBuilder) -> Result<Response, Failure> {
    let response = builder.send().await.map_err(|error| {
        if error.is_timeout() {
            Failure::new(
                StatusCode::GATEWAY_TIMEOUT,
                "DeepSeek Web upstream timed out",
            )
        } else {
            Failure::upstream("Could not reach DeepSeek Web")
        }
    })?;
    if response.headers().contains_key("cf-mitigated")
        || response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|mime| mime.to_ascii_lowercase().starts_with("text/html"))
    {
        return Err(Failure::upstream(
            "DeepSeek Web did not accept the native request",
        ));
    }
    match response.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(Failure::unauthorized()),
        StatusCode::TOO_MANY_REQUESTS => Err(Failure::new(
            StatusCode::TOO_MANY_REQUESTS,
            "DeepSeek Web rate limited the request",
        )),
        status @ (StatusCode::NOT_FOUND | StatusCode::GONE) => Err(Failure::new(
            status,
            "DeepSeek Web resource is no longer available",
        )),
        status if !status.is_success() => {
            Err(Failure::upstream("DeepSeek Web rejected the request"))
        }
        _ => Ok(response),
    }
}

async fn envelope(builder: RequestBuilder) -> Result<Value, Failure> {
    let mut response = send(builder).await?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Failure::upstream("Could not read DeepSeek Web reply"))?
    {
        if chunk.len() > JSON_LIMIT - bytes.len() {
            return Err(Failure::upstream("DeepSeek Web reply was too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let data: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Failure::upstream("DeepSeek Web did not return JSON"))?;
    if data["code"] == 40003 || data["code"] == "40003" {
        return Err(Failure::unauthorized());
    }
    if truthy(&data["code"]) && data["code"] != "0"
        || truthy(&data["data"]["biz_code"]) && data["data"]["biz_code"] != "0"
    {
        return Err(Failure::upstream("DeepSeek Web rejected the request"));
    }
    data["data"]
        .get("biz_data")
        .or_else(|| data.get("biz_data"))
        .filter(|body| body.is_object())
        .cloned()
        .ok_or_else(|| Failure::upstream("DeepSeek Web returned an incomplete reply"))
}

struct Session {
    client: Client,
    authorization: header::HeaderValue,
    id: String,
    cache: Option<OwnedMutexGuard<Option<String>>>,
    reused: bool,
    retained: bool,
}

impl Session {
    async fn next_id(&self) -> Result<String, Failure> {
        create_session(&self.client, &self.authorization).await
    }

    fn remember(&mut self) {
        if let Some(mut cache) = self.cache.take() {
            *cache = Some(self.id.clone());
            self.retained = true;
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.retained {
            return;
        }
        let request = request(&self.client, Method::POST, DELETE, &self.authorization)
            .timeout(Duration::from_secs(5))
            .json(&json!({"chat_session_id":self.id}));
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = request.send().await;
            });
        }
    }
}

async fn create_session(
    client: &Client,
    authorization: &header::HeaderValue,
) -> Result<String, Failure> {
    let created =
        envelope(request(client, Method::POST, CREATE, authorization).json(&json!({}))).await?;
    created["chat_session"]["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 256)
        .map(str::to_owned)
        .ok_or_else(|| Failure::upstream("DeepSeek Web did not create a session"))
}

fn session_slot(
    storage: &Storage,
    account: &Value,
    token: &str,
    caller: &str,
    delivery: &Delivery,
) -> Result<SessionSlot, Failure> {
    let route = match delivery {
        Delivery::Direct => "direct",
        Delivery::Bypass => "bypass",
        Delivery::Tunnel(url) => url.as_str(),
    };
    let mut hash = Sha256::new();
    hash.update(storage.root().as_os_str().as_encoded_bytes());
    for value in [
        account["id"].as_str().unwrap_or_default(),
        token,
        caller,
        route,
    ] {
        hash.update([0]);
        hash.update(value.as_bytes());
    }
    let key: [u8; 32] = hash.finalize().into();
    let mut slots = SESSIONS
        .lock()
        .map_err(|_| Failure::upstream("DeepSeek Web sessions are unavailable"))?;
    if let Some(index) = slots.iter().position(|(hash, _)| *hash == key)
        && let Some(entry) = slots.remove(index)
    {
        let slot = entry.1.clone();
        slots.push_back(entry);
        return Ok(slot);
    }
    if slots.len() == SESSION_LIMIT {
        let index = slots
            .iter()
            .position(|(_, slot)| Arc::strong_count(slot) == 1)
            .ok_or_else(|| {
                Failure::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Too many active DeepSeek Web sessions",
                )
            })?;
        slots.remove(index);
    }
    let slot = Arc::new(AsyncMutex::new(None));
    slots.push_back((key, slot.clone()));
    Ok(slot)
}

fn reference_ids(body: &Value) -> Result<Vec<String>, Failure> {
    if body["ref_file_ids"].is_null() {
        return Ok(Vec::new());
    }
    let invalid = || {
        Failure::new(
            StatusCode::BAD_REQUEST,
            "DeepSeek Web file references must be remote file IDs",
        )
    };
    let ids = body["ref_file_ids"]
        .as_array()
        .filter(|ids| ids.len() <= 128)
        .ok_or_else(invalid)?;
    ids.iter()
        .map(|id| {
            id.as_str()
                .filter(|value| {
                    !value.is_empty()
                        && value.len() <= 256
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
                })
                .map(str::to_owned)
                .ok_or_else(invalid)
        })
        .collect()
}

pub(super) async fn complete(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    caller: Option<&str>,
    model: &str,
    body: &Value,
    streaming: bool,
) -> Result<Response, Failure> {
    let token = user_token(account).ok_or_else(Failure::unauthorized)?;
    let user_authorization = authorization(&token).ok_or_else(Failure::unauthorized)?;
    let refs = reference_ids(body)?;
    let tools = ClientTools::from_request(body)?;
    let prompt = prompt(body, account, tools.as_ref())?;
    let route = Delivery::select(storage, settings, account, ORIGIN)
        .await
        .map_err(|_| Failure::upstream("DeepSeek Web cannot use the configured route"))?;
    let direct = http_client(TIMEOUT)
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| Failure::upstream("Could not initialize DeepSeek Web transport"))?;
    let client = route
        .client(&direct, TIMEOUT)
        .ok_or_else(|| Failure::upstream("Could not initialize the DeepSeek Web route"))?;
    let mut cache = if account["providerSpecificData"]["persistSession"] == true
        && let Some(caller) = caller.filter(|caller| !caller.is_empty())
    {
        let slot = session_slot(storage, account, &token, caller, &route)?;
        Some(
            tokio::time::timeout(TIMEOUT, slot.lock_owned())
                .await
                .map_err(|_| {
                    Failure::new(StatusCode::GATEWAY_TIMEOUT, "DeepSeek Web session is busy")
                })?,
        )
    } else {
        None
    };
    let latest = storage
        .read_settings()
        .await
        .ok_or_else(|| Failure::upstream("DeepSeek Web route is unavailable"))?;
    if !Delivery::select(storage, &latest, account, ORIGIN)
        .await
        .is_ok_and(|current| current == route)
    {
        return Err(Failure::upstream(
            "DeepSeek Web route changed before submission",
        ));
    }
    let current = envelope(request(&client, Method::GET, CURRENT, &user_authorization)).await?;
    let access = current["token"]
        .as_str()
        .ok_or_else(Failure::unauthorized)?;
    let authorization = authorization(access).ok_or_else(Failure::unauthorized)?;
    let saved = cache.as_deref_mut().and_then(Option::take);
    let reused = saved.is_some();
    let id = match saved {
        Some(saved) => saved,
        None => create_session(&client, &authorization).await?,
    };
    let mut session = Session {
        client,
        authorization,
        id,
        cache,
        reused,
        retained: false,
    };
    let options = Model::new(model, body);
    let response = match submit(&session, options, &prompt, &refs).await {
        Err(failure)
            if session.reused
                && matches!(failure.status, StatusCode::NOT_FOUND | StatusCode::GONE) =>
        {
            session.id = session.next_id().await?;
            session.reused = false;
            submit(&session, options, &prompt, &refs).await?
        }
        result => result?,
    };
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|header| header.to_str().ok());
    if !content_type.is_some_and(|mime| mime.starts_with("text/event-stream")) {
        return Err(Failure::upstream(
            "DeepSeek Web did not return an event stream",
        ));
    }
    let mut answer = Answer {
        _session: session,
        reader: SseReader::new(response).with_line_limit(4 * 1024 * 1024),
        id: format!("chatcmpl-deepseek-web-{}", uuid::Uuid::new_v4()),
        created: chrono::Utc::now().timestamp(),
        model: model.to_owned(),
        options,
        thinking: options.thinking,
        pending: VecDeque::new(),
        citations: Vec::new(),
        finished: None,
        received: 0,
        secrets: [token, access.to_owned()],
        tools,
    };
    if streaming {
        return Ok(answer.stream());
    }
    let (text, reasoning) = answer.collect_output().await?;
    Ok(answer.json(text, reasoning))
}

async fn submit(
    session: &Session,
    options: Model,
    prompt: &str,
    refs: &[String],
) -> Result<Response, Failure> {
    let challenge = envelope(
        request(
            &session.client,
            Method::POST,
            CHALLENGE,
            &session.authorization,
        )
        .json(&json!({"target_path":COMPLETION})),
    )
    .await
    .map_err(|failure| {
        if matches!(failure.status, StatusCode::NOT_FOUND | StatusCode::GONE) {
            Failure::upstream("DeepSeek Web challenge endpoint is unavailable")
        } else {
            failure
        }
    })?;
    let proof = pow::answer(&challenge["challenge"], COMPLETION)
        .await
        .map_err(Failure::upstream)?;
    let mut proof = header::HeaderValue::from_str(&proof)
        .map_err(|_| Failure::upstream("Could not encode DeepSeek Web proof-of-work"))?;
    proof.set_sensitive(true);
    send(request(&session.client,Method::POST,COMPLETION,&session.authorization)
        .header("X-Ds-Pow-Response",proof).json(&json!({
            "chat_session_id":session.id,"parent_message_id":null,"model_type":options.kind,"prompt":prompt,
            "ref_file_ids":refs,"thinking_enabled":options.thinking,"search_enabled":options.search,"preempt":false,
        }))).await
}

#[derive(Clone, Copy)]
struct Model {
    kind: &'static str,
    thinking: bool,
    search: bool,
    hide_citations: bool,
}

impl Model {
    fn new(model: &str, body: &Value) -> Self {
        let model = model.to_ascii_lowercase();
        Self {
            kind: if model.contains("pro") || model.contains("expert") {
                "expert"
            } else {
                "default"
            },
            thinking: ["r1", "think", "reason"]
                .iter()
                .any(|part| model.contains(*part))
                || body["thinking_enabled"] == true
                || body["thinking"] == true
                || truthy(&body["reasoning_effort"]),
            search: model.contains("search")
                || ["search_enabled", "search", "web_search"]
                    .iter()
                    .any(|flag| body[*flag] == true),
            hide_citations: model.contains("search-silent"),
        }
    }
}

fn prompt(body: &Value, account: &Value, tools: Option<&ClientTools>) -> Result<String, Failure> {
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| Failure::new(StatusCode::BAD_REQUEST, "DeepSeek Web requires messages"))?;
    let mut systems = Vec::new();
    let mut history = Vec::new();
    let mut call_names = HashMap::new();
    let mut saw_tools = false;
    if let Some(tools) = tools {
        systems.push(tools.manifest().to_owned());
    }
    for msg in messages {
        let text = match &msg["content"] {
            Value::String(text) => text.to_owned(),
            Value::Array(parts) => parts
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let mut text = js_trim(&text).to_owned();
        match msg["role"].as_str() {
            Some("system" | "developer") => {
                if !text.is_empty() {
                    systems.push(text)
                }
            }
            Some(role @ ("user" | "assistant")) => {
                for call in msg["tool_calls"].as_array().into_iter().flatten() {
                    let name = call["function"]["name"].as_str().unwrap_or_default();
                    if let (Some(id), Some(name)) =
                        (call["id"].as_str(), call["function"]["name"].as_str())
                    {
                        call_names.insert(id, name);
                    }
                    if tools.is_some() && role == "assistant" {
                        let args = &call["function"]["arguments"];
                        let args = if let Some(raw) = args.as_str().filter(|raw| !raw.is_empty()) {
                            raw.to_owned()
                        } else if args.is_null() {
                            "{}".into()
                        } else {
                            args.to_string()
                        };
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&format!(
                            "<tool>{{\"name\": {}, \"arguments\": {args}}}</tool>",
                            json!(name)
                        ));
                        saw_tools = true;
                    }
                }
                if !text.is_empty() {
                    let label = if role == "user" {
                        "User: "
                    } else {
                        "Assistant: "
                    };
                    history.push((label, text));
                }
            }
            Some("tool") if tools.is_some() || !text.is_empty() => {
                let name = msg["tool_call_id"]
                    .as_str()
                    .and_then(|id| call_names.get(id))
                    .copied()
                    .or_else(|| msg["name"].as_str())
                    .unwrap_or("tool");
                if tools.is_some() {
                    let text = if text.is_empty() {
                        "(no output)"
                    } else {
                        &text
                    };
                    history.push(("Tool result ", format!("({name}): {text}")));
                    saw_tools = true;
                } else {
                    history.push(("Tool result ", format!("({name}) {text}")));
                }
            }
            _ => {}
        }
    }
    if tools.is_some() {
        systems.extend(
            history
                .into_iter()
                .map(|(role, text)| format!("{role}{text}")),
        );
        if saw_tools {
            systems.push("Continue the task using the tool results above. Do NOT repeat tool calls that already succeeded; perform the next step or give the final answer.".into());
        }
    } else if history.len() > 1 {
        let requested = account["providerSpecificData"]["historyWindow"]
            .as_u64()
            .filter(|size| *size != 0)
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(20);
        systems.extend(
            history
                .drain(history.len().saturating_sub(requested)..)
                .map(|(role, text)| format!("{role}{text}")),
        );
    } else if let Some(("User: ", text)) = history.pop() {
        systems.push(text);
    }
    let prompt = IMAGE_MARKDOWN
        .replace_all(&systems.join("\n\n"), "")
        .into_owned();
    if js_trim(&prompt).is_empty() || prompt.len() > 4 * 1024 * 1024 {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "DeepSeek Web prompt is empty or too large",
        ));
    }
    Ok(prompt)
}

struct Piece {
    thinking: bool,
    text: String,
}

struct Answer {
    _session: Session,
    reader: SseReader,
    id: String,
    created: i64,
    model: String,
    options: Model,
    thinking: bool,
    pending: VecDeque<Piece>,
    citations: Vec<Value>,
    finished: Option<Instant>,
    received: usize,
    secrets: [String; 2],
    tools: Option<ClientTools>,
}

impl Answer {
    async fn next(&mut self) -> Result<Option<Piece>, Failure> {
        loop {
            if let Some(piece) = self.pending.pop_front() {
                return Ok(Some(piece));
            }
            let line = match self.reader.next(TIMEOUT, self.finished).await {
                ReadLine::Line(line, _) => line,
                ReadLine::End | ReadLine::Timeout if self.finished.is_some() => {
                    self._session.remember();
                    return Ok(None);
                }
                ReadLine::Timeout => {
                    return Err(Failure::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        "DeepSeek Web stream timed out",
                    ));
                }
                ReadLine::End | ReadLine::Disconnected => {
                    return Err(Failure::upstream(
                        "DeepSeek Web stream ended before completion",
                    ));
                }
            };
            self.received += line.len().saturating_add(1);
            if self.received > RESPONSE_LIMIT {
                return Err(Failure::upstream(
                    "DeepSeek Web stream exceeded its size limit",
                ));
            }
            let Some(payload) = line.strip_prefix("data:").map(js_trim) else {
                continue;
            };
            if payload == "[DONE]" {
                self._session.remember();
                return Ok(None);
            }
            let Ok(data) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            if truthy(&data["code"]) && data["code"] != "0" || truthy(&data["error"]) {
                return Err(Failure::upstream(
                    "DeepSeek Web returned a conversation error",
                ));
            }
            self.apply(&data);
        }
    }

    fn apply(&mut self, data: &Value) {
        let value = &data["v"];
        let path = data["p"].as_str().unwrap_or_default();
        if let Some(thinking) = value["response"]["thinking_enabled"].as_bool() {
            self.thinking = thinking;
        }
        if let Some(fragments) = value["response"]["fragments"].as_array() {
            for fragment in fragments {
                self.fragment(fragment);
            }
        }
        match path {
            "response/status" if value == "FINISHED" => {
                self.finished.get_or_insert_with(|| Instant::now() + DRAIN);
            }
            "response/search_results" => {
                if let Some(results) = value.as_array() {
                    if data["o"] != "BATCH" {
                        self.citations = results.clone();
                    } else {
                        for change in results {
                            if let Some(citation) = change["p"]
                                .as_str()
                                .and_then(|path| path.strip_suffix("/cite_index"))
                                .and_then(|index| index.parse::<usize>().ok())
                                .and_then(|index| self.citations.get_mut(index))
                            {
                                citation["cite_index"] = change["v"].clone();
                            }
                        }
                    }
                }
            }
            "response/search_status" | "response/status" => {}
            "response/fragments" => {
                if let Some(fragments) = value.as_array() {
                    for fragment in fragments {
                        self.fragment(fragment);
                    }
                } else {
                    self.fragment(value);
                }
            }
            "response" if value.is_array() => {
                for item in value.as_array().into_iter().flatten() {
                    if let Some(thinking) = item["v"]["thinking_enabled"].as_bool() {
                        self.thinking = thinking;
                    }
                    if let Some(fragments) = item["v"].as_array() {
                        for fragment in fragments {
                            self.fragment(fragment);
                        }
                    }
                }
            }
            _ => {
                if let Some(text) = value.as_str() {
                    if path.contains("/thinking") {
                        self.thinking = true;
                    }
                    self.push(text);
                }
            }
        }
    }

    fn fragment(&mut self, fragment: &Value) {
        match fragment["type"].as_str() {
            Some("THINK") => self.thinking = true,
            Some("ANSWER" | "RESPONSE") => self.thinking = false,
            _ => {}
        }
        if let Some(text) = fragment["content"].as_str() {
            self.push(text);
        }
    }

    fn push(&mut self, raw: &str) {
        let cleaned = raw.replace("FINISHED", "");
        let text = SEARCH_PREFIX.replace(&cleaned, "");
        let text = if self.options.hide_citations {
            CITATION.replace_all(&text, "").into_owned()
        } else if self.options.search {
            CITATION.replace_all(&text, "[$1]").into_owned()
        } else {
            text.into_owned()
        };
        if !text.is_empty() {
            self.pending.push_back(Piece {
                thinking: self.thinking,
                text,
            });
        }
    }

    fn redact(&self, text: &str) -> String {
        self.secrets
            .iter()
            .filter(|secret| secret.len() >= 8)
            .fold(text.to_owned(), |text, secret| {
                text.replace(secret, "[redacted]")
            })
    }

    fn citation_text(&self) -> String {
        if self.options.hide_citations {
            return String::new();
        }
        let mut citations: Vec<_> = self
            .citations
            .iter()
            .filter_map(|entry| {
                let index = entry["cite_index"].as_u64().filter(|index| *index > 0)?;
                let title = entry["title"].as_str()?;
                let url = entry["url"].as_str()?;
                Some((index, format!("[{index}]: [{title}]({url})")))
            })
            .collect();
        citations.sort_unstable_by_key(|entry| entry.0);
        if citations.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n{}",
                citations
                    .into_iter()
                    .map(|(_, citation)| citation)
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
    }

    async fn collect_output(&mut self) -> Result<(String, String), Failure> {
        let mut text = String::new();
        let mut reasoning = String::new();
        while let Some(piece) = self.next().await? {
            if piece.thinking {
                reasoning.push_str(&piece.text);
            } else {
                text.push_str(&piece.text);
            }
        }
        Ok((text, reasoning))
    }

    fn output(&self, text: &str, reasoning: &str) -> (String, String, Vec<Value>) {
        let text = self.redact(text);
        let reasoning = self.redact(reasoning);
        let citations = self.redact(&self.citation_text());
        if let Some(tools) = &self.tools {
            let (mut text, mut calls) = tools.parse(&text);
            for call in &mut calls {
                if let Some(args) = call["function"]["arguments"].as_str() {
                    call["function"]["arguments"] = json!(self.redact(args));
                }
            }
            text.push_str(&tools.redact(&citations));
            (text, tools.redact(&reasoning), calls)
        } else {
            (text + &citations, reasoning, Vec::new())
        }
    }

    fn json(&self, text: String, reasoning: String) -> Response {
        let (text, reasoning, calls) = self.output(&text, &reasoning);
        let mut message = json!({"role":"assistant","content":text});
        if !reasoning.is_empty() {
            message["reasoning_content"] = json!(reasoning);
        }
        let finish = if calls.is_empty() {
            "stop"
        } else {
            if text.is_empty() {
                message["content"] = Value::Null;
            }
            message["tool_calls"] = json!(calls);
            "tool_calls"
        };
        let completion = json!({"id":self.id,"object":"chat.completion","created":self.created,"model":self.model,
            "choices":[{"index":0,"message":message,"finish_reason":finish,"logprobs":null}],
            "usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}});
        response("application/json")
            .body(Body::from(completion.to_string()))
            .expect("DeepSeek JSON response")
            .into()
    }

    fn frame(&self, delta: Value, finish: Option<&str>) -> String {
        let frame = json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,
            "model":self.model,"system_fingerprint":null,
            "choices":[{"index":0,"delta":delta,"finish_reason":finish,"logprobs":null}]});
        format!("data: {frame}\n\n")
    }

    fn tool_frames(&self, text: &str, reasoning: &str) -> String {
        let (text, reasoning, mut calls) = self.output(text, reasoning);
        let mut wire = String::new();
        if !reasoning.is_empty() {
            wire.push_str(&self.frame(json!({"reasoning_content":reasoning}), None));
        }
        if !text.is_empty() {
            wire.push_str(&self.frame(json!({"content":text}), None));
        }
        let finish = if calls.is_empty() {
            "stop"
        } else {
            for (index, call) in calls.iter_mut().enumerate() {
                call["index"] = json!(index);
            }
            wire.push_str(&self.frame(json!({"tool_calls":calls}), None));
            "tool_calls"
        };
        wire.push_str(&self.frame(json!({}), Some(finish)));
        wire.push_str("data: [DONE]\n\n");
        wire
    }

    fn stream(self) -> Response {
        let wire = stream::unfold((self, Some(false)), |(mut answer, state)| async move {
            let (wire, next) = if !state? {
                (
                    answer.frame(json!({"role":"assistant","content":""}), None),
                    Some(true),
                )
            } else if answer.tools.is_some() {
                let wire = match answer.collect_output().await {
                    Ok((text, reasoning)) => answer.tool_frames(&text, &reasoning),
                    Err(error) => error_frame(error),
                };
                (wire, None)
            } else {
                match answer.next().await {
                    Ok(Some(piece)) => {
                        let field = if piece.thinking {
                            "reasoning_content"
                        } else {
                            "content"
                        };
                        (
                            answer.frame(json!({(field):answer.redact(&piece.text)}), None),
                            Some(true),
                        )
                    }
                    Ok(None) => {
                        let citations = answer.citation_text();
                        let mut wire = if citations.is_empty() {
                            String::new()
                        } else {
                            answer.frame(json!({"content":answer.redact(&citations)}), None)
                        };
                        wire.push_str(&answer.frame(json!({}), Some("stop")));
                        wire.push_str("data: [DONE]\n\n");
                        (wire, None)
                    }
                    Err(error) => (error_frame(error), None),
                }
            };
            Some((Ok::<_, Infallible>(Bytes::from(wire)), (answer, next)))
        });
        response("text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("x-accel-buffering", "no")
            .body(Body::wrap_stream(wire))
            .expect("DeepSeek event response")
            .into()
    }
}

fn error_frame(error: Failure) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"error":{"message":error.message,"type":"upstream_error"}})
    )
}

fn response(mime: &'static str) -> axum::http::response::Builder {
    axum::http::Response::builder()
        .url(Url::parse(&format!("{ORIGIN}{COMPLETION}")).expect("fixed DeepSeek Web URL"))
        .header(header::CONTENT_TYPE, mime)
}
