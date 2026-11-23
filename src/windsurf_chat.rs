use std::sync::LazyLock;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{StatusCode, header};
use futures_util::StreamExt;
use reqwest::{Client, RequestBuilder, Response, ResponseBuilderExt};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::chat_accounts;
use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_http::{Delivery, http_client, requires_protection};
use crate::storage::Storage;

const RPC_PATH: &str = "/exa.language_server_pb.LanguageServerService/GetChatMessage";
const FIXTURE: &str =
    "http://127.0.0.1:52989/windsurf/exa.language_server_pb.LanguageServerService/GetChatMessage";
const MEDIA: &str = "application/grpc-web+proto";
const TIMEOUT: Duration = Duration::from_secs(180);
const MAX_FRAME: usize = 16 * 1024 * 1024;
static CLIENT: LazyLock<Option<Client>> = LazyLock::new(|| http_client(TIMEOUT).build().ok());
static MODELS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("windsurf-model-aliases.json"))
        .expect("embedded Windsurf model aliases")
});

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
            "Windsurf outbound connection is unavailable",
        )
    }
}

pub(super) fn supports(account: &Value) -> bool {
    account["provider"] == "windsurf"
        && matches!(
            account["authType"].as_str(),
            Some("oauth" | "apikey" | "api_key")
        )
        && token(account).is_some()
}

pub(super) async fn protects_chat(storage: &Storage, settings: &Value) -> bool {
    requires_protection(settings, &Value::Null)
        || chat_accounts::active_connections(storage, "windsurf")
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
) -> Result<Response, Failure> {
    let key = token(account)
        .ok_or_else(|| Failure::new(StatusCode::UNAUTHORIZED, "No Windsurf credential found"))?;
    let destination = destination(account).ok_or_else(|| {
        Failure::new(
            StatusCode::BAD_GATEWAY,
            "Windsurf returned an unsupported API server",
        )
    })?;
    let route = Delivery::select(storage, settings, account, &destination)
        .await
        .map_err(|_| Failure::transport())?;
    let client = route
        .client(CLIENT.as_ref().ok_or_else(Failure::transport)?, TIMEOUT)
        .ok_or_else(Failure::transport)?;
    let endpoint = if route.use_fixture() {
        FIXTURE
    } else {
        &destination
    };
    let payload = request(key, MODELS[model].as_str().unwrap_or(model), body)?;
    let mut overrides = crate::provider_overrides::transport_headers(settings, "windsurf");
    for name in [
        "authorization",
        "proxy-authorization",
        "content-length",
        "host",
        "x-grpc-web",
        "content-type",
    ] {
        overrides.remove(name);
    }
    let response = headers(client.post(endpoint))
        .headers(overrides)
        .bearer_auth(key)
        .body(payload)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                Failure::new(StatusCode::GATEWAY_TIMEOUT, "Windsurf upstream timed out")
            } else {
                Failure::transport()
            }
        })?;
    let status = response.status();
    if status.is_redirection() {
        return Err(Failure::new(
            StatusCode::BAD_GATEWAY,
            "Windsurf upstream redirected the request",
        ));
    }
    if !status.is_success() {
        let message = bounded_error(response, key).await;
        return Err(Failure::new(status, message));
    }
    Ok(adapt(response, model, key))
}

pub(super) fn destination(account: &Value) -> Option<String> {
    let origin = crate::provider_oauth::windsurf::api_server(
        account["providerSpecificData"]["apiServerUrl"].as_str(),
    )?;
    Some(format!("{origin}{RPC_PATH}"))
}

pub(super) fn headers(request: RequestBuilder) -> RequestBuilder {
    request
        .header(header::CONTENT_TYPE, MEDIA)
        .header(header::ACCEPT, MEDIA)
        .header(header::USER_AGENT, "windsurf/3.14.0")
        .header("X-Grpc-Web", "1")
}

async fn bounded_error(mut response: Response, key: &str) -> String {
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if chunk.len() > 16 * 1024 - bytes.len() {
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return "Windsurf rejected the request".to_owned();
    }
    String::from_utf8_lossy(&bytes).replace(key, "[redacted]")
}

fn token(account: &Value) -> Option<&str> {
    [&account["accessToken"], &account["apiKey"]]
        .into_iter()
        .filter_map(Value::as_str)
        .find(|key| !key.is_empty())
}

