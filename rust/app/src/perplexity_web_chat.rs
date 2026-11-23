use std::collections::{HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::http::{StatusCode, header};
use regex::Regex;
use reqwest::{Client, RequestBuilder, Response, ResponseBuilderExt};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::chat_accounts;
use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_http::{Delivery, http_client, requires_protection};
use crate::provider_index::js_trim;
use crate::providers::web_login::identity::perplexity_cookie;
use crate::sse_reader::{ReadLine, SseReader};
use crate::storage::Storage;

pub(super) const ENDPOINT: &str = "https://www.perplexity.ai/rest/sse/perplexity_ask";
const FIXTURE: &str = "http://127.0.0.1:52989/perplexity-web/rest/sse/perplexity_ask";
const TIMEOUT: Duration = Duration::from_secs(180);
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(15);
const STALL: Duration = Duration::from_secs(30);
const MAX_EVENT: usize = 4 * 1024 * 1024;
const MAX_TEXT: usize = 16 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(3600);
const SESSION_LIMIT: usize = 200;

static CLIENT: LazyLock<Option<Client>> = LazyLock::new(|| http_client(TIMEOUT).build().ok());
static CLEAN: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"<[?]xml[^?]*[?]>",
        r"\[[0-9]+\]",
        r"(?s)<grok:[^>]*>.*?</grok:[^>]*>",
        r"<grok:[^>]*/>",
        r"(?i)</?response\b[^>]*>",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("Perplexity answer pattern"))
    .collect()
});
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" {2,}").expect("space pattern"));
static NEWLINES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("newline pattern"));

struct Session {
    key: [u8; 32],
    backend: String,
    expires: Instant,
}

static SESSIONS: Mutex<VecDeque<Session>> = Mutex::new(VecDeque::new());

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

    fn transport() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "Perplexity outbound connection is unavailable",
        )
    }
}

pub(super) fn supports(account: &Value) -> bool {
    account["provider"] == "perplexity-web"
        && (access(account).is_some()
            || account["apiKey"]
                .as_str()
                .and_then(perplexity_cookie)
                .is_some())
}

pub(super) fn preview_auth_header(account: &Value) -> Option<&'static str> {
    supports(account).then(|| {
        if access(account).is_some() {
            "Authorization"
        } else {
            "Cookie"
        }
    })
}

pub(super) async fn protects_chat(storage: &Storage, settings: &Value) -> bool {
    requires_protection(settings, &Value::Null)
        || chat_accounts::active_connections(storage, "perplexity-web")
            .await
            .map_or(true, |accounts| {
                accounts
                    .iter()
                    .any(|account| requires_protection(settings, account))
            })
}

pub(crate) async fn validate_cookie(
    storage: &Storage,
    settings: &Value,
    input: &Value,
) -> Result<(), &'static str> {
    const INVALID: &str =
        "Invalid session cookie — re-paste __Secure-next-auth.session-token from perplexity.ai";
    const UNAVAILABLE: &str = "fetch failed";
    let cookie = input.as_str().and_then(perplexity_cookie).ok_or(INVALID)?;
    let delivery = Delivery::select(storage, settings, &Value::Null, ENDPOINT)
        .await
        .map_err(|_| UNAVAILABLE)?;
    let client = delivery
        .client(CLIENT.as_ref().ok_or(UNAVAILABLE)?, VALIDATE_TIMEOUT)
        .ok_or(UNAVAILABLE)?;
    let endpoint = if delivery.use_fixture() {
        FIXTURE
    } else {
        ENDPOINT
    };
    let response = ask(&client, endpoint, "ping", "concise", "pplx_pro", None)
        .header(header::COOKIE, cookie)
        .timeout(VALIDATE_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                "The operation was aborted due to timeout"
            } else {
                UNAVAILABLE
            }
        })?;
    match response.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(INVALID),
        status
            if status.is_redirection() || status == StatusCode::PROXY_AUTHENTICATION_REQUIRED =>
        {
            Err(UNAVAILABLE)
        }
        _ => Ok(()),
    }
}

