use std::convert::Infallible;
use std::time::Duration;

use axum::body::Bytes;
use futures_util::stream;
use reqwest::{Body, Method, Response, ResponseBuilderExt, StatusCode, Url, header};
use serde_json::{Value, json};

use crate::chat_input::truthy;
use crate::chat_output::js_text;
use crate::provider_http::{Delivery, http_client};
use crate::provider_index::js_trim;
use crate::sse_reader::{ReadLine, SseReader};
use crate::storage::Storage;

#[path = "grok_web_tools.rs"]
mod tools;

use tools::ToolBridge;

pub(super) const CONVERSATIONS: &str = "https://grok.com/rest/app-chat/conversations/new";
const ASSET_HOST: &str = "assets.grok.com";
const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_LINE: usize = 4 * 1024 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;

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

    fn upstream(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn credentials() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "Missing or invalid Grok Web SSO cookie",
        )
    }
}

pub(super) fn supports(account: &Value) -> bool {
    Session::read(account).is_some()
}

struct Session {
    sso: String,
    write: Option<String>,
}

impl Session {
    fn read(account: &Value) -> Option<Self> {
        let raw = account["apiKey"].as_str().map(js_trim)?;
        if account["provider"] != "grok-web"
            || !matches!(
                account["authType"].as_str(),
                Some("cookie" | "apikey" | "api_key")
            )
            || raw.len() > 128 * 1024
        {
            return None;
        }
        let raw = raw
            .get(7..)
            .filter(|_| {
                raw[..7].eq_ignore_ascii_case("cookie:") || raw[..7].eq_ignore_ascii_case("bearer ")
            })
            .map_or(raw, js_trim);
        let mut sso = None;
        let mut write = None;
        let mut remember = |name: &str, value: &str| {
            if valid_cookie(value) {
                match name {
                    "sso" => sso = Some(value.to_owned()),
                    "sso-rw" => write = Some(value.to_owned()),
                    _ => (),
                }
            }
        };
        if raw.starts_with(['[', '{']) {
            let exported: Value = serde_json::from_str(raw).ok()?;
            let rows = exported
                .as_array()
                .or_else(|| exported["cookies"].as_array())?;
            for row in rows {
                if row["domain"].as_str().is_some_and(|domain| {
                    !matches!(domain.trim_start_matches('.'), "grok.com" | ASSET_HOST)
                }) {
                    continue;
                }
                if let (Some(name), Some(value)) = (row["name"].as_str(), row["value"].as_str()) {
                    remember(name, value);
                }
            }
        } else if !raw.contains(';') && !raw.trim_end_matches('=').contains('=') {
            remember("sso", raw);
        } else {
            for (name, value) in raw.split(';').filter_map(|pair| pair.split_once('=')) {
                remember(js_trim(name), js_trim(value));
            }
        }
        Some(Self { sso: sso?, write })
    }

    fn header(&self, write: bool) -> Result<header::HeaderValue, Failure> {
        let mut raw = format!("sso={}", self.sso);
        if write && let Some(write) = &self.write {
            raw.push_str("; sso-rw=");
            raw.push_str(write);
        }
        let mut value = header::HeaderValue::from_str(&raw).map_err(|_| Failure::credentials())?;
        value.set_sensitive(true);
        Ok(value)
    }

    fn redact(&self, value: &str) -> String {
        [&self.sso]
            .into_iter()
            .chain(&self.write)
            .filter(|secret| secret.len() >= 8)
            .fold(value.to_owned(), |value, secret| {
                value.replace(secret, "[redacted]")
            })
    }
}

fn valid_cookie(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 8192
        && value.bytes().all(
            |byte| matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e),
        )
}

pub(super) async fn post_conversation(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    payload: &Value,
    timeout: Duration,
) -> Result<Response, Failure> {
    let url = Url::parse(CONVERSATIONS).expect("fixed Grok Web endpoint");
    let session = Session::read(account).ok_or_else(Failure::credentials)?;
    send(
        storage,
        settings,
        account,
        &session,
        url,
        Some(payload),
        timeout,
    )
    .await
}

pub(super) async fn get_asset(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    asset: &Url,
    timeout: Duration,
) -> Result<Response, Failure> {
    if asset.scheme() != "https"
        || asset.host_str() != Some(ASSET_HOST)
        || asset.port().is_some()
        || !asset.username().is_empty()
        || asset.password().is_some()
        || asset.fragment().is_some()
    {
        return Err(Failure::upstream(
            "Grok Web returned an unsupported asset URL",
        ));
    }
    let session = Session::read(account).ok_or_else(Failure::credentials)?;
    send(
        storage,
        settings,
        account,
        &session,
        asset.clone(),
        None,
        timeout,
    )
    .await
}

