use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Map, Value, json};

use chat_account_rules_rs::port::check_fallback_error;

use crate::provider_connections::{self as connections, object, truthy};
use crate::storage::Storage;

const MODEL_LOCK_PREFIX: &str = "modelLock_";
const ACCOUNT_LOCK: &str = "modelLock___all";
const MAX_RATE_LIMIT_COOLDOWN_MS: i64 = 30 * 60 * 1000;

pub enum Selection {
    Ready(Value),
    CoolingDown(Retry),
    Empty,
}

pub struct Retry {
    pub retry_after_seconds: u64,
    pub retry_after_human: String,
    pub last_error: Option<String>,
    pub last_error_code: Option<u16>,
}

pub struct Fallback {
    pub should_fallback: bool,
}

struct SelectionMode {
    round_robin: bool,
    sticky_limit: f64,
}

impl SelectionMode {
    fn from_settings(settings: &Value, provider: &str) -> Self {
        let overrides = &settings["providerStrategies"][provider];
        let value = |name: &str| {
            overrides
                .get(name)
                .filter(|value| truthy(value))
                .or_else(|| settings.get(name).filter(|value| truthy(value)))
        };
        Self {
            round_robin: value("fallbackStrategy").and_then(Value::as_str) == Some("round-robin"),
            sticky_limit: value("stickyRoundRobinLimit").map_or(3.0, numeric_value),
        }
    }
}

pub async fn active_connections(storage: &Storage, provider: &str) -> Result<Vec<Value>, String> {
    let provider = provider.to_owned();
    storage
        .run(false, move |db| read_active(db, &provider))
        .await
}

fn read_active(db: &Connection, provider: &str) -> rusqlite::Result<Vec<Value>> {
    let mut connections = connections::read_all(db, Some(provider))?;
    connections.retain(|connection| connection["isActive"] == true);
    Ok(connections)
}

pub async fn select(
    storage: &Storage,
    provider: &str,
    model: &str,
    excluded: &HashSet<String>,
    preferred_id: Option<&str>,
) -> Result<Selection, String> {
    select_for_request(storage, provider, model, model, excluded, preferred_id).await
}

pub(super) async fn select_for_request(
    storage: &Storage,
    provider: &str,
    model: &str,
    requested_model: &str,
    excluded: &HashSet<String>,
    preferred_id: Option<&str>,
) -> Result<Selection, String> {
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(|| "Could not read account-selection settings".to_owned())?;
    let mode = SelectionMode::from_settings(&settings, provider);
    let provider = provider.to_owned();
    let model = model.to_owned();
    let requested_model = requested_model.to_owned();
    let root = storage.root().to_owned();
    let excluded = excluded.clone();
    let preferred_id = preferred_id.map(str::to_owned);
    storage
        .run(mode.round_robin, move |db| {
            if mode.round_robin {
                let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let selection = read_selection(
                    &tx,
                    &provider,
                    &model,
                    &requested_model,
                    &root,
                    &excluded,
                    preferred_id.as_deref(),
                    mode,
                )?;
                tx.commit()?;
                Ok(selection)
            } else {
                read_selection(
                    db,
                    &provider,
                    &model,
                    &requested_model,
                    &root,
                    &excluded,
                    preferred_id.as_deref(),
                    mode,
                )
            }
        })
        .await
}

fn model_lock_key(model: &str) -> String {
    if model.is_empty() {
        ACCOUNT_LOCK.to_owned()
    } else {
        format!("{MODEL_LOCK_PREFIX}{model}")
    }
}

fn lock_active(connection: &Value, key: &str, now_ms: i64) -> bool {
    connection
        .get(key)
        .filter(|value| truthy(value))
        .or_else(|| connection.get(ACCOUNT_LOCK))
        .and_then(date_ms)
        .is_some_and(|expiration| expiration > now_ms)
}