pub(super) async fn complete(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    caller: Option<&str>,
    model: &str,
    body: &Value,
    stream: bool,
) -> Result<Response, Failure> {
    let parsed = Conversation::new(body)
        .ok_or_else(|| Failure::new(StatusCode::BAD_REQUEST, "Missing or empty messages array"))?;
    let route = Delivery::select(storage, settings, account, ENDPOINT)
        .await
        .map_err(|_| Failure::transport())?;
    let client = route
        .client(CLIENT.as_ref().ok_or_else(Failure::transport)?, TIMEOUT)
        .ok_or_else(Failure::transport)?;
    let scope = session_scope(storage, account, caller, &route);
    let continuation = parsed.last_backend(scope);
    let query = parsed.query(body, continuation.as_deref());
    if query.trim().is_empty() {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "Empty query after processing",
        ));
    }
    let (mode, preference) = model_preference(model, body);
    let endpoint = if route.use_fixture() {
        FIXTURE
    } else {
        ENDPOINT
    };
    let request = ask(
        &client,
        endpoint,
        &query,
        mode,
        preference,
        continuation.as_deref(),
    );
    let (request, secret) = if let Some(token) = access(account) {
        (request.bearer_auth(token), token)
    } else if let Some((input, cookie)) = account["apiKey"]
        .as_str()
        .and_then(|input| Some((input, perplexity_cookie(input)?)))
    {
        (
            request.header(header::COOKIE, cookie),
            input
                .strip_prefix("__Secure-next-auth.session-token=")
                .unwrap_or(input),
        )
    } else {
        return Err(Failure::new(
            StatusCode::UNAUTHORIZED,
            "Perplexity session credential is unavailable",
        ));
    };
    let upstream = request.send().await.map_err(|error| {
        if error.is_timeout() {
            Failure::new(StatusCode::GATEWAY_TIMEOUT, "Perplexity upstream timed out")
        } else {
            Failure::transport()
        }
    })?;
    let status = upstream.status();
    if !status.is_success() {
        let (status, message) = match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => (
                status,
                "Perplexity auth failed — session cookie may be expired".into(),
            ),
            StatusCode::TOO_MANY_REQUESTS => (
                status,
                "Perplexity rate limited. Wait a moment and retry.".into(),
            ),
            status if status.is_redirection() => (
                StatusCode::BAD_GATEWAY,
                "Perplexity upstream redirected the request".into(),
            ),
            status => (
                status,
                format!("Perplexity returned HTTP {}", status.as_u16()),
            ),
        };
        return Err(Failure::new(status, message));
    }
    let answer = Answer::new(upstream, model, secret, parsed, scope);
    if stream {
        return Ok(answer.stream());
    }
    answer.json().await
}

fn ask(
    client: &Client,
    endpoint: &str,
    query: &str,
    mode: &str,
    preference: &str,
    continuation: Option<&str>,
) -> RequestBuilder {
    let timezone = jiff::tz::TimeZone::system();
    client
        .post(endpoint)
        .header(header::ACCEPT, "text/event-stream")
        .header(header::ORIGIN, "https://www.perplexity.ai")
        .header(header::REFERER, "https://www.perplexity.ai/")
        .header(header::USER_AGENT, "9Router")
        .header("X-App-ApiClient", "default")
        .header("X-App-ApiVersion", "2.18")
        .json(&json!({
            "query_str": query,
            "params": {
                "query_str":query,"search_focus":"internet","mode":mode,"model_preference":preference,
                "sources":["web"],"attachments":[],"frontend_uuid":Uuid::new_v4().to_string(),
                "frontend_context_uuid":Uuid::new_v4().to_string(),"version":"2.18",
                "language":"en-US","timezone":timezone.iana_name().unwrap_or("UTC"),
                "search_recency_filter":null,"is_incognito":true,"use_schematized_api":true,
                "last_backend_uuid":continuation,
            },
        }))
}

fn access(account: &Value) -> Option<&str> {
    account["accessToken"]
        .as_str()
        .filter(|token| !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic()))
}