async fn send(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    session: &Session,
    url: Url,
    payload: Option<&Value>,
    timeout: Duration,
) -> Result<Response, Failure> {
    let delivery = Delivery::select(storage, settings, account, url.as_str())
        .await
        .map_err(|_| Failure::upstream("Grok Web cannot use the configured route"))?;
    let direct = http_client(timeout)
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| Failure::upstream("Could not initialize the Grok Web client"))?;
    let client = delivery
        .client(&direct, timeout)
        .ok_or_else(|| Failure::upstream("Could not initialize the Grok Web route"))?;
    let mut request = client
        .request(
            if payload.is_some() {
                Method::POST
            } else {
                Method::GET
            },
            url,
        )
        .header(header::USER_AGENT, "9Router")
        .header(header::ACCEPT, "*/*")
        .header(header::COOKIE, session.header(payload.is_some())?);
    if let Some(payload) = payload {
        request = request.json(payload);
    }
    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            Failure::new(StatusCode::GATEWAY_TIMEOUT, "Grok Web upstream timed out")
        } else {
            Failure::upstream("Could not reach Grok Web")
        }
    })?;
    if response.headers().contains_key("cf-mitigated") {
        return Err(Failure::upstream("Grok Web requires verification"));
    }
    let status = response.status();
    if status.is_redirection() {
        return Err(Failure::upstream("Grok Web returned a redirect"));
    }
    if !status.is_success() {
        let message = match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                "Grok Web did not accept the SSO session"
            }
            StatusCode::TOO_MANY_REQUESTS => "Grok Web rate limited the request",
            _ => "Grok Web rejected the request",
        };
        return Err(Failure::new(status, message));
    }
    if response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|mime| {
            mime.split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/html"))
        })
    {
        return Err(Failure::upstream(
            "Grok Web did not return the requested data",
        ));
    }
    Ok(response)
}

pub(super) async fn complete(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    body: &Value,
    streaming: bool,
) -> Result<Response, Failure> {
    let (mode, thinking) = match model.rsplit('/').next().unwrap_or_default() {
        "grok-image" | "grok-video" => {
            return Err(Failure::new(
                StatusCode::BAD_REQUEST,
                "Grok Web media models require a media route",
            ));
        }
        "grok-3-mini" | "grok-4-mini" | "grok-4.1-mini" => {
            ("MODEL_MODE_GROK_4_1_MINI_THINKING", true)
        }
        "grok-3-thinking" | "grok-4-thinking" | "grok-4.1-thinking" => {
            ("MODEL_MODE_GROK_4_1_THINKING", true)
        }
        "grok-4-heavy" | "grok-4.1-expert" => ("MODEL_MODE_EXPERT", true),
        "grok-4.2" | "grok-4.20" | "grok-4.20-beta" => ("MODEL_MODE_GROK_420", false),
        _ => ("MODEL_MODE_FAST", false),
    };
    let (message, tools) = prompt(body)?;
    let payload = json!({
        "temporary":true,"modelName":"grok-4-1-thinking-1129","modelMode":mode,"message":message,
        "fileAttachments":[],"imageAttachments":[],"disableSearch":false,"enableImageGeneration":false,
        "returnImageBytes":false,"returnRawGrokInXaiRequest":false,"enableImageStreaming":false,
        "imageGenerationCount":0,"forceConcise":false,"toolOverrides":{},"enableSideBySide":true,
        "sendFinalMetadata":true,"isReasoning":false,"disableTextFollowUps":false,"disableMemory":true,
        "forceSideBySide":false,"isAsyncChat":false,"disableSelfHarmShortCircuit":false,
    });
    let session = Session::read(account).ok_or_else(Failure::credentials)?;
    let response = post_conversation(storage, settings, account, &payload, TIMEOUT).await?;
    let first = uuid::Uuid::new_v4().to_string();
    let mut answer = Answer {
        reader: SseReader::new(response).with_line_limit(MAX_LINE),
        id: format!("chatcmpl-grok-{}", &first[..12]),
        model: model.to_owned(),
        created: chrono::Utc::now().timestamp(),
        session,
        thinking,
        fingerprint: Value::Null,
        text: String::new(),
        reasoning: String::new(),
        incoming_bytes: 0,
        tools,
    };
    if streaming {
        return Ok(answer.stream());
    }
    answer.collect().await?;
    Ok(answer.json())
}

