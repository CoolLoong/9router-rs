use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::{Client, Method, Response, StatusCode, header};
use serde_json::{Map, Value, json};

use crate::chat_output::js_text;
use crate::grok_cli_http::{account_access, identity_headers, refresh_mode};
use crate::provider_connections::truthy;
use crate::provider_http::{self, Bound, http_client};
use crate::provider_index::js_trim;
use crate::provider_oauth::{RefreshError, refresh_account};
use crate::storage::Storage;

const ENDPOINT: &str = "https://cli-chat-proxy.grok.com/v1/models";
const FIXTURE: &str = "http://127.0.0.1:52989/grok-cli/v1/models";
const BUILD_MODEL: &str = "grok-build";
const TIMEOUT: Duration = Duration::from_secs(12);
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
const MAX_DETAIL: usize = 64 * 1024;

static HTTP: LazyLock<Option<Client>> = LazyLock::new(|| http_client(TIMEOUT).build().ok());

pub(crate) enum Discovery {
    Models(Vec<Value>),
    Unavailable(String),
}

pub(crate) async fn discover(storage: &Storage, account: &Value) -> Discovery {
    let token = match access(account) {
        Ok(token) => token,
        Err(error) => return error,
    };
    let binding = match egress(storage, account).await {
        Ok(binding) => binding,
        Err(error) => return error,
    };
    let (response, renewed) = match fetch_with_refresh(storage, account, token, &binding).await {
        Ok(result) => result,
        Err(result) => return result,
    };
    let mut credentials = secrets(account);
    credentials.extend(binding.relay_secret());
    if let Some(refreshed) = &renewed {
        credentials.extend(secrets(refreshed));
    }
    credentials
        .sort_unstable_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    credentials.dedup();
    let status = response.status();
    if !status.is_success() {
        let body = bytes(response, MAX_DETAIL).await.unwrap_or_default();
        let detail = redact(&String::from_utf8_lossy(&body), &credentials);
        let detail = String::from_utf16_lossy(&detail.encode_utf16().take(160).collect::<Vec<_>>());
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        return Discovery::Unavailable(format!(
            "Grok CLI model discovery failed ({}){suffix}",
            status.as_u16()
        ));
    }
    let Some(body) = bytes(response, MAX_RESPONSE).await else {
        return unavailable("Grok CLI model discovery failed: invalid or oversized response.");
    };
    let Ok(payload) = serde_json::from_slice(&body) else {
        return unavailable("Grok CLI model discovery failed: invalid response.");
    };
    let mut models = parse(&payload);
    models.retain(|model| {
        model["id"]
            .as_str()
            .is_some_and(|id| !credentials.iter().any(|secret| id.contains(secret)))
    });
    for model in &mut models {
        let id = model["id"].clone();
        scrub(model, &credentials);
        model["id"] = id;
    }
    if models.is_empty() {
        unavailable("Grok CLI returned no selectable models.")
    } else {
        Discovery::Models(models)
    }
}

async fn fetch_with_refresh(
    storage: &Storage,
    account: &Value,
    token: &str,
    delivery: &Bound,
) -> Result<(Response, Option<Value>), Discovery> {
    let refresh = account["refreshToken"]
        .as_str()
        .is_some_and(|value| !js_trim(value).is_empty());
    if expired(account) {
        if !refresh {
            return Err(unavailable("Grok CLI access token has expired."));
        }
        return refreshed_catalog(storage, account, delivery).await;
    }
    let response = fetch(delivery, token, &account["providerSpecificData"]).await?;
    if refresh
        && matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        )
    {
        return refreshed_catalog(storage, account, delivery).await;
    }
    Ok((response, None))
}

