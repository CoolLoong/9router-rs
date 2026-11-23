use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{SecondsFormat, Utc};
use reqwest::StatusCode;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::kimi_portal::{self, Prepared, saved_device_id, text};
use super::refresh::{RefreshMode, kimi_due, refresh_account};
use crate::storage::Storage;

pub(crate) const ATTEMPT: &str = "kimiRefreshAttempt";
pub(super) const CONTEXT: &[&str] = &[
    "authMethod",
    "deviceId",
    "deviceClientId",
    "proxyPoolId",
    "connectionProxyEnabled",
    "connectionProxyUrl",
    "connectionNoProxy",
    "strictProxy",
    "vercelRelayUrl",
];

pub(super) struct Grant<'a> {
    pub device: &'a str,
    pub token: &'a str,
    pub client_id: Option<&'a str>,
}

impl<'a> Grant<'a> {
    pub(super) fn from_account(account: &'a Value) -> Option<Self> {
        if account["provider"] != "kimi" || account["authType"] != "oauth" {
            return None;
        }
        let details = &account["providerSpecificData"];
        let device = saved_device_id(account)?;
        Some(Self {
            device,
            token: text(&account["refreshToken"])?,
            client_id: text(&details["deviceClientId"]),
        })
    }
}

pub(super) fn marker(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
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
    if !status.is_success() || !data["error"].is_null() {
        return Outcome::Uncertain;
    }
    let now = Utc::now();
    let Ok(mut fields) = kimi_portal::token_fields(&data, now) else {
        return Outcome::Uncertain;
    };
    fields.insert("tokenExpiresAt".into(), Value::Null);
    fields.insert(
        "lastRefreshAt".into(),
        json!(now.to_rfc3339_opts(SecondsFormat::Millis, true)),
    );
    fields.insert(ATTEMPT.into(), Value::Null);
    Outcome::Complete(fields)
}

pub(crate) async fn test_expiry(
    storage: &Storage,
    account: Value,
) -> (Value, Option<String>, bool) {
    if text(&account["accessToken"]).is_none() {
        return (account, Some("No access token".into()), false);
    }
    if !kimi_due(&account) {
        return (account, None, false);
    }
    if text(&account["refreshToken"]).is_none() {
        return (account, Some("Token expired".into()), false);
    }
    match refresh_account(storage, &account, RefreshMode::Proactive).await {
        Ok(latest) if text(&latest["accessToken"]).is_some() && !kimi_due(&latest) => {
            let changed = ["accessToken", "refreshToken", "lastRefreshAt"]
                .iter()
                .any(|key| account[*key] != latest[*key]);
            (latest, None, changed)
        }
        _ => (
            account,
            Some("Token expired and refresh failed".into()),
            false,
        ),
    }
}