fn read_selection(
    db: &Connection,
    provider: &str,
    model: &str,
    requested_model: &str,
    root: &Path,
    excluded: &HashSet<String>,
    preferred_id: Option<&str>,
    mode: SelectionMode,
) -> rusqlite::Result<Selection> {
    let all = read_active(db, provider)?;
    let now_ms = Utc::now().timestamp_millis();
    let key = model_lock_key(model);
    let mut quota_expiry = None;
    let available: Vec<_> = all
        .iter()
        .filter(|connection| {
            let quota = if provider == "antigravity" {
                connection["id"].as_str().and_then(|id| {
                    crate::antigravity_quota_cache::blocked_until(root, id, model, now_ms)
                })
            } else {
                None
            };
            if let Some(expiry) = quota {
                quota_expiry = Some(quota_expiry.map_or(expiry, |old: i64| old.min(expiry)));
            }
            connection["id"]
                .as_str()
                .is_some_and(|id| !excluded.contains(id))
                && !lock_active(connection, &key, now_ms)
                && quota.is_none()
                && (provider != "codex"
                    || connection["providerSpecificData"]["enabledModels"]
                        .as_array()
                        .is_none_or(|models| {
                            models.is_empty()
                                || models.iter().any(|enabled| enabled == requested_model)
                        }))
        })
        .collect();

    if let Some(preferred) = preferred_id.and_then(|id| {
        available
            .iter()
            .copied()
            .find(|connection| connection["id"] == id)
    }) {
        return Ok(Selection::Ready(preferred.clone()));
    }
    if available.is_empty() {
        return Ok(retry_data(&all, &key, now_ms, quota_expiry)
            .map_or(Selection::Empty, Selection::CoolingDown));
    }
    if !mode.round_robin {
        return Ok(Selection::Ready(available[0].clone()));
    }

    let current = available
        .iter()
        .copied()
        .min_by(|a, b| compare_usage(a, b, true));
    let keep = current.filter(|connection| {
        truthy(&connection["lastUsedAt"]) && count_before_increment(connection) < mode.sticky_limit
    });
    let chosen = keep.or_else(|| {
        available
            .iter()
            .copied()
            .min_by(|a, b| compare_usage(a, b, false))
    });
    let Some(chosen) = chosen else {
        return Ok(Selection::Empty);
    };
    let count = if keep.is_some() {
        increment_usage(chosen)
    } else {
        json!(1)
    };
    let now = connections::now();
    let patch = object(json!({ "lastUsedAt": now, "consecutiveUseCount": count }));
    let updated = connections::patch(chosen.clone(), patch, &now);
    connections::write(db, &updated)?;
    Ok(Selection::Ready(chosen.clone()))
}

fn compare_usage(a: &Value, b: &Value, newest_first: bool) -> Ordering {
    let a = &a["lastUsedAt"];
    let b = &b["lastUsedAt"];
    let oldest_first = match (truthy(a), truthy(b)) {
        (true, true) => match (date_ms(a), date_ms(b)) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => Ordering::Equal,
        },
        (false, true) => Ordering::Less,
        (true, false) => Ordering::Greater,
        (false, false) => Ordering::Equal,
    };
    if newest_first {
        oldest_first.reverse()
    } else {
        oldest_first
    }
}

fn count_before_increment(connection: &Value) -> f64 {
    let count = &connection["consecutiveUseCount"];
    if truthy(count) {
        numeric_value(count)
    } else {
        0.0
    }
}

fn increment_usage(connection: &Value) -> Value {
    if let Some(count) = connection["consecutiveUseCount"]
        .as_str()
        .filter(|count| !count.is_empty())
    {
        return json!(format!("{count}1"));
    }
    let next = count_before_increment(connection) + 1.0;
    if next.fract() == 0.0 && next.is_finite() && next.abs() < 9_007_199_254_740_992.0 {
        json!(next as i64)
    } else {
        json!(next)
    }
}

fn numeric_value(value: &Value) -> f64 {
    match value {
        Value::Null | Value::Bool(false) => 0.0,
        Value::Bool(true) => 1.0,
        Value::Number(value) => value.as_f64().unwrap_or(f64::NAN),
        Value::String(value) if value.trim().is_empty() => 0.0,
        Value::String(value) => value.trim().parse().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

pub(super) fn date_ms(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        return DateTime::parse_from_rfc3339(text)
            .or_else(|_| DateTime::parse_from_rfc2822(text))
            .map(|time| time.timestamp_millis())
            .ok()
            .or_else(|| {
                NaiveDate::parse_from_str(text, "%Y-%m-%d")
                    .ok()?
                    .and_hms_opt(0, 0, 0)
                    .map(|time| time.and_utc().timestamp_millis())
            });
    }
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|ms| ms as i64))
}