async fn refreshed_catalog(
    storage: &Storage,
    account: &Value,
    before: &Bound,
) -> Result<(Response, Option<Value>), Discovery> {
    let current = match refresh_account(storage, account, refresh_mode(before)).await {
        Ok(current) => current,
        Err(RefreshError::Unsupported) if before.protected => {
            return Err(unavailable(
                "Grok CLI credential refresh failed: configured proxy is unavailable.",
            ));
        }
        Err(error) => {
            return Err(Discovery::Unavailable(format!(
                "Grok CLI credential refresh failed: {error}."
            )));
        }
    };
    if expired(&current) {
        return Err(unavailable("Grok CLI access token has expired."));
    }
    let delivery = egress(storage, &current).await?;
    if !before.same_route(&delivery) {
        return Err(unavailable(
            "Grok CLI proxy routing changed during credential refresh.",
        ));
    }
    let response = fetch(
        &delivery,
        access(&current)?,
        &current["providerSpecificData"],
    )
    .await?;
    Ok((response, Some(current)))
}

fn access(account: &Value) -> Result<&str, Discovery> {
    account["accessToken"]
        .as_str()
        .filter(|value| !js_trim(value).is_empty())
        .ok_or_else(|| unavailable("Grok CLI access token is missing."))?;
    account_access(account).ok_or_else(|| unavailable("Grok CLI access token is invalid."))
}

async fn egress(storage: &Storage, account: &Value) -> Result<Bound, Discovery> {
    let client = HTTP
        .as_ref()
        .ok_or_else(|| unavailable("Grok CLI model discovery failed: HTTP client unavailable."))?;
    provider_http::bind(storage, account, ENDPOINT, FIXTURE, client, TIMEOUT, None)
        .await
        .ok_or_else(|| {
            unavailable("Grok CLI model discovery failed: configured proxy is unavailable.")
        })
}

async fn fetch(delivery: &Bound, token: &str, details: &Value) -> Result<Response, Discovery> {
    let email = metadata(&details["email"]);
    let user_id = metadata(&details["userId"]).or_else(|| metadata(&details["principalId"]));
    let request = identity_headers(delivery.request(Method::GET), token, email, user_id)
        .header(header::ACCEPT, "application/json");
    let response = request
        .send()
        .await
        .map_err(|_| unavailable("Grok CLI model discovery failed: fetch failed"))?;
    if delivery.protected && response.status().is_redirection() {
        return Err(unavailable(
            "Grok CLI model discovery failed: proxy redirect refused.",
        ));
    }
    Ok(response)
}

fn unavailable(message: &str) -> Discovery {
    Discovery::Unavailable(message.into())
}

pub(super) fn expired(account: &Value) -> bool {
    due(account, 0)
}

pub(super) fn due(account: &Value, ahead_ms: i64) -> bool {
    let expiry = ["expiresAt", "tokenExpiresAt"]
        .into_iter()
        .find_map(|key| expiry_ms(&account[key]));
    expiry.is_some_and(|expires| expires <= Utc::now().timestamp_millis() + ahead_ms)
}

fn expiry_ms(raw: &Value) -> Option<i64> {
    let number = raw.as_i64().or_else(|| raw.as_str()?.parse().ok());
    if let Some(number) = number {
        return (number > 0).then(|| {
            if number < 100_000_000_000 {
                number.saturating_mul(1_000)
            } else {
                number
            }
        });
    }
    DateTime::parse_from_rfc3339(raw.as_str()?)
        .ok()
        .map(|time| time.timestamp_millis())
}

fn metadata(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 1024)
}

pub(super) fn secrets(account: &Value) -> Vec<&str> {
    let details = &account["providerSpecificData"];
    [
        "accessToken",
        "refreshToken",
        "idToken",
        "apiKey",
        "sessionToken",
        "email",
        "userId",
        "principalId",
    ]
    .into_iter()
    .flat_map(|key| [&account[key], &details[key]])
    .filter_map(Value::as_str)
    .filter(|value| !value.is_empty())
    .collect()
}

pub(super) fn redact(text: &str, credentials: &[&str]) -> String {
    credentials.iter().fold(text.to_owned(), |text, secret| {
        text.replace(secret, "[redacted]")
    })
}

