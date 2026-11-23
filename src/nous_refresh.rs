use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{SecondsFormat, Utc};
use reqwest::StatusCode;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::flow::nonempty;
use super::nous_portal::{self, Prepared};
use super::{RefreshError, refresh::time_ms, store::expiry};

pub(crate) const ATTEMPT: &str = "nousRefreshAttempt";
pub(super) const CONTEXT: &[&str] = &[
    "inferenceBaseUrl",
    "agentKeyId",
    "agentKeyExpiresAt",
    "proxyPoolId",
    "connectionProxyEnabled",
    "connectionProxyUrl",
    "connectionNoProxy",
    "strictProxy",
    "vercelRelayUrl",
];

pub(crate) fn compatible(account: &Value) -> bool {
    matches!(
        account["authType"].as_str(),
        Some("oauth" | "apikey" | "api_key")
    )
}

pub(crate) fn inference_token(account: &Value) -> Option<&str> {
    let api_key = nonempty(&account["apiKey"]);
    match account["authType"].as_str() {
        Some("apikey" | "api_key") => api_key,
        Some("oauth") => {
            let now_ms = Utc::now().timestamp_millis();
            valid_agent_key(account, now_ms).or_else(|| {
                nonempty(&account["accessToken"])
                    .filter(|_| valid_at(access_expiry(account), now_ms))
            })
        }
        _ => None,
    }
}

pub(super) fn refresh_due(account: &Value, now_ms: i64, lead: i64) -> bool {
    let check_time = now_ms.saturating_add(lead);
    valid_agent_key(account, check_time).is_none()
        && (nonempty(&account["accessToken"]).is_none()
            || !valid_at(access_expiry(account), check_time))
}

pub(crate) fn can_refresh(account: &Value) -> bool {
    account["authType"] == "oauth" && nonempty(&account["refreshToken"]).is_some()
}

pub(crate) fn redact(account: &Value, message: &str) -> String {
    let mut credentials = ["apiKey", "accessToken", "refreshToken"]
        .into_iter()
        .filter_map(|field| nonempty(&account[field]))
        .collect::<Vec<_>>();
    credentials.sort_unstable_by_key(|token| std::cmp::Reverse(token.len()));
    credentials
        .into_iter()
        .fold(message.to_owned(), |text, token| {
            text.replace(token, "[redacted]")
        })
}

fn access_expiry(account: &Value) -> &Value {
    account
        .get("expiresAt")
        .filter(|value| !value.is_null())
        .unwrap_or(&account["tokenExpiresAt"])
}

fn valid_agent_key(account: &Value, at: i64) -> Option<&str> {
    nonempty(&account["apiKey"])
        .filter(|key| Some(*key) != nonempty(&account["accessToken"]))
        .filter(|_| valid_at(&account["providerSpecificData"]["agentKeyExpiresAt"], at))
}

fn valid_at(expiration: &Value, at: i64) -> bool {
    expiration.is_null() || time_ms(expiration).is_some_and(|expiry| expiry > at)
}

pub(super) fn marker(refresh: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(refresh.as_bytes()))
}

pub(super) enum Outcome {
    Complete(Map<String, Value>),
    Throttled,
    Uncertain,
}

pub(super) async fn execute(request: Prepared) -> Outcome {
    let Ok((status, data)) = request.send().await else {
        return Outcome::Uncertain;
    };
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Outcome::Throttled;
    }
    let Some(access) = nonempty(&data["access_token"]).filter(|_| status.is_success()) else {
        return Outcome::Uncertain;
    };
    let now = Utc::now();
    let expiration = expiry(now, data.get("expires_in"));
    let Ok(expiration) = expiration else {
        return Outcome::Uncertain;
    };
    let (key, details) = nous_portal::credentials(access, &Value::Null);
    let mut patch = Map::from_iter([
        ("accessToken".into(), json!(access)),
        ("apiKey".into(), json!(key)),
        ("expiresAt".into(), expiration),
        ("tokenExpiresAt".into(), Value::Null),
        ("providerSpecificData".into(), Value::Object(details)),
        ("expiresIn".into(), data["expires_in"].clone()),
        (
            "lastRefreshAt".into(),
            json!(now.to_rfc3339_opts(SecondsFormat::Millis, true)),
        ),
        (ATTEMPT.into(), Value::Null),
    ]);
    if let Some(refresh) = nonempty(&data["refresh_token"]) {
        patch.insert("refreshToken".into(), json!(refresh));
    }
    if let Some(scope) = nonempty(&data["scope"]) {
        patch.insert("scope".into(), json!(scope));
    }
    Outcome::Complete(patch)
}

pub(super) fn check_marker(account: &Value, refresh: &str) -> Result<String, RefreshError> {
    let marker = marker(refresh);
    if account[ATTEMPT] == marker {
        return Err(RefreshError::ReauthenticationRequired);
    }
    Ok(marker)
}
