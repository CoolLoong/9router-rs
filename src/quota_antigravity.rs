use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::DateTime;
use reqwest::{Client, Method, Response, StatusCode};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::provider_defaults;
use crate::provider_http::{self, Bound, http_client};
use crate::provider_oauth::{self, RefreshError, RefreshMode};
use crate::proxy_transport::Route;
use crate::storage::Storage;

use super::{Failure, access, parse, proxy_unavailable};

const SUBSCRIPTION: &str = "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
const MODELS: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels";
const SUMMARY: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
const TIMEOUT: Duration = Duration::from_secs(10);
const CACHE_TTL: Duration = Duration::from_secs(180);
const CACHE_LIMIT: usize = 256;
const BODY_LIMIT: usize = 4 * 1024 * 1024;
const MODELS_WITH_QUOTAS: &[&str] = &[
    "gemini-4-carbon-high",
    "gemini-4-carbon-medium",
    "gemini-4-carbon-low",
    "gemini-4-argon-high",
    "gemini-4-argon-medium",
    "gemini-4-argon-low",
    "gemini-3.8-flash-high",
    "gemini-3.8-flash-medium",
    "gemini-3.8-flash-low",
    "gemini-3.7-flash-high",
    "gemini-3.7-flash-medium",
    "gemini-3.7-flash-low",
    "gemini-3.6-flash-high",
    "gemini-3.6-flash-medium",
    "gemini-3.6-flash-low",
    "gemini-3.1-pro-high",
    "gemini-pro-agent",
    "gemini-3.1-pro-low",
    "claude-sonnet-5-5-high",
    "claude-sonnet-5-5-medium",
    "claude-sonnet-5-5-low",
    "claude-sonnet-5-5",
    "claude-opus-5-5-high",
    "claude-opus-5-5-medium",
    "claude-opus-5-5-low",
    "claude-opus-5-5",
    "claude-sonnet-4-6",
    "claude-opus-4-6-thinking",
    "gpt-oss-120b-medium",
    "gemini-3.1-flash-image",
];
static CLIENT: LazyLock<Client> = LazyLock::new(|| {
    http_client(TIMEOUT)
        .build()
        .expect("Antigravity quota HTTP client")
});
static VERSION: LazyLock<&'static str> = LazyLock::new(|| {
    provider_defaults::text("shared", "antigravityIdeVersion")
        .expect("built-in Antigravity version")
});

type Quotas = Map<String, Value>;
type CacheSlot = Arc<AsyncMutex<Option<(Instant, Quotas)>>>;
type Cache = VecDeque<([u8; 32], CacheSlot)>;
static WEEKLY: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);

pub(crate) async fn read(storage: &Storage, observed: &Value) -> Result<Value, Failure> {
    let delivery = quota_binding(storage, observed, MODELS)
        .await
        .ok_or_else(proxy_unavailable)?;
    let relay = delivery.route.relay_identity();
    let mut private = Vec::from_iter(delivery.relay_secret().map(str::to_owned));
    let mut account = std::borrow::Cow::Borrowed(observed);
    if observed["refreshToken"]
        .as_str()
        .is_some_and(|token| !token.is_empty())
        && (access(observed).is_none()
            || provider_oauth::refresh_due_for_provider("antigravity", observed))
    {
        let mode = if let Some(identity) = relay {
            RefreshMode::RelayRouted(identity)
        } else {
            let endpoint =
                provider_oauth::test_token_url("antigravity").ok_or_else(proxy_unavailable)?;
            let token_route = provider_http::bind(
                storage, observed, endpoint, endpoint, &CLIENT, TIMEOUT, None,
            )
            .await
            .ok_or_else(proxy_unavailable)?;
            token_route.route.relay_identity().map_or(
                RefreshMode::Routed(token_route.protected),
                RefreshMode::RelayRouted,
            )
        };
        match provider_oauth::refresh_account(storage, observed, mode).await {
            Ok(updated) => account = std::borrow::Cow::Owned(updated),
            Err(RefreshError::AccountUnavailable) => {
                return Err(Failure::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Antigravity connection changed during quota renewal",
                ));
            }
            Err(_) => (),
        }
        if !quota_binding(storage, &account, MODELS)
            .await
            .is_some_and(|actual| delivery.same_route(&actual))
        {
            return Err(proxy_unavailable());
        }
    }
    let Some(token) = access(&account) else {
        return Ok(
            json!({"message":"Antigravity quota API authentication expired. Chat may still work.","quotas":{}}),
        );
    };
    let subscription = match quota_binding(storage, &account, SUBSCRIPTION).await {
        Some(route) if relay.is_none() || route.route.relay_identity() == relay => {
            private.extend(route.relay_secret().map(str::to_owned));
            match request(&route, SUBSCRIPTION, token, &json!({
            "metadata":{"ideType":9,"platform":crate::google_code_assist::platform(),"pluginType":2},"mode":1,
        })).await {
            Some(response) if response.status().is_success() => json_body(response).await.unwrap_or_default(),
            _ => Value::Null,
            }
        }
        _ => Value::Null,
    };
    let project = crate::google_code_assist::project_id(&subscription["cloudaicompanionProject"])
        .or_else(|| crate::google_code_assist::saved_project(&account));
    let body = project.map_or_else(|| json!({}), |project| json!({"project":project}));
    let Some(response) = request(&delivery, MODELS, token, &body).await else {
        return Ok(json!({"message":"Antigravity error: fetch failed"}));
    };
    let status = response.status();
    match status {
        StatusCode::UNAUTHORIZED => {
            return Ok(
                json!({"message":"Antigravity quota API authentication expired. Chat may still work.", "quotas":{}}),
            );
        }
        StatusCode::FORBIDDEN => {
            return Ok(
                json!({"message":"Antigravity quota API access forbidden. Chat may still work.", "quotas":{}}),
            );
        }
        status if !status.is_success() => {
            return Ok(
                json!({"message":format!("Antigravity error: Antigravity API error: {}", status.as_u16())}),
            );
        }
        _ => {}
    }
    let Some(data) = json_body(response).await else {
        return Ok(json!({"message":"Antigravity error: invalid provider response"}));
    };
    let paid = subscription["paidTier"]["id"]
        .as_str()
        .is_some_and(|tier| !tier.is_empty() && tier != "free-tier");
    let mut quotas = if paid {
        model_quotas(&data)
    } else {
        Quotas::new()
    };
    if let Some(route) = quota_binding(storage, &account, SUMMARY).await
        && (relay.is_none() || route.route.relay_identity() == relay)
    {
        private.extend(route.relay_secret().map(str::to_owned));
        let mut summary = cached_weekly(&route, token, &body).await;
        reconcile_sessions(&quotas, &mut summary);
        quotas.extend(summary);
    }
    let plan = subscription["currentTier"]["name"]
        .as_str()
        .filter(|plan| !plan.is_empty())
        .unwrap_or("Unknown");
    let mut result = json!({"plan":plan,"quotas":quotas,"subscriptionInfo":subscription});
    if let std::borrow::Cow::Owned(_) = account {
        redact(&mut result, observed);
    }
    redact(&mut result, &account);
    crate::provider_tools::scrub_payload(&mut result, &private);
    Ok(result)
}