fn scrub(payload: &mut Value, credentials: &[&str]) {
    match payload {
        Value::Object(fields) => {
            fields.retain(|key, _| {
                let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
                !matches!(
                    normalized.as_str(),
                    "authorization"
                        | "apikey"
                        | "accesstoken"
                        | "refreshtoken"
                        | "idtoken"
                        | "sessiontoken"
                        | "authtoken"
                        | "clientsecret"
                        | "privatekey"
                        | "secret"
                        | "token"
                        | "cookie"
                        | "password"
                        | "credentials"
                        | "email"
                        | "userid"
                        | "principalid"
                ) && !credentials.iter().any(|secret| key.contains(secret))
            });
            fields
                .values_mut()
                .for_each(|value| scrub(value, credentials));
        }
        Value::Array(rows) => rows.iter_mut().for_each(|value| scrub(value, credentials)),
        Value::String(text) => *text = redact(text, credentials),
        _ => (),
    }
}

async fn bytes(mut response: Response, limit: usize) -> Option<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|value| value > limit as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > limit - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(bytes)
}

fn first_nonnull<'a>(object: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names
        .iter()
        .find_map(|name| object.get(*name).filter(|value| !value.is_null()))
}

fn parse(payload: &Value) -> Vec<Value> {
    let value = if payload.is_array() {
        payload
    } else if let Some(value) = first_nonnull(payload, &["data", "models", "results"]) {
        value
    } else {
        return Vec::new();
    };
    let entries: Vec<_> = match value {
        Value::Array(rows) => rows.iter().map(|row| (None, row)).collect(),
        Value::Object(rows) => {
            let mut rows: Vec<_> = rows
                .iter()
                .map(|(key, row)| (Some(key.as_str()), row))
                .collect();
            rows.sort_by_key(|(key, _)| {
                key.and_then(|key| {
                    key.parse::<u32>()
                        .ok()
                        .filter(|index| index.to_string() == key)
                })
                .unwrap_or(u32::MAX)
            });
            rows
        }
        _ => return Vec::new(),
    };
    let mut ids = HashSet::new();
    let mut models = Vec::new();
    for (key, item) in entries {
        let synthetic;
        let item = if let Some(id) = item.as_str() {
            synthetic = json!({"id":id});
            &synthetic
        } else if item.is_object() {
            item
        } else {
            continue;
        };
        let id = first_nonnull(item, &["id", "model_id", "modelId", "model", "slug"])
            .map(js_text)
            .or_else(|| key.map(str::to_owned))
            .or_else(|| first_nonnull(item, &["name"]).map(js_text))
            .unwrap_or_default();
        let id = js_trim(&id);
        if id.is_empty() || !ids.insert(id.to_owned()) {
            continue;
        }
        let mut row: Map<_, _> = item.as_object().cloned().unwrap_or_default();
        row.insert("id".into(), json!(id));
        row.insert(
            "name".into(),
            first_nonnull(item, &["display_name", "displayName", "name"])
                .cloned()
                .unwrap_or_else(|| json!(id)),
        );
        for (destination, sources, default) in [
            (
                "contextLength",
                &[
                    "context_length",
                    "contextLength",
                    "context_window",
                    "contextWindow",
                ][..],
                500_000,
            ),
            (
                "maxOutputTokens",
                &["max_output_tokens", "maxOutputTokens"][..],
                64_000,
            ),
        ] {
            if let Some(value) = first_nonnull(item, sources).and_then(positive) {
                row.insert(destination.into(), value);
            } else if id == BUILD_MODEL && !row.get(destination).is_some_and(truthy) {
                row.insert(destination.into(), json!(default));
            }
        }
        models.push(Value::Object(row));
    }
    models
}

fn positive(value: &Value) -> Option<Value> {
    let value = crate::thinking_intent::numeric(value).or_else(|| {
        let string = js_text(value);
        let string = js_trim(&string);
        for (prefixes, radix) in [(["0x", "0X"], 16), (["0o", "0O"], 8), (["0b", "0B"], 2)] {
            if let Some(text) = prefixes
                .into_iter()
                .find_map(|prefix| string.strip_prefix(prefix))
            {
                return u64::from_str_radix(text, radix)
                    .ok()
                    .map(|value| value as f64);
            }
        }
        string
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
    })?;
    (value > 0.0).then(|| {
        if value.fract() == 0.0 && value < 9_007_199_254_740_992.0 {
            json!(value as i64)
        } else {
            json!(value)
        }
    })
}
