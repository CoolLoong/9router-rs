use std::collections::HashMap;
use std::io;
use std::sync::LazyLock;
use std::time::Duration;

use axum::body::Bytes;
use chrono::Utc;
use futures_util::stream;
use reqwest::{
    Body, Client, RequestBuilder, Response, ResponseBuilderExt, StatusCode, Url, header,
};
use serde_json::{Value, json};

use crate::chat_accounts;
use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_http::{Delivery, http_client, requires_protection};
use crate::provider_index::js_trim;
use crate::provider_oauth::{RefreshMode, refresh_account, refresh_due_for_provider};
use crate::sse_reader::{ReadLine, SseReader};
use crate::storage::Storage;

pub(super) const ENDPOINT: &str = "https://core-normal.trae.ai/api/remote/v1/chat_sessions";
const FIXTURE: &str = "http://127.0.0.1:52989/trae-fixture/api/remote/v1/chat_sessions";
const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 ",
    "(KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36",
);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_METADATA: usize = 256 * 1024;
const MAX_LINE: usize = 4 * 1024 * 1024;
const MAX_TEXT: usize = 16 * 1024 * 1024;

static TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
    let millis = std::env::var("TRAE_STREAM_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (1..=i32::MAX as u64).contains(value))
        .unwrap_or(300_000);
    Duration::from_millis(millis)
});
static DIRECT: LazyLock<Option<Client>> = LazyLock::new(|| {
    http_client(*TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .ok()
});

pub(super) struct Failure {
    pub status: StatusCode,
    pub message: String,
    event: bool,
}

impl Failure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            event: false,
        }
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn credentials() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "Trae credentials are unavailable or expired",
        )
    }

    fn reqwest(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::new(StatusCode::GATEWAY_TIMEOUT, "Trae upstream timed out")
        } else {
            Self::upstream("Could not reach Trae upstream")
        }
    }
}

pub(super) fn supports(account: &Value) -> bool {
    account["provider"] == "trae"
        && account["authType"] == "oauth"
        && (authorization(account).is_some() || token(account, "refreshToken").is_some())
}

pub(super) async fn protects_chat(storage: &Storage, settings: &Value) -> bool {
    requires_protection(settings, &Value::Null)
        || chat_accounts::active_connections(storage, "trae")
            .await
            .map_or(true, |accounts| {
                accounts
                    .iter()
                    .any(|account| requires_protection(settings, account))
            })
}

pub(super) async fn complete(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    body: &Value,
    streaming: bool,
) -> Result<Response, Failure> {
    let delivery = Delivery::select(storage, settings, account, ENDPOINT)
        .await
        .map_err(|_| Failure::upstream("Trae proxy is unavailable"))?;
    let client = DIRECT
        .as_ref()
        .and_then(|direct| delivery.client(direct, *TIMEOUT))
        .ok_or_else(|| Failure::upstream("Could not initialize the Trae client"))?;
    let url = Url::parse(if delivery.use_fixture() {
        FIXTURE
    } else {
        ENDPOINT
    })
    .map_err(|_| Failure::upstream("Invalid Trae endpoint"))?;
    let renewable = token(account, "refreshToken").is_some();
    let mut current = if renewable && authorization(account).is_none() {
        Some(rotate(storage, account, &delivery, RefreshMode::Rejected).await?)
    } else if renewable && refresh_due_for_provider("trae", account) {
        Some(rotate(storage, account, &delivery, RefreshMode::Proactive).await?)
    } else {
        None
    };

    let mut session = submit(
        &client,
        &url,
        current.as_ref().unwrap_or(account),
        body,
        model,
        streaming,
    )
    .await;
    if renewable && current.is_none() && session.as_ref().is_err_and(rejected) {
        let next = rotate(storage, account, &delivery, RefreshMode::Rejected).await?;
        session = submit(&client, &url, &next, body, model, streaming).await;
        current = Some(next);
    }
    let (session, message) = session?;

    let mut event_url = url.clone();
    event_url
        .path_segments_mut()
        .map_err(|_| Failure::upstream("Invalid Trae events endpoint"))?
        .push(&session)
        .push("events");
    event_url
        .query_pairs_mut()
        .append_pair("reply_to_message_id", &message);
    let mut events = subscribe(
        &client,
        &event_url,
        current.as_ref().unwrap_or(account),
        streaming,
    )
    .await;
    if renewable && current.is_none() && events.as_ref().is_err_and(rejected) {
        let next = rotate(storage, account, &delivery, RefreshMode::Rejected).await?;
        events = subscribe(&client, &event_url, &next, streaming).await;
        current = Some(next);
    }
    let events = events?;
    let secrets = [Some(account), current.as_ref()]
        .into_iter()
        .flatten()
        .flat_map(|account| {
            ["accessToken", "refreshToken"]
                .into_iter()
                .filter_map(|name| token(account, name))
        })
        .map(str::to_owned)
        .collect();
    let (reader, response) = (Events::new(events, secrets), Completion::new(model));
    if streaming {
        return Ok(stream_response(event_url, reader, response));
    }
    let mut reader = reader;
    while let Some(delta) = reader.next_delta().await? {
        drop(delta);
    }
    Ok(axum::http::Response::builder()
        .status(StatusCode::OK)
        .url(event_url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(response.json(reader)))
        .expect("Trae JSON response")
        .into())
}