fn model_preference<'a>(model: &'a str, body: &Value) -> (&'static str, &'a str) {
    let thinking = body["thinking"] == true
        || (!body["reasoning_effort"].is_null() && body["reasoning_effort"] != "none");
    match (model, thinking) {
        ("pplx-auto", _) => ("concise", "pplx_pro"),
        ("pplx-sonar", _) => ("copilot", "experimental"),
        ("pplx-gpt", true) => ("copilot", "gpt54_thinking"),
        ("pplx-gpt", false) => ("copilot", "gpt54"),
        ("pplx-gemini", _) => ("copilot", "gemini31pro_high"),
        ("pplx-sonnet", true) => ("copilot", "claude46sonnetthinking"),
        ("pplx-sonnet", false) => ("copilot", "claude46sonnet"),
        ("pplx-opus", true) => ("copilot", "claude46opusthinking"),
        ("pplx-opus", false) => ("copilot", "claude46opus"),
        ("pplx-nemotron", _) => ("copilot", "nv_nemotron_3_super"),
        _ => ("copilot", model),
    }
}

struct Conversation {
    system: String,
    history: Vec<Value>,
    current: String,
}

impl Conversation {
    fn new(body: &Value) -> Option<Self> {
        let messages = body["messages"]
            .as_array()
            .filter(|messages| !messages.is_empty())?;
        let mut history = Vec::new();
        let mut system = String::new();
        for message in messages {
            let role = message["role"]
                .as_str()
                .filter(|role| !role.is_empty())
                .unwrap_or("user");
            let content = message["content"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    message["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|block| block["type"] == "text")
                        .map(|block| {
                            if truthy(&block["text"]) {
                                js_text(&block["text"])
                            } else {
                                String::new()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                });
            if content.trim().is_empty() {
                continue;
            }
            match role {
                "developer" | "system" => {
                    system.push_str(&content);
                    system.push('\n');
                }
                "assistant" | "user" => history.push(json!({"role":role,"content":content})),
                _ => (),
            }
        }
        let current = if history
            .last()
            .is_some_and(|message| message["role"] == "user")
        {
            history
                .pop()
                .and_then(|message| message["content"].as_str().map(str::to_owned))
                .unwrap_or_default()
        } else {
            String::new()
        };
        Some(Self {
            system,
            history,
            current,
        })
    }

    fn query(&self, body: &Value, continuation: Option<&str>) -> String {
        if continuation.is_some() {
            return self.current.clone();
        }
        let mut instructions = Vec::new();
        if !self.system.trim().is_empty() {
            instructions.push(self.system.trim().to_owned());
        }
        if let Some(tools) = body["tools"].as_array().filter(|tools| !tools.is_empty()) {
            let mut hint = String::from("Available tools (reference only, cannot invoke):");
            for tool in tools {
                let function = tool
                    .get("function")
                    .filter(|value| value.is_object())
                    .unwrap_or(tool);
                let name = function["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .unwrap_or("unnamed");
                let description = function["description"]
                    .as_str()
                    .unwrap_or_default()
                    .split('\n')
                    .next()
                    .unwrap_or_default();
                let description = String::from_utf16_lossy(
                    &description.encode_utf16().take(200).collect::<Vec<_>>(),
                );
                hint.push_str(&format!("\n- {name}: {description}"));
            }
            instructions.push(hint);
        }
        instructions.push(
            "You have built-in web search. Answer questions directly using search results.".into(),
        );
        let mut query = Map::from_iter([("instructions".to_owned(), json!(instructions))]);
        if !self.history.is_empty() {
            query.insert("history".into(), json!(self.history));
        }
        if !self.current.is_empty() || self.history.is_empty() {
            query.insert("query".into(), json!(self.current));
        }
        let text = Value::Object(query).to_string();
        if text.encode_utf16().count() <= 96_000 {
            return text;
        }
        let units: Vec<_> = text.encode_utf16().collect();
        String::from_utf16_lossy(&units[units.len() - 96_000..])
    }

    fn last_backend(&self, scope: [u8; 32]) -> Option<String> {
        if self.history.is_empty() {
            return None;
        }
        let key = history_key(scope, &self.history);
        let mut cache = SESSIONS.lock().unwrap_or_else(|error| error.into_inner());
        cache.retain(|session| session.expires > Instant::now());
        cache
            .iter()
            .find(|session| session.key == key)
            .map(|session| session.backend.clone())
    }

    fn remember(&mut self, scope: [u8; 32], content: &str, backend: String) {
        if backend.is_empty() {
            return;
        }
        self.history
            .push(json!({"role":"user","content":self.current}));
        self.history
            .push(json!({"role":"assistant","content":content}));
        let key = history_key(scope, &self.history);
        let mut cache = SESSIONS.lock().unwrap_or_else(|error| error.into_inner());
        cache.retain(|session| session.key != key && session.expires > Instant::now());
        if cache.len() >= SESSION_LIMIT {
            cache.pop_front();
        }
        cache.push_back(Session {
            key,
            backend,
            expires: Instant::now() + TTL,
        });
    }
}

fn history_key(scope: [u8; 32], history: &[Value]) -> [u8; 32] {
    Sha256::new()
        .chain_update(scope)
        .chain_update(serde_json::to_vec(history).expect("Perplexity message history serializes"))
        .finalize()
        .into()
}

fn session_scope(
    storage: &Storage,
    account: &Value,
    caller: Option<&str>,
    route: &Delivery,
) -> [u8; 32] {
    let route = match route {
        Delivery::Direct => "direct",
        Delivery::Bypass => "bypass",
        Delivery::Tunnel(url) => url.as_str(),
    };
    let input = json!([
        storage.root().to_string_lossy(),
        account["id"],
        account["apiKey"],
        account["accessToken"],
        caller,
        route
    ]);
    Sha256::digest(input.to_string().as_bytes()).into()
}

fn clean(text: &str, strip: bool) -> String {
    let mut text = text.to_owned();
    for pattern in CLEAN.iter() {
        text = pattern.replace_all(&text, "").into_owned();
    }
    if strip {
        text = SPACES.replace_all(&text, " ").into_owned();
        text = NEWLINES.replace_all(&text, "\n\n").into_owned();
        return js_trim(&text).to_owned();
    }
    text
}

enum Piece {
    Text(String),
    Thought(String),
}

struct Answer {
    source: SseReader,
    endpoint: reqwest::Url,
    query: Conversation,
    scope: [u8; 32],
    id: String,
    model: String,
    secret: String,
    created: i64,
    seen: HashSet<String>,
    seen_bytes: usize,
    backend: String,
    content: String,
    seen_length: usize,
    done: bool,
}

impl Answer {
    fn new(
        source: Response,
        model: &str,
        secret: &str,
        query: Conversation,
        scope: [u8; 32],
    ) -> Self {
        Self {
            endpoint: source.url().clone(),
            source: SseReader::new(source).with_line_limit(MAX_EVENT),
            query,
            scope,
            id: format!("chatcmpl-pplx-{}", &Uuid::new_v4().to_string()[..12]),
            model: model.into(),
            secret: secret.into(),
            created: chrono::Utc::now().timestamp(),
            seen: HashSet::new(),
            seen_bytes: 0,
            backend: String::new(),
            content: String::new(),
            seen_length: 0,
            done: false,
        }
    }

    async fn event(&mut self) -> Result<Option<Value>, String> {
        let mut lines = String::new();
        let mut saw_data = false;
        loop {
            match self.source.next(STALL, None).await {
                ReadLine::Line(line, _) => {
                    let line = line.trim_end_matches('\r');
                    if line == "event: end_of_stream" {
                        return Ok(None);
                    }
                    if let Some(data) = line.strip_prefix("data:") {
                        if lines.len() + data.len() + 1 > MAX_EVENT {
                            return Err("Perplexity response event is too large".into());
                        }
                        if saw_data {
                            lines.push('\n');
                        }
                        saw_data = true;
                        lines.push_str(data.trim_start());
                    } else if line.is_empty() && saw_data {
                        if matches!(lines.trim(), "" | "[DONE]") {
                            return Ok(None);
                        }
                        if let Ok(value) = serde_json::from_str(&lines) {
                            return Ok(Some(value));
                        }
                        lines.clear();
                        saw_data = false;
                    }
                }
                ReadLine::End => return Ok(serde_json::from_str(&lines).ok()),
                ReadLine::Timeout => return Err("Perplexity response timed out".into()),
                ReadLine::Disconnected => return Err("Perplexity response was interrupted".into()),
            }
        }
    }

    fn parts(&mut self, event: Value) -> Result<Vec<Piece>, String> {
        if truthy(&event["error_code"]) || truthy(&event["error_message"]) {
            return Err(if truthy(&event["error_message"]) {
                js_text(&event["error_message"])
            } else {
                format!("Perplexity error: {}", js_text(&event["error_code"]))
            }
            .replace(&self.secret, "[redacted]"));
        }
        if let Some(backend) = event["backend_uuid"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 512)
        {
            self.backend = backend.to_owned();
        }
        let blocks = event["blocks"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut parts = Vec::new();
        for block in blocks {
            let usage = block["intended_usage"].as_str().unwrap_or_default();
            if usage == "pro_search_steps" {
                for step in block["plan_block"]["steps"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    match step["step_type"].as_str() {
                        Some("SEARCH_WEB") => {
                            for query in step["search_web_content"]["queries"]
                                .as_array()
                                .into_iter()
                                .flatten()
                            {
                                self.thought(&query["query"], "Searching: ", &mut parts);
                            }
                        }
                        Some("READ_RESULTS") => {
                            for url in step["read_results_content"]["urls"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .take(3)
                            {
                                self.thought(url, "Reading: ", &mut parts);
                            }
                        }
                        _ => (),
                    }
                }
            } else if usage == "plan" {
                for goal in block["plan_block"]["goals"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    self.thought(&goal["description"], "", &mut parts);
                }
            }
            if !usage.contains("markdown") {
                continue;
            }
            let Some(chunks) = block["markdown_block"]["chunks"]
                .as_array()
                .filter(|chunks| !chunks.is_empty())
            else {
                continue;
            };
            let text: String = chunks.iter().filter_map(Value::as_str).collect();
            if block["markdown_block"]["progress"] == "DONE" {
                self.content = text;
            } else {
                self.content.push_str(&text);
                self.delta(&mut parts);
            }
        }
        if blocks.is_empty()
            && let Some(text) = event["text"].as_str()
            && text.trim().encode_utf16().count() > self.seen_length
        {
            self.content = text.trim().to_owned();
            self.delta(&mut parts);
        }
        if self.content.len() > MAX_TEXT {
            return Err("Perplexity response text is too large".into());
        }
        self.done = truthy(&event["final"]) || event["status"] == "COMPLETED";
        Ok(parts)
    }

    fn thought(&mut self, value: &Value, prefix: &str, output: &mut Vec<Piece>) {
        let Some(text) = value.as_str().filter(|text| !text.is_empty()) else {
            return;
        };
        if self.seen.len() < 4096
            && self.seen_bytes + text.len() <= MAX_EVENT
            && self.seen.insert(text.into())
        {
            self.seen_bytes += text.len();
            let text = text.replace(&self.secret, "[redacted]");
            output.push(Piece::Thought(format!("{prefix}{text}")));
        }
    }

    fn delta(&mut self, output: &mut Vec<Piece>) {
        let utf16: Vec<_> = self.content.encode_utf16().collect();
        if utf16.len() > self.seen_length {
            let part = String::from_utf16_lossy(&utf16[self.seen_length..])
                .replace(&self.secret, "[redacted]");
            self.seen_length = utf16.len();
            let cleaned = clean(&part, false);
            if !cleaned.is_empty() {
                output.push(Piece::Text(cleaned));
            }
        }
    }

    fn completed(&mut self) -> String {
        let content = clean(&self.content, true).replace(&self.secret, "[redacted]");
        self.query
            .remember(self.scope, &content, std::mem::take(&mut self.backend));
        content
    }

    fn chunk(&self, delta: Value, finish: Value) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"system_fingerprint":null,
            "choices":[{"index":0,"delta":delta,"finish_reason":finish,"logprobs":null}]})
    }

    fn stream(self) -> Response {
        let endpoint = self.endpoint.clone();
        let stream = futures_util::stream::unfold(
            (self, false, false),
            |(mut answer, ready, mut ended)| async move {
                if ended {
                    return None;
                }
                let mut output = String::new();
                if !ready {
                    emit(
                        &mut output,
                        answer.chunk(json!({"role":"assistant"}), Value::Null),
                    );
                    return Some((
                        Ok::<_, std::convert::Infallible>(Bytes::from(output)),
                        (answer, true, false),
                    ));
                }
                loop {
                    let next = if answer.done {
                        Ok(None)
                    } else {
                        answer.event().await
                    };
                    match next {
                        Ok(Some(event)) => match answer.parts(event) {
                            Ok(parts) => {
                                for part in parts {
                                    let delta = match part {
                                        Piece::Text(text) => json!({"content":text}),
                                        Piece::Thought(thought) => {
                                            json!({"reasoning_content":format!("{thought}\n")})
                                        }
                                    };
                                    emit(&mut output, answer.chunk(delta, Value::Null));
                                }
                            }
                            Err(message) => {
                                emit(
                                    &mut output,
                                    answer.chunk(
                                        json!({"content":format!("[Error: {message}]")}),
                                        Value::Null,
                                    ),
                                );
                                ended = true;
                            }
                        },
                        Ok(None) => {
                            answer.completed();
                            ended = true;
                        }
                        Err(message) => {
                            emit(
                                &mut output,
                                answer.chunk(
                                    json!({"content":format!("[Stream error: {message}]")}),
                                    Value::Null,
                                ),
                            );
                            ended = true;
                        }
                    }
                    if ended {
                        emit(&mut output, answer.chunk(json!({}), json!("stop")));
                        output.push_str("data: [DONE]\n\n");
                    }
                    if !output.is_empty() {
                        return Some((Ok(Bytes::from(output)), (answer, true, ended)));
                    }
                }
            },
        );
        axum::http::Response::builder()
            .url(endpoint)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("x-accel-buffering", "no")
            .body(reqwest::Body::wrap_stream(stream))
            .expect("Perplexity stream response")
            .into()
    }

    async fn json(mut self) -> Result<Response, Failure> {
        let mut reasoning = Vec::new();
        while !self.done {
            let Some(event) = self
                .event()
                .await
                .map_err(|message| Failure::new(StatusCode::BAD_GATEWAY, message))?
            else {
                break;
            };
            let parts = self
                .parts(event)
                .map_err(|message| Failure::new(StatusCode::BAD_GATEWAY, message))?;
            for part in parts {
                if let Piece::Thought(text) = part {
                    reasoning.push(text);
                }
            }
        }
        let content = self.completed();
        let rates = (
            self.query.current.encode_utf16().count().div_ceil(4),
            content.encode_utf16().count().div_ceil(4),
        );
        let mut reply = json!({"role":"assistant","content":content});
        if !reasoning.is_empty() {
            reply["reasoning_content"] = json!(reasoning.join("\n"));
        }
        let response = json!({
            "id":self.id,"object":"chat.completion","created":self.created,"model":self.model,"system_fingerprint":null,
            "choices":[{"index":0,"message":reply,"finish_reason":"stop","logprobs":null}],
            "usage":{"prompt_tokens":rates.0,"completion_tokens":rates.1,"total_tokens":rates.0+rates.1},
        });
        Ok(axum::http::Response::builder()
            .url(self.endpoint)
            .header(header::CONTENT_TYPE, "application/json")
            .body(response.to_string())
            .expect("Perplexity completion response")
            .into())
    }
}

fn emit(output: &mut String, value: Value) {
    output.push_str("data: ");
    output.push_str(&value.to_string());
    output.push_str("\n\n");
}