async fn quota_binding(storage: &Storage, account: &Value, endpoint: &str) -> Option<Bound> {
    let (_, action) = endpoint.rsplit_once(':')?;
    let fixture = format!("http://127.0.0.1:52989/antigravity/v1internal:{action}");
    provider_http::bind(storage, account, endpoint, &fixture, &CLIENT, TIMEOUT, None).await
}

async fn request(delivery: &Bound, endpoint: &str, token: &str, body: &Value) -> Option<Response> {
    let mut request = delivery
        .request(Method::POST)
        .bearer_auth(token)
        .header(
            "user-agent",
            format!("antigravity/hub/{} darwin/arm64", *VERSION),
        )
        .json(body);
    if endpoint != SUBSCRIPTION {
        request = request
            .header("x-client-name", "antigravity")
            .header("x-client-version", *VERSION);
    }
    request.send().await.ok()
}

async fn json_body(mut response: Response) -> Option<Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > BODY_LIMIT - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes)).ok()
}

fn entry(fraction: f64, reset: &Value, label: &str, floor: bool) -> Value {
    let used = 1000.0 - (1000.0 * fraction + 0.5).floor();
    let used = if floor { used.max(0.0) } else { used };
    json!({"used":parse::number(used),"total":1000,"resetAt":parse::reset_date(reset,true),
        "remainingPercentage":parse::number(fraction*100.0),"unlimited":false,"displayName":label})
}

fn model_quotas(data: &Value) -> Quotas {
    data["models"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(id, info)| {
            MODELS_WITH_QUOTAS.contains(&id.as_str())
                && !crate::provider_connections::truthy(&info["isInternal"])
                && info["quotaInfo"].is_object()
        })
        .map(|(id, info)| {
            let quota = &info["quotaInfo"];
            let fraction = crate::thinking_intent::numeric(&quota["remainingFraction"])
                .unwrap_or_else(|| parse::finite(&quota["remainingFraction"]));
            let label = info["displayName"]
                .as_str()
                .filter(|name| !name.is_empty())
                .unwrap_or(id);
            (
                id.clone(),
                entry(fraction, &quota["resetTime"], label, false),
            )
        })
        .collect()
}

