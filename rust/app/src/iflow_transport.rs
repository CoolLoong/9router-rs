use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::{RequestBuilder, header};
use serde_json::{Value, json};
use sha2::Sha256;
use uuid::Uuid;

use crate::chat_input::PreparedChat;
use crate::provider_connections::truthy;

pub const ENDPOINT: &str = "https://apis.iflow.cn/v1/chat/completions";
const AGENT: &str = "iFlow-Cli";

pub fn fixture_url() -> Option<String> {
    (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"))
        .then(|| "http://127.0.0.1:52989/iflow-fixture/v1/chat/completions".into())
}

pub fn supports(account: &Value) -> bool {
    signed_key(account).is_some()
        && [
            &account["expiresAt"],
            &account["providerSpecificData"]["expireTime"],
        ]
        .into_iter()
        .filter(|value| truthy(value))
        .all(|expiry| {
            expiry
                .as_str()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|time| time.timestamp_millis())
                .or_else(|| {
                    expiry.as_i64().filter(|value| *value > 0).map(|time| {
                        if time < 100_000_000_000 {
                            time * 1000
                        } else {
                            time
                        }
                    })
                })
                .is_some_and(|time| time > Utc::now().timestamp_millis())
        })
}

fn signed_key(account: &Value) -> Option<&str> {
    [&account["apiKey"], &account["accessToken"]]
        .into_iter()
        .find_map(|value| value.as_str().filter(|key| !key.is_empty()))
}

pub fn redact_error(account: &Value, message: &str) -> String {
    signed_key(account).map_or_else(
        || message.to_owned(),
        |key| message.replace(key, "[redacted]"),
    )
}

pub fn can_refresh(account: &Value) -> bool {
    account["authType"] == "oauth"
        && account["refreshToken"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
        && account["apiKey"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
}

pub fn prepare(request: &mut PreparedChat) {
    if request.stream
        && truthy(&request.body["messages"])
        && !truthy(&request.body["stream_options"])
    {
        request.body["stream_options"] = json!({"include_usage":true});
    }
}

pub fn headers(request: RequestBuilder, account: &Value, stream: bool) -> RequestBuilder {
    let timestamp = Utc::now().timestamp_millis();
    let session = format!("session-{}", Uuid::new_v4());
    let payload = format!("{AGENT}:{session}:{timestamp}");
    let signature = signed_key(account)
        .and_then(|key| Hmac::<Sha256>::new_from_slice(key.as_bytes()).ok())
        .map(|mut mac| {
            mac.update(payload.as_bytes());
            format!("{:x}", mac.finalize().into_bytes())
        })
        .unwrap_or_default();
    let mut request = request
        .header(header::USER_AGENT, AGENT)
        .header("session-id", session)
        .header("x-iflow-timestamp", timestamp.to_string())
        .header("x-iflow-signature", signature);
    if let Some(key) = account["apiKey"].as_str().filter(|key| !key.is_empty()) {
        request = request.bearer_auth(key);
    }
    if stream {
        request = request.header(header::ACCEPT, "text/event-stream");
    }
    request
}