fn prompt(body: &Value) -> Result<(String, Option<ToolBridge>), Failure> {
    let messages = body["messages"]
        .as_array()
        .filter(|messages| !messages.is_empty())
        .ok_or_else(|| Failure::new(StatusCode::BAD_REQUEST, "Missing or empty messages array"))?;
    let tools = ToolBridge::from_request(body, messages)?;
    let last_user = messages
        .iter()
        .rposition(|message| message["role"] == "user");
    let mut lines = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let role = if truthy(&message["role"]) {
            js_text(&message["role"])
        } else {
            "user".into()
        };
        let role = if role == "developer" {
            "system".into()
        } else {
            role
        };
        let mut text = match &message["content"] {
            Value::String(text) => text.to_owned(),
            Value::Array(parts) if tools.is_some() => parts
                .iter()
                .filter_map(|part| part.as_str().or_else(|| part["text"].as_str()))
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            Value::Array(parts) => parts
                .iter()
                .filter(|part| part["type"] == "text")
                .map(|part| {
                    if truthy(&part["text"]) {
                        js_text(&part["text"])
                    } else {
                        String::new()
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
        if tools.is_some() {
            if role == "tool" {
                if last_user.is_some_and(|user| index < user) {
                    continue;
                }
                let name = message["name"].as_str().unwrap_or("unknown_tool");
                let id = message["tool_call_id"].as_str().unwrap_or("unknown_call");
                text = format!(
                    "CLIENT TOOL RESULT from caller runtime for {name} ({id}). Use this result to answer; do not call the same tool again:\n{text}"
                );
            } else if role == "assistant"
                && let Some(calls) = message["tool_calls"].as_array()
            {
                if last_user.is_some_and(|user| index < user) {
                    continue;
                }
                let calls = calls
                    .iter()
                    .map(|call| json!({"id":call["id"],"function":call["function"]}))
                    .collect::<Vec<_>>();
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&format!("Previous assistant tool calls: {}", json!(calls)));
            }
        }
        if !js_trim(&text).is_empty() {
            lines.push((role, text));
        }
    }
    let recent = lines.iter().rposition(|(role, _)| role == "user");
    if lines.is_empty() && tools.is_none() {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "Empty query after processing",
        ));
    }
    let mut message = lines
        .into_iter()
        .enumerate()
        .map(|(index, (role, text))| {
            if Some(index) == recent {
                text
            } else {
                format!("{role}: {text}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if let Some(tools) = &tools {
        if !message.is_empty() {
            message.push_str("\n\n");
        }
        message.push_str(tools.manifest());
    }
    Ok((message, tools))
}

struct Answer {
    reader: SseReader,
    id: String,
    model: String,
    created: i64,
    session: Session,
    thinking: bool,
    fingerprint: Value,
    text: String,
    reasoning: String,
    incoming_bytes: usize,
    tools: Option<ToolBridge>,
}

impl Answer {
    async fn advance(&mut self, streaming: bool) -> Result<Option<Value>, Failure> {
        loop {
            let line = match self.reader.next(TIMEOUT, None).await {
                ReadLine::Line(line, _) => line,
                ReadLine::End if !self.text.is_empty() || !self.reasoning.is_empty() => {
                    return Ok(None);
                }
                ReadLine::End | ReadLine::Disconnected => {
                    return Err(Failure::upstream("Grok Web conversation was incomplete"));
                }
                ReadLine::Timeout => {
                    return Err(Failure::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        "Grok Web conversation timed out",
                    ));
                }
            };
            self.incoming_bytes += line.len().saturating_add(1);
            if self.incoming_bytes > MAX_RESPONSE {
                return Err(Failure::upstream(
                    "Grok Web conversation exceeded its size limit",
                ));
            }
            let Ok(event) = serde_json::from_str::<Value>(js_trim(&line)) else {
                continue;
            };
            if truthy(&event["error"]) {
                return Err(Failure::upstream("Grok Web returned a conversation error"));
            }
            let response = &event["result"]["response"];
            if self.fingerprint.is_null()
                && let Some(hash) = response["llmInfo"]["modelHash"]
                    .as_str()
                    .filter(|hash| !hash.is_empty())
            {
                self.fingerprint = json!(self.session.redact(hash));
            }
            if truthy(&response["toolUsageCard"]) {
                continue;
            }
            let snapshot = &response["modelResponse"];
            if let Some(hash) = snapshot["metadata"]["llm_info"]["modelHash"]
                .as_str()
                .filter(|hash| !hash.is_empty())
            {
                self.fingerprint = json!(self.session.redact(hash));
            }
            let (value, full) = match snapshot["message"].as_str() {
                Some(value) if !value.is_empty() => (value, true),
                _ => match response["token"].as_str().filter(|value| !value.is_empty()) {
                    Some(value) => (value, false),
                    None => continue,
                },
            };
            let thought =
                self.thinking && (response["isThinking"] == true || snapshot["isThinking"] == true);
            let target = if thought {
                &mut self.reasoning
            } else {
                &mut self.text
            };
            let delta = if full {
                let suffix = value
                    .strip_prefix(target.as_str())
                    .unwrap_or_default()
                    .to_owned();
                if !streaming || !suffix.is_empty() {
                    *target = value.to_owned();
                }
                suffix
            } else {
                target.push_str(value);
                value.to_owned()
            };
            if !streaming || !delta.is_empty() {
                let field = if thought {
                    "reasoning_content"
                } else {
                    "content"
                };
                return Ok(Some(json!({(field):self.session.redact(&delta)})));
            }
        }
    }

    async fn collect(&mut self) -> Result<(), Failure> {
        while self.advance(false).await?.is_some() {}
        Ok(())
    }

    fn completion_parts(&self) -> (String, Vec<Value>, String) {
        let text = self.session.redact(&self.text);
        let reasoning = self.session.redact(&self.reasoning);
        match &self.tools {
            Some(tools) => {
                let (text, mut calls) = tools.parse(&text);
                for call in &mut calls {
                    if let Some(args) = call["function"]["arguments"].as_str() {
                        call["function"]["arguments"] = json!(self.session.redact(args));
                    }
                }
                (text, calls, tools.redact(&reasoning))
            }
            None => (text, Vec::new(), reasoning),
        }
    }

    fn json(self) -> Response {
        let tokens = self.text.encode_utf16().count().div_ceil(4);
        let (text, calls, reasoning) = self.completion_parts();
        let mut message = json!({"role":"assistant","content":text});
        if !reasoning.is_empty() {
            message["reasoning_content"] = json!(reasoning);
        }
        let finish = if calls.is_empty() {
            "stop"
        } else {
            message["tool_calls"] = json!(calls);
            "tool_calls"
        };
        let output = json!({
            "id":self.id,"object":"chat.completion","created":self.created,"model":self.model,
            "system_fingerprint":self.fingerprint,
            "choices":[{"index":0,"message":message,"finish_reason":finish,"logprobs":null}],
            "usage":{"prompt_tokens":tokens,"completion_tokens":tokens,"total_tokens":tokens*2},
        });
        axum::http::Response::builder()
            .url(Url::parse(CONVERSATIONS).expect("fixed Grok Web endpoint"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(output.to_string()))
            .expect("Grok Web JSON response")
            .into()
    }

    fn frame(&self, delta: Value, finish: Option<&str>) -> String {
        let frame = json!({
            "id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,
            "system_fingerprint":self.fingerprint,
            "choices":[{"index":0,"delta":delta,"finish_reason":finish,"logprobs":null}],
        });
        format!("data: {frame}\n\n")
    }

    fn tool_frames(&self) -> String {
        let (text, calls, reasoning) = self.completion_parts();
        let finish = if calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        let mut wire = String::new();
        if !reasoning.is_empty() {
            wire.push_str(&self.frame(json!({"reasoning_content":reasoning}), None));
        }
        if !text.is_empty() {
            wire.push_str(&self.frame(json!({"content":text}), None));
        }
        for (index, mut call) in calls.into_iter().enumerate() {
            call["index"] = json!(index);
            wire.push_str(&self.frame(json!({"tool_calls":[call]}), None));
        }
        wire.push_str(&self.frame(json!({}), Some(finish)));
        wire.push_str("data: [DONE]\n\n");
        wire
    }

    fn error_frame(&self, failure: Failure) -> String {
        self.frame(
            json!({"content":format!("[Error: {}]",failure.message)}),
            Some("stop"),
        ) + "data: [DONE]\n\n"
    }

    fn stream(self) -> Response {
        let outgoing = stream::unfold((self, Some(false)), |(mut answer, stage)| async move {
            let (wire, next) = if !stage? {
                (answer.frame(json!({"role":"assistant"}), None), Some(true))
            } else if answer.tools.is_some() {
                let wire = match answer.collect().await {
                    Ok(()) => answer.tool_frames(),
                    Err(failure) => answer.error_frame(failure),
                };
                (wire, None)
            } else {
                match answer.advance(true).await {
                    Ok(Some(delta)) => (answer.frame(delta, None), Some(true)),
                    Ok(None) => (
                        answer.frame(json!({}), Some("stop")) + "data: [DONE]\n\n",
                        None,
                    ),
                    Err(failure) => (answer.error_frame(failure), None),
                }
            };
            Some((Ok::<_, Infallible>(Bytes::from(wire)), (answer, next)))
        });
        axum::http::Response::builder()
            .url(Url::parse(CONVERSATIONS).expect("fixed Grok Web endpoint"))
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("x-accel-buffering", "no")
            .body(Body::wrap_stream(outgoing))
            .expect("Grok Web event response")
            .into()
    }
}