fn parse_weekly(data: &Value) -> Quotas {
    let mut quotas = Quotas::new();
    let groups = data["groups"]
        .as_array()
        .or_else(|| data["quotaSummary"]["groups"].as_array());
    for group in groups.into_iter().flatten() {
        let name = group["displayName"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let (family, title) = if name.contains("argon") {
            ("gemini_argon", "Gemini 4 Argon")
        } else if name.contains("gemini") {
            ("gemini", "Gemini")
        } else if name.contains("claude") || name.contains("gpt") {
            ("claude_gpt", "Claude & GPT")
        } else {
            continue;
        };
        for bucket in group["buckets"].as_array().into_iter().flatten() {
            let window = bucket["window"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let text = format!(
                "{} {}",
                bucket["bucketId"].as_str().unwrap_or_default(),
                bucket["displayName"].as_str().unwrap_or_default()
            )
            .to_ascii_lowercase();
            let weekly = window == "weekly" || text.contains("weekly");
            let session = matches!(window.as_str(), "5h" | "daily")
                || ["five hour", "5h", "daily"]
                    .iter()
                    .any(|term| text.contains(term));
            let disabled = bucket["disabled"] == true;
            if (!weekly && !session) || (disabled && weekly) {
                continue;
            }
            let fraction = if disabled {
                Some(0.0)
            } else {
                bucket.get("remainingFraction").and_then(|value| {
                    crate::thinking_intent::numeric(value).or_else(|| parse::finite_number(value))
                })
            };
            let Some(fraction) = fraction else {
                continue;
            };
            let (key, label) = if weekly {
                ("weekly", "Weekly")
            } else {
                ("session", "5h")
            };
            quotas.entry(format!("{family}_{key}")).or_insert_with(|| {
                entry(
                    fraction,
                    &bucket["resetTime"],
                    &format!("{title} ({label})"),
                    true,
                )
            });
        }
    }
    quotas
}

async fn cached_weekly(route: &Bound, token: &str, body: &Value) -> Quotas {
    let mut hash = Sha256::new();
    hash.update(token);
    hash.update([0]);
    hash.update(body.to_string());
    match &route.route {
        Route::Direct if route.protected => hash.update(b"bypass"),
        Route::Proxy { url, .. } => hash.update(url),
        Route::Relay { .. } => hash.update(route.route.relay_identity().unwrap_or_default()),
        _ => hash.update(b"direct"),
    }
    let key: [u8; 32] = hash.finalize().into();
    let slot = {
        let mut cache = WEEKLY.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(index) = cache.iter().position(|(hash, _)| *hash == key) {
            let entry = cache.remove(index).expect("Antigravity quota cache entry");
            let slot = entry.1.clone();
            cache.push_back(entry);
            slot
        } else {
            let slot = Arc::new(AsyncMutex::new(None));
            if cache.len() == CACHE_LIMIT {
                cache.pop_front();
            }
            cache.push_back((key, slot.clone()));
            slot
        }
    };
    let mut cached = slot.lock().await;
    if let Some((_, quotas)) = cached
        .as_ref()
        .filter(|(instant, _)| instant.elapsed() < CACHE_TTL)
    {
        return quotas.clone();
    }
    let quotas = match request(route, SUMMARY, token, body).await {
        Some(response) if response.status().is_success() => json_body(response)
            .await
            .as_ref()
            .map(parse_weekly)
            .unwrap_or_default(),
        _ => Quotas::new(),
    };
    if !quotas.is_empty() {
        *cached = Some((Instant::now(), quotas.clone()));
    }
    quotas
}

fn reconcile_sessions(models: &Quotas, summary: &mut Quotas) {
    for (session, prefix) in [
        ("gemini_argon_session", "gemini-"),
        ("gemini_session", "gemini-"),
        ("claude_gpt_session", "claude-"),
    ] {
        let Some(row) = summary
            .get_mut(session)
            .filter(|row| parse::finite(&row["remainingPercentage"]) > 0.0)
        else {
            continue;
        };
        let mut family = models
            .iter()
            .filter(|(id, _)| {
                id.starts_with(prefix)
                    && match session {
                        "gemini_argon_session" => id.contains("argon"),
                        "gemini_session" => !id.contains("argon") && !id.contains("image"),
                        _ => true,
                    }
            })
            .peekable();
        if family.peek().is_none()
            || family
                .clone()
                .any(|(_, item)| parse::finite(&item["remainingPercentage"]) != 0.0)
        {
            continue;
        }
        row["used"] = row["total"].clone();
        row["remainingPercentage"] = json!(0);
        if let Some((reset, _)) = family
            .filter_map(|(_, item)| {
                let reset = item["resetAt"].as_str()?;
                Some((reset, DateTime::parse_from_rfc3339(reset).ok()?))
            })
            .max_by_key(|(_, date)| *date)
        {
            row["resetAt"] = json!(reset);
        }
    }
}

fn redact(value: &mut Value, account: &Value) {
    match value {
        Value::String(text) => {
            for field in ["accessToken", "refreshToken", "idToken"] {
                if let Some(secret) = account[field].as_str().filter(|secret| !secret.is_empty()) {
                    *text = text.replace(secret, "[redacted]");
                }
            }
        }
        Value::Object(fields) => fields.values_mut().for_each(|value| redact(value, account)),
        Value::Array(items) => items.iter_mut().for_each(|value| redact(value, account)),
        _ => {}
    }
}