fn iso(ms: i64) -> Option<String> {
    Some(DateTime::from_timestamp_millis(ms)?.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn retry_data(
    connections: &[Value],
    key: &str,
    now_ms: i64,
    quota_expiry: Option<i64>,
) -> Option<Retry> {
    let mut locked = connections
        .iter()
        .filter(|connection| lock_active(connection, key, now_ms));
    let first = locked.next();
    let expiry = first
        .into_iter()
        .chain(locked)
        .filter_map(Value::as_object)
        .flat_map(|fields| fields.iter())
        .filter(|(field, value)| field.starts_with(MODEL_LOCK_PREFIX) && truthy(value))
        .filter_map(|(_, value)| date_ms(value))
        .filter(|expiry| *expiry > now_ms)
        .chain(quota_expiry)
        .min()?;
    let retry_after_seconds = expiry.abs_diff(now_ms).div_ceil(1000);
    let last_error = first
        .and_then(|connection| connection["lastError"].as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let last_error_code = first.and_then(|connection| {
        connection["errorCode"]
            .as_u64()
            .and_then(|value| u16::try_from(value).ok())
            .or_else(|| connection["errorCode"].as_str()?.parse().ok())
    });
    Some(Retry {
        retry_after_seconds,
        retry_after_human: format_delay(retry_after_seconds),
        last_error,
        last_error_code,
    })
}

pub(crate) fn format_delay(seconds: u64) -> String {
    let mut parts = Vec::new();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours != 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes != 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds != 0 || parts.is_empty() {
        parts.push(format!("{seconds}s"));
    }
    format!("reset after {}", parts.join(" "))
}

pub async fn mark_unavailable(
    storage: &Storage,
    provider: &str,
    connection_id: &str,
    model: &str,
    status: u16,
    message: &str,
    resets_at_ms: Option<i64>,
) -> Result<Fallback, String> {
    if connection_id.is_empty() || connection_id == "noauth" {
        return Ok(Fallback {
            should_fallback: false,
        });
    }
    let provider = provider.to_owned();
    let connection_id = connection_id.to_owned();
    let key = model_lock_key(model);
    let message = message.to_owned();
    storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = connections::by_id(&tx, &connection_id)?;
            let backoff = current
                .as_ref()
                .filter(|conn| conn["provider"] == provider)
                .and_then(|conn| conn.get("backoffLevel").filter(|level| truthy(level)))
                .cloned()
                .unwrap_or_else(|| json!(0));
            let now_ms = Utc::now().timestamp_millis();
            let rule = if let Some(reset) = resets_at_ms.filter(|reset| *reset > now_ms) {
                let ms = if provider == "github" && key == ACCOUNT_LOCK && status == 402 {
                    reset - now_ms
                } else {
                    (reset - now_ms).min(MAX_RATE_LIMIT_COOLDOWN_MS)
                };
                json!({ "shouldFallback": true, "cooldownMs": ms, "newBackoffLevel": 0 })
            } else {
                check_fallback_error(
                    json!(status),
                    json!(message),
                    Some(numeric_value(&backoff)),
                    Some(json!(provider)),
                )
                .map_err(conversion_error)?
            };
            if rule["shouldFallback"] != true {
                return Ok(Fallback {
                    should_fallback: false,
                });
            }
            let cooldown = rule["cooldownMs"]
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .ok_or_else(|| conversion_error("Invalid cooldown".into()))?;
            if let Some(current) = current {
                let until = iso(now_ms.saturating_add(cooldown as i64))
                    .ok_or_else(|| conversion_error("Invalid model lock expiration".into()))?;
                let reason: Vec<u16> = message.encode_utf16().take(200).collect();
                let reason = String::from_utf16_lossy(&reason);
                let mut patch = Map::new();
                patch.insert(key, json!(until));
                patch.extend(object(json!({
                    "testStatus": "unavailable",
                    "lastError": reason,
                    "errorCode": status,
                    "lastErrorAt": iso(now_ms),
                    "backoffLevel": rule.get("newBackoffLevel").cloned().unwrap_or(backoff),
                })));
                let updated = connections::patch(current, patch, &connections::now());
                connections::write(&tx, &updated)?;
            }
            tx.commit()?;
            Ok(Fallback {
                should_fallback: true,
            })
        })
        .await
}

pub async fn mark_success(
    storage: &Storage,
    connection_id: &str,
    model: &str,
) -> Result<(), String> {
    if connection_id.is_empty() || connection_id == "noauth" {
        return Ok(());
    }
    let connection_id = connection_id.to_owned();
    let model = model.to_owned();
    storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(current) = connections::by_id(&tx, &connection_id)?
                && let Some(patch) = success_patch(&current, &model, Utc::now().timestamp_millis())
            {
                let updated = connections::patch(current, patch, &connections::now());
                connections::write(&tx, &updated)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

fn success_patch(connection: &Value, model: &str, now_ms: i64) -> Option<Map<String, Value>> {
    let fields = connection.as_object()?;
    let locks: Vec<_> = fields
        .iter()
        .filter(|(key, _)| key.starts_with(MODEL_LOCK_PREFIX))
        .collect();
    let status = &connection["testStatus"];
    let last_error = &connection["lastError"];
    if !truthy(status) && !truthy(last_error) && locks.is_empty() {
        return None;
    }

    let model_key = model_lock_key(model);
    let mut patch = Map::new();
    let mut locked = false;
    for (key, value) in locks {
        let expiry = date_ms(value).filter(|_| truthy(value));
        if (!model.is_empty() && (key == &model_key || key == ACCOUNT_LOCK))
            || expiry.is_some_and(|ms| ms <= now_ms)
        {
            patch.insert(key.clone(), Value::Null);
        } else if expiry.is_some_and(|ms| ms > now_ms) {
            locked = true;
        }
    }

    if patch.is_empty() && status != "unavailable" && !truthy(last_error) {
        return None;
    }
    if !locked {
        patch.extend(object(json!({
            "testStatus": "active",
            "lastError": null,
            "errorCode": null,
            "lastErrorAt": null,
            "backoffLevel": 0,
        })));
    }
    Some(patch)
}

fn conversion_error(message: String) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(message)))
}