fn request(key: &str, model: &str, body: &Value) -> Result<Vec<u8>, Failure> {
    let session = Uuid::new_v4().to_string();
    let cascade = Uuid::new_v4().to_string();
    let mut payload = vec![0; 5];
    let mut metadata = Vec::new();
    for (index, text) in [key, "windsurf", "3.14.0", "3.14.0", &session, "en-US"]
        .into_iter()
        .enumerate()
    {
        encode(index as u64 + 1, text.as_bytes(), &mut metadata);
    }
    encode(1, &metadata, &mut payload);
    encode(2, cascade.as_bytes(), &mut payload);
    let mut model_field = Vec::new();
    encode(1, model.as_bytes(), &mut model_field);
    encode(3, &model_field, &mut payload);
    if let Some(messages) = body["messages"]
        .as_array()
        .filter(|messages| !messages.is_empty())
    {
        for message in messages {
            let role = message
                .get("role")
                .filter(|role| truthy(role))
                .map_or_else(|| "user".into(), js_text);
            let text = if let Some(text) = message["content"].as_str() {
                text.to_owned()
            } else {
                message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|part| part["type"] == "text" && truthy(&part["text"]))
                    .map(|part| js_text(&part["text"]))
                    .collect::<String>()
            };
            let id = message
                .get("tool_call_id")
                .filter(|id| truthy(id))
                .map(js_text);
            encode_message(&role, &text, id.as_deref(), &mut payload);
        }
    } else {
        encode_message("user", "", None, &mut payload);
    }
    let size = payload.len() - 5;
    if size > MAX_FRAME {
        return Err(Failure::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Windsurf request is too large",
        ));
    }
    payload[1..5].copy_from_slice(&(size as u32).to_be_bytes());
    Ok(payload)
}

fn encode_message(role: &str, text: &str, id: Option<&str>, payload: &mut Vec<u8>) {
    let mut message = Vec::new();
    encode(1, role.as_bytes(), &mut message);
    encode(2, text.as_bytes(), &mut message);
    if let Some(id) = id {
        encode(3, id.as_bytes(), &mut message);
    }
    encode(4, &message, payload);
}

fn encode(field: u64, value: &[u8], output: &mut Vec<u8>) {
    varint((field << 3) | 2, output);
    varint(value.len() as u64, output);
    output.extend_from_slice(value);
}