fn rejected(error: &Failure) -> bool {
    matches!(
        error.status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    )
}

async fn rotate(
    storage: &Storage,
    account: &Value,
    delivery: &Delivery,
    mode: RefreshMode,
) -> Result<Value, Failure> {
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(|| Failure::upstream("Could not read Trae proxy settings"))?;
    ensure_route(storage, &settings, account, delivery).await?;
    let updated = refresh_account(storage, account, mode)
        .await
        .map_err(|error| match error {
            crate::provider_oauth::RefreshError::ReauthenticationRequired => Failure::credentials(),
            _ => Failure::upstream("Could not refresh Trae credentials"),
        })?;
    if updated["isActive"] == false
        || authorization(&updated).is_none()
        || mode == RefreshMode::Rejected && updated["accessToken"] == account["accessToken"]
    {
        return Err(Failure::credentials());
    }
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(|| Failure::upstream("Could not read Trae proxy settings"))?;
    ensure_route(storage, &settings, &updated, delivery).await?;
    Ok(updated)
}

async fn ensure_route(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    expected: &Delivery,
) -> Result<(), Failure> {
    if !matches!(Delivery::select(storage, settings, account, ENDPOINT).await, Ok(actual) if &actual == expected)
    {
        return Err(Failure::upstream(
            "Trae proxy changed during credential refresh",
        ));
    }
    Ok(())
}

fn token<'a>(account: &'a Value, name: &str) -> Option<&'a str> {
    account[name]
        .as_str()
        .map(js_trim)
        .filter(|value| !value.is_empty())
}

fn authorization(account: &Value) -> Option<header::HeaderValue> {
    let mut value =
        header::HeaderValue::from_str(&format!("Cloud-IDE-JWT {}", token(account, "accessToken")?))
            .ok()?;
    value.set_sensitive(true);
    Some(value)
}

