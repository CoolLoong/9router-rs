use std::sync::LazyLock;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode, header};
use reqwest::{Client, Response, ResponseBuilderExt};
use serde_json::Value;

use crate::cursor_identity::Account;
use crate::provider_http::{Delivery, http_client};
use crate::storage::Storage;

#[path = "cursor_agent.rs"]
mod agent;
#[path = "cursor_legacy.rs"]
mod legacy;
#[path = "cursor_request.rs"]
mod request;

pub(super) const ENDPOINT: &str =
    "https://api2.cursor.sh/aiserver.v1.ChatService/StreamUnifiedChatWithTools";
const FIXTURE: &str =
    "http://127.0.0.1:52989/cursor/aiserver.v1.ChatService/StreamUnifiedChatWithTools";
pub(super) const MEDIA: &str = "application/connect+proto";
const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_RESPONSE: usize = 32 * 1024 * 1024;

static DIRECT: LazyLock<Option<Client>> =
    LazyLock::new(|| http_client(TIMEOUT).http2_prior_knowledge().build().ok());
static FIXTURE_CLIENT: LazyLock<Option<Client>> =
    LazyLock::new(|| http_client(TIMEOUT).build().ok());

pub(super) use legacy::is_legacy_body;
pub(super) use request::prepare_openai;

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

    fn unavailable() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "Cursor outbound connection is unavailable",
        )
    }
}

pub(super) fn supports_account(connection: &Value) -> bool {
    connection["provider"] == "cursor" && Account::from_connection(connection).is_some()
}

pub(super) async fn complete(
    storage: &Storage,
    settings: &Value,
    connection: &Value,
    model: &str,
    incoming: &HeaderMap,
    body: &Value,
    stream: bool,
) -> Result<Response, Failure> {
    let account = Account::from_connection(connection).ok_or_else(|| {
        Failure::new(
            StatusCode::UNAUTHORIZED,
            "Cursor requires a saved access token and machine ID",
        )
    })?;
    if !is_legacy_body(body) {
        return agent::complete(storage, settings, connection, account, model, body, stream).await;
    }
    let delivery = Delivery::select(storage, settings, connection, ENDPOINT)
        .await
        .map_err(|_| Failure::unavailable())?;
    let fixture = delivery.use_fixture();
    let direct = if fixture { &FIXTURE_CLIENT } else { &DIRECT };
    let client = delivery
        .client(direct.as_ref().ok_or_else(Failure::unavailable)?, TIMEOUT)
        .ok_or_else(Failure::unavailable)?;
    let ua = incoming
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let force_agent = ["claude-cli", "claude-code", "Claude Code"]
        .into_iter()
        .any(|value| ua.contains(value));
    let request = legacy::request(body, model, force_agent).ok_or_else(|| {
        Failure::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Cursor request exceeds the Connect frame limit",
        )
    })?;
    let endpoint = if fixture { FIXTURE } else { ENDPOINT };
    let mut response = client
        .post(endpoint)
        .headers(headers(&account)?)
        .body(request)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                Failure::new(StatusCode::GATEWAY_TIMEOUT, "Cursor upstream timed out")
            } else {
                Failure::unavailable()
            }
        })?;
    let status = response.status();
    let destination = response.url().clone();
    if status.is_redirection() {
        return Err(Failure::new(
            StatusCode::BAD_GATEWAY,
            "Cursor upstream redirects are refused",
        ));
    }
    let mut buffer = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Failure::unavailable())? {
        if chunk.len() > MAX_RESPONSE - buffer.len() {
            return Err(Failure::new(
                StatusCode::BAD_GATEWAY,
                "Cursor response exceeded the size limit",
            ));
        }
        buffer.extend_from_slice(&chunk);
    }
    let redact = |text: &str| {
        text.replace(account.token, "[redacted]")
            .replace(account.machine, "[redacted]")
    };
    if status != StatusCode::OK {
        let error = String::from_utf8_lossy(&buffer);
        let detail = if error.is_empty() {
            "Unknown error"
        } else {
            &error
        };
        return Err(Failure::new(
            status,
            format!("[{}]: {}", status.as_u16(), redact(detail)),
        ));
    }
    let (status, content_type, body) = match legacy::render(&buffer, model, body, stream) {
        legacy::Rendered::Json(status, body) => {
            (status, "application/json", redact(&body.to_string()))
        }
        legacy::Rendered::Sse(body) => (200, "text/event-stream", redact(&body)),
    };
    axum::http::Response::builder()
        .url(destination)
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(reqwest::Body::from(body))
        .map(Into::into)
        .map_err(|_| Failure::unavailable())
}

fn headers(account: &Account<'_>) -> Result<HeaderMap, Failure> {
    let mut headers = HeaderMap::new();
    let checksum = account.checksum().ok_or_else(Failure::unavailable)?;
    let mut auth = header::HeaderValue::from_str(&format!("Bearer {}", account.token))
        .map_err(|_| Failure::new(StatusCode::UNAUTHORIZED, "Invalid Cursor authorization"))?;
    auth.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, auth);
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(MEDIA),
    );
    headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_static("connect-es/1.6.1"),
    );
    for (name, value) in [
        ("connect-accept-encoding", "gzip"),
        ("connect-protocol-version", "1"),
        ("x-cursor-client-version", "3.12.17"),
        ("x-ghost-mode", if account.ghost { "true" } else { "false" }),
    ] {
        headers.insert(name, header::HeaderValue::from_static(value));
    }
    let mut checksum =
        header::HeaderValue::from_str(&checksum).map_err(|_| Failure::unavailable())?;
    checksum.set_sensitive(true);
    headers.insert("x-cursor-checksum", checksum);
    Ok(headers)
}