fn varint(mut value: u64, output: &mut Vec<u8>) {
    while value > 0x7f {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn read_varint(data: &mut &[u8]) -> Option<u64> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = data.split_first()?;
        *data = rest;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

enum ValueOnWire<'a> {
    Varint(u64),
    Message(&'a [u8]),
    Skipped,
}

fn next_field<'a>(data: &mut &'a [u8]) -> Option<(u64, ValueOnWire<'a>)> {
    let tag = read_varint(data)?;
    let value = match tag & 7 {
        0 => ValueOnWire::Varint(read_varint(data)?),
        1 | 5 => {
            let size = if tag & 7 == 1 { 8 } else { 4 };
            *data = data.get(size..)?;
            ValueOnWire::Skipped
        }
        2 => {
            let size = usize::try_from(read_varint(data)?).ok()?;
            let (value, rest) = data.split_at_checked(size)?;
            *data = rest;
            ValueOnWire::Message(value)
        }
        _ => return None,
    };
    Some((tag >> 3, value))
}

fn nested(mut data: &[u8], number: u64) -> Option<&[u8]> {
    while let Some((field, value)) = next_field(&mut data) {
        if field == number
            && let ValueOnWire::Message(value) = value
        {
            return Some(value);
        }
    }
    None
}

struct Sse {
    id: String,
    model: String,
    key: String,
    created: i64,
    prompt: u64,
    completion: u64,
    role_sent: bool,
    error: Option<String>,
    buffer: Vec<u8>,
}

impl Sse {
    fn feed(&mut self, chunk: Bytes, output: &mut String) {
        self.buffer.extend_from_slice(&chunk);
        let mut rest = std::mem::take(&mut self.buffer);
        let mut offset = 0;
        while let Some(header) = rest.get(offset..offset + 5) {
            let size = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            if size > MAX_FRAME {
                self.error = Some("Windsurf response frame is too large".to_owned());
                offset = rest.len();
                break;
            }
            let end = offset + 5 + size;
            let Some(body) = rest.get(offset + 5..end) else {
                break;
            };
            match header[0] {
                0x00 => self.data(body, output),
                0x80 => self.trailer(&String::from_utf8_lossy(body)),
                _ => (),
            }
            offset = end;
        }
        rest.drain(..offset);
        self.buffer = rest;
    }

    fn data(&mut self, mut bytes: &[u8], output: &mut String) {
        while let Some((field, value)) = next_field(&mut bytes) {
            let ValueOnWire::Message(body) = value else {
                continue;
            };
            match field {
                1 => {
                    if let Some(text) = nested(body, 1) {
                        let text = String::from_utf8_lossy(text);
                        if !text.is_empty() {
                            if !self.role_sent {
                                emit(
                                    output,
                                    self.chunk(
                                        json!({"role":"assistant","content":""}),
                                        Value::Null,
                                    ),
                                );
                                self.role_sent = true;
                            }
                            emit(output, self.chunk(json!({"content":text}), Value::Null));
                        }
                        return;
                    }
                }
                3 => {
                    let mut usage = nested(body, 1).unwrap_or_default();
                    (self.prompt, self.completion) = (0, 0);
                    while let Some((field, value)) = next_field(&mut usage) {
                        if let ValueOnWire::Varint(value) = value {
                            match field {
                                1 => self.prompt = value,
                                2 => self.completion = value,
                                _ => (),
                            }
                        }
                    }
                    return;
                }
                4 => {
                    self.error = Some(nested(body, 1).map_or_else(
                        || "unknown windsurf error".into(),
                        |data| String::from_utf8_lossy(data).into_owned(),
                    ));
                    return;
                }
                _ => (),
            }
        }
    }

    fn trailer(&mut self, headers: &str) {
        let header = |expected: &str| {
            headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case(expected))
                .map(|(_, value)| value.trim())
        };
        if let Some(code) = header("grpc-status").filter(|code| !code.is_empty() && *code != "0") {
            self.error = Some(header("grpc-message").map_or_else(
                || format!("gRPC status {code}"),
                |message| {
                    percent_encoding::percent_decode_str(message)
                        .decode_utf8_lossy()
                        .into_owned()
                },
            ));
        }
    }

    fn chunk(&self, delta: Value, finish: Value) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    }

    fn finish(&self, output: &mut String) {
        let truncated = (!self.buffer.is_empty())
            .then_some("Windsurf upstream returned an incomplete response frame");
        if let Some(message) = self.error.as_deref().or(truncated) {
            let message: String = message
                .replace(&self.key, "[redacted]")
                .chars()
                .take(2000)
                .collect();
            emit(
                output,
                json!({"error":{"message":message,"type":"windsurf_error","code":"upstream_error"}}),
            );
        } else {
            let mut done = self.chunk(json!({}), json!("stop"));
            if self.prompt > 0 || self.completion > 0 {
                done["usage"] = json!({"prompt_tokens":self.prompt,"completion_tokens":self.completion,
                    "total_tokens":self.prompt.saturating_add(self.completion)});
            }
            emit(output, done);
        }
        output.push_str("data: [DONE]\n\n");
    }
}

fn emit(output: &mut String, value: Value) {
    output.push_str("data: ");
    output.push_str(&value.to_string());
    output.push_str("\n\n");
}

fn adapt(response: Response, model: &str, key: &str) -> Response {
    let endpoint = response.url().clone();
    let now = chrono::Utc::now();
    let sse = Sse {
        id: format!("chatcmpl-ws-{}", now.timestamp_millis()),
        model: model.into(),
        key: key.to_owned(),
        created: now.timestamp(),
        prompt: 0,
        completion: 0,
        role_sent: false,
        error: None,
        buffer: Vec::new(),
    };
    let input = (Box::pin(response.bytes_stream()), sse, false);
    let stream = futures_util::stream::unfold(input, |(mut input, mut sse, ended)| async move {
        if ended {
            return None;
        }
        loop {
            let mut output = String::new();
            match input.next().await {
                Some(Ok(bytes)) => sse.feed(bytes, &mut output),
                Some(Err(_)) => {
                    sse.error = Some("Windsurf stream error: connection interrupted".into());
                    sse.finish(&mut output);
                    return Some((
                        Ok::<_, std::convert::Infallible>(Bytes::from(output)),
                        (input, sse, true),
                    ));
                }
                None => {
                    sse.finish(&mut output);
                    return Some((Ok(Bytes::from(output)), (input, sse, true)));
                }
            }
            if !output.is_empty() {
                return Some((Ok(Bytes::from(output)), (input, sse, false)));
            }
        }
    });
    axum::http::Response::builder()
        .url(endpoint)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(reqwest::Body::wrap_stream(stream))
        .expect("Windsurf SSE response")
        .into()
}