fn headers(
    request: RequestBuilder,
    account: &Value,
    streaming: bool,
    authorization: header::HeaderValue,
) -> RequestBuilder {
    let details = &account["providerSpecificData"];
    request
        .header(header::AUTHORIZATION, authorization)
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-Trae-Client-Type", "web")
        .header(
            "X-Preferenced-Language",
            details["appLanguage"]
                .as_str()
                .filter(|value| !value.is_empty())
                .unwrap_or("en"),
        )
        .header(
            "x-user-region",
            details["userRegion"]
                .as_str()
                .filter(|value| !value.is_empty())
                .unwrap_or("US"),
        )
        .header(header::REFERER, "https://solo.trae.ai/")
        .header(header::USER_AGENT, USER_AGENT)
        .header(
            header::ACCEPT,
            if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
}

pub(super) fn preview_headers(
    request: RequestBuilder,
    account: &Value,
    streaming: bool,
) -> RequestBuilder {
    headers(
        request,
        account,
        streaming,
        header::HeaderValue::from_static("[redacted]"),
    )
}

async fn submit(
    client: &Client,
    url: &Url,
    account: &Value,
    body: &Value,
    model: &str,
    streaming: bool,
) -> Result<(String, String), Failure> {
    let outgoing = session_body(&account["providerSpecificData"], body, model);
    let authorization = authorization(account).ok_or_else(Failure::credentials)?;
    let mut answer = headers(client.post(url.clone()), account, streaming, authorization)
        .timeout(CONNECT_TIMEOUT)
        .json(&outgoing)
        .send()
        .await
        .map_err(Failure::reqwest)?;
    if !answer.status().is_success() {
        return Err(status_failure(
            answer.status(),
            "Trae could not create the chat session",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = answer.chunk().await.map_err(Failure::reqwest)? {
        if chunk.len() > MAX_METADATA - bytes.len() {
            return Err(Failure::upstream("Trae session response is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let data: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Failure::upstream("Invalid Trae session response"))?;
    if data["code"].as_f64() != Some(0.0) {
        return Err(Failure::upstream("Trae could not create the chat session"));
    }
    let response = &data["data"];
    let id = |field| {
        let value: &Value = &response[field];
        match value {
            Value::String(value) if !value.is_empty() => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        }
    };
    id("chat_session_id")
        .zip(id("message_id"))
        .ok_or_else(|| Failure::upstream("Trae session response is missing its identifiers"))
}

async fn subscribe(
    client: &Client,
    url: &Url,
    account: &Value,
    streaming: bool,
) -> Result<Response, Failure> {
    let authorization = authorization(account).ok_or_else(Failure::credentials)?;
    let response = headers(client.get(url.clone()), account, streaming, authorization)
        .send()
        .await
        .map_err(Failure::reqwest)?;
    if !response.status().is_success() {
        return Err(status_failure(
            response.status(),
            "Trae events stream failed",
        ));
    }
    Ok(response)
}

fn status_failure(status: StatusCode, message: &'static str) -> Failure {
    Failure::new(
        if matches!(
            status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
        ) {
            status
        } else {
            StatusCode::BAD_GATEWAY
        },
        format!("{message} (HTTP {})", status.as_u16()),
    )
}

fn session_body(details: &Value, body: &Value, model: &str) -> Value {
    let normalized = js_trim(model).to_ascii_lowercase();
    let (mode, strategy, model_name) = match normalized.as_str() {
        "work" | "auto-work" | "solo-work" => ("work", "auto", ""),
        "" | "auto" => ("code", "auto", ""),
        _ => ("code", "manual", model),
    };
    let parts = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|message| {
            let content = match &message["content"] {
                Value::String(value) => value.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .map(|part| {
                        if let Some(text) = part.as_str() {
                            text.to_owned()
                        } else if !part.is_object() || part["text"].is_null() {
                            String::new()
                        } else {
                            js_text(&part["text"])
                        }
                    })
                    .collect::<String>(),
                _ => String::new(),
            };
            match message["role"].as_str() {
                Some("system") => format!("[System]\n{content}"),
                Some("assistant") => format!("[Assistant]\n{content}"),
                _ => content,
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let metadata = |field: &str, fallback: &str| {
        details
            .get(field)
            .filter(|value| truthy(value))
            .cloned()
            .unwrap_or_else(|| json!(fallback))
    };
    let region = metadata("region", "US-East");
    let common = json!({
        "language":"en-us","app_language":metadata("appLanguage", "en"),"quality":"stable",
        "app_version":metadata("appVersion", "1.0.0.1229"),"web_id":metadata("webId", ""),
        "user_identity":metadata("userIdentity", "Free"),"is_freshman":"0","biz_user_id":metadata("bizUserId",""),
        "user_unique_id":metadata("userUniqueId",""),"scope":metadata("scope","marscode-us"),
        "tenant":metadata("tenant","marscode"),"region":region,
        "aiRegion":details.get("aiRegion").filter(|value| truthy(value)).unwrap_or(&region),
        "is_privacy_mode":0,"privacy_mode":"off","solo_chat_mode":mode,
    });
    json!({
        "mode":mode,"environment_id":"default",
        "initial_message":{
            "chat_session_id":"","content":[],
            "query":json!([{"type":"text","data":{"content":parts}}]).to_string(),
            "model_name":model_name,"agent_type":"solo_agent_remote",
            "model_selection_strategy":strategy,"common_params":common.to_string(),
        },
        "env":"remote","auto_create_project":false,"origin":"web",
    })
}

struct Events {
    reader: SseReader,
    deadline: tokio::time::Instant,
    event: String,
    order: Vec<String>,
    thoughts: HashMap<String, String>,
    bytes: usize,
    sent: usize,
    usage: Option<Value>,
    secrets: Vec<String>,
}

impl Events {
    fn new(upstream: Response, secrets: Vec<String>) -> Self {
        Self {
            reader: SseReader::new(upstream).with_line_limit(MAX_LINE),
            deadline: tokio::time::Instant::now() + *TIMEOUT,
            event: String::new(),
            order: Vec::new(),
            thoughts: HashMap::new(),
            bytes: 0,
            sent: 0,
            usage: None,
            secrets,
        }
    }

    async fn next_delta(&mut self) -> Result<Option<String>, Failure> {
        loop {
            let line = match self.reader.next(*TIMEOUT, Some(self.deadline)).await {
                ReadLine::Line(line, true) => line,
                ReadLine::Line(_, false) | ReadLine::End => return Ok(None),
                ReadLine::Timeout => {
                    return Err(Failure::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        "Trae events stream timed out",
                    ));
                }
                ReadLine::Disconnected => {
                    return Err(Failure::upstream("Trae events stream disconnected"));
                }
            };
            if let Some(kind) = line.strip_prefix("event:") {
                self.event = kind.trim().into();
            } else if line.trim_end_matches('\r').is_empty() {
                self.event.clear();
            } else if let Some(payload) = line.strip_prefix("data:") {
                let data: Value = serde_json::from_str(payload.trim()).unwrap_or(Value::Null);
                match self.event.as_str() {
                    "done" => return Ok(None),
                    "error" => {
                        let code = data
                            .get("code")
                            .filter(|value| truthy(value))
                            .map(js_text)
                            .unwrap_or_default();
                        let message = data
                            .get("message")
                            .filter(|value| truthy(value))
                            .map(js_text)
                            .unwrap_or_default();
                        let mut combined = format!("trae {code}: {message}");
                        for secret in &self.secrets {
                            combined = combined.replace(secret, "[redacted]");
                        }
                        return Err(Failure {
                            status: StatusCode::BAD_GATEWAY,
                            message: combined.chars().take(2000).collect(),
                            event: true,
                        });
                    }
                    "token_usage" => {
                        self.usage = Some(json!({
                            "prompt_tokens":usage_value(&data["prompt_tokens"]),
                            "completion_tokens":usage_value(&data["completion_tokens"]),
                            "total_tokens":usage_value(&data["total_tokens"]),
                        }))
                    }
                    "plan_item" if truthy(&data["id"]) => {
                        let delta = self.plan(&data)?;
                        if !delta.is_empty() {
                            return Ok(Some(delta));
                        }
                    }
                    _ => (),
                }
            }
        }
    }

    fn plan(&mut self, data: &Value) -> Result<String, Failure> {
        let id = js_text(&data["id"]);
        let next = data["thought"].as_str().unwrap_or_default();
        let previous = self.thoughts.entry(id.clone()).or_insert_with(|| {
            self.order.push(id);
            String::new()
        });
        if next.encode_utf16().count() >= previous.encode_utf16().count() {
            self.bytes = self.bytes - previous.len() + next.len();
            if self.bytes > MAX_TEXT || self.order.len() > 16_384 {
                return Err(Failure::upstream("Trae event content is too large"));
            }
            *previous = next.to_owned();
        }
        let full = self
            .order
            .iter()
            .flat_map(|id| self.thoughts[id].encode_utf16());
        let units: Vec<_> = full.skip(self.sent).collect();
        self.sent += units.len();
        Ok(String::from_utf16_lossy(&units))
    }

    fn content(&self) -> String {
        self.order
            .iter()
            .map(|id| self.thoughts[id].as_str())
            .collect()
    }
}

fn usage_value(value: &Value) -> Value {
    if truthy(value) {
        value.clone()
    } else {
        json!(0)
    }
}

struct Completion {
    id: String,
    created: i64,
    model: String,
}

impl Completion {
    fn new(model: &str) -> Self {
        let now = Utc::now();
        Self {
            id: format!("chatcmpl-trae-{}", now.timestamp_millis()),
            created: now.timestamp(),
            model: model.to_owned(),
        }
    }

    fn chunk(&self, choices: Value) -> Value {
        json!({
            "id":self.id,"object":"chat.completion.chunk","created":self.created,
            "model":self.model,"choices":choices,
        })
    }

    fn delta(&self, delta: Value, finish: Value) -> Value {
        self.chunk(json!([{"index":0,"delta":delta,"finish_reason":finish}]))
    }

    fn json(&self, reader: Events) -> String {
        let mut answer = json!({
            "id":self.id,"object":"chat.completion","created":self.created,"model":self.model,
            "choices":[{"index":0,"message":{"role":"assistant","content":reader.content()},"finish_reason":"stop"}],
        });
        if let Some(usage) = reader.usage {
            answer["usage"] = usage;
        }
        answer.to_string()
    }
}

fn stream_response(url: Url, reader: Events, response: Completion) -> Response {
    let stream = stream::unfold(
        (reader, response, Some(false)),
        |(mut reader, response, stage)| async move {
            let mut wire = String::new();
            let next = if !stage? {
                emit(
                    &mut wire,
                    response.delta(json!({"role":"assistant"}), Value::Null),
                );
                Some(true)
            } else {
                match reader.next_delta().await {
                    Ok(Some(content)) => {
                        emit(
                            &mut wire,
                            response.delta(json!({"content":content}), Value::Null),
                        );
                        Some(true)
                    }
                    Ok(None) => {
                        emit(&mut wire, response.delta(json!({}), json!("stop")));
                        if let Some(usage) = reader.usage.take() {
                            let mut chunk = response.chunk(json!([]));
                            chunk["usage"] = usage;
                            emit(&mut wire, chunk);
                        }
                        wire.push_str("data: [DONE]\n\n");
                        None
                    }
                    Err(error) if error.event => {
                        let mut chunk = response.chunk(json!([]));
                        chunk["error"] = json!({"message":error.message,"type":"api_error"});
                        emit(&mut wire, chunk);
                        wire.push_str("data: [DONE]\n\n");
                        None
                    }
                    Err(error) => {
                        return Some((
                            Err(io::Error::other(error.message)),
                            (reader, response, None),
                        ));
                    }
                }
            };
            Some((Ok(Bytes::from(wire)), (reader, response, next)))
        },
    );
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .url(url)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::wrap_stream(stream))
        .expect("Trae event response")
        .into()
}

fn emit(wire: &mut String, chunk: Value) {
    wire.push_str("data: ");
    wire.push_str(&chunk.to_string());
    wire.push_str("\n\n");
}
