use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use rusqlite::{TransactionBehavior, params};
use serde_json::{Value, json};
use uuid::Uuid;

use super::flow::{ApiError, Credentials, Provider};
use crate::{provider_connections as connections, storage::Storage};

pub(super) async fn save(
    storage: &Storage,
    provider: Provider,
    Credentials(mut fields): Credentials,
) -> Result<Value, ApiError> {
    let now = Utc::now();
    let expires_at = expiry(now, fields.get("expiresIn"))?;
    fields.insert("expiresAt".into(), expires_at);
    save_connection(storage, provider.id(), Some(provider), fields, now).await
}

pub(super) async fn save_connection(
    storage: &Storage,
    provider_id: &'static str,
    provider: Option<Provider>,
    mut fields: serde_json::Map<String, Value>,
    now: DateTime<Utc>,
) -> Result<Value, ApiError> {
    let now = now.to_rfc3339_opts(SecondsFormat::Millis, true);
    fields.insert("testStatus".into(), json!("active"));
    let imported_type = provider
        .is_none()
        .then(|| fields.shift_remove("authType"))
        .flatten();
    let auth_type = match imported_type.as_ref().and_then(Value::as_str) {
        Some("cookie") => "cookie",
        Some("api_key") => "api_key",
        _ => "oauth",
    };
    storage
        .run(true, move |database| {
            let tx = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let pool = connections::read_all(&tx, Some(provider_id))?;
            let existing = fields
                .get("email")
                .and_then(Value::as_str)
                .filter(|email| auth_type == "oauth" && !email.is_empty())
                .and_then(|email| {
                    pool.iter().find(|conn| {
                        conn["authType"] == "oauth"
                            && conn["email"] == email
                            && same_login(provider_id, conn, fields.get("providerSpecificData"))
                    })
                })
                .or_else(|| {
                    if provider_id != "zed" || auth_type != "oauth" {
                        return None;
                    }
                    let details = fields.get("providerSpecificData")?;
                    let user_id = details.get("userId").and_then(Value::as_str)?;
                    let token = fields.get("accessToken").and_then(Value::as_str)?;
                    if details["authMethod"] != "oauth" || user_id.is_empty() || token.is_empty() {
                        return None;
                    }
                    pool.iter().find(|conn| {
                        conn["authType"] == "oauth"
                            && conn["providerSpecificData"]["userId"] == user_id
                            && conn["accessToken"] == token
                    })
                });

            let connection = if let Some(existing) = existing {
                let mut existing = existing.clone();
                let existing_fields = existing.as_object_mut().expect("connection object");
                if let Some(details) = fields
                    .get_mut("providerSpecificData")
                    .and_then(Value::as_object_mut)
                    && (provider == Some(Provider::GrokCli)
                        || provider_id == "zed"
                            && details
                                .get("authMethod")
                                .is_some_and(|kind| kind == "oauth"))
                    && let Some(saved) = existing_fields
                        .get("providerSpecificData")
                        .and_then(Value::as_object)
                {
                    let mut merged = saved.clone();
                    merged.extend(std::mem::take(details));
                    *details = merged;
                }
                if provider.is_some() {
                    for optional in ["refreshToken", "expiresIn"] {
                        if !fields.contains_key(optional) {
                            existing_fields.shift_remove(optional);
                        }
                    }
                }
                match provider {
                    Some(Provider::Claude | Provider::Gemini | Provider::Antigravity)
                        if !fields.contains_key("scope") =>
                    {
                        existing_fields.shift_remove("scope");
                    }
                    Some(Provider::Codex) if !fields.contains_key("idToken") => {
                        existing_fields.shift_remove("idToken");
                    }
                    Some(Provider::GitHub) => {
                        if !fields.contains_key("displayName") {
                            existing_fields.shift_remove("displayName");
                        }
                        fields.entry("name").or_insert(Value::Null);
                    }
                    _ => (),
                }
                connections::patch(existing, fields, &now)
            } else {
                let name = fields
                    .get("name")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        (auth_type != "api_key")
                            .then(|| fields.get("email").and_then(Value::as_str))
                            .flatten()
                    })
                    .filter(|name| !name.is_empty())
                    .map_or_else(
                        || {
                            if auth_type == "api_key" {
                                Value::Null
                            } else {
                                json!(format!("Account {}", pool.len() + 1))
                            }
                        },
                        |name| json!(name),
                    );
                let priority = tx.query_row(
                "SELECT COALESCE(MAX(priority),0) + 1 FROM providerConnections WHERE provider = ?",
                params![provider_id], |row| row.get::<_, i64>(0),
            )?;
                let email = fields.shift_remove("email");
                fields.retain(|_, value| !value.is_null());
                if let Some(email) = email {
                    fields.insert("email".into(), email);
                }
                fields.extend([
                    ("id".into(), json!(Uuid::new_v4().to_string())),
                    ("provider".into(), json!(provider_id)),
                    ("authType".into(), json!(auth_type)),
                    ("name".into(), name),
                    ("priority".into(), json!(priority)),
                    ("isActive".into(), json!(true)),
                    ("createdAt".into(), json!(now)),
                    ("updatedAt".into(), json!(now)),
                ]);
                Value::Object(fields)
            };
            connections::write(&tx, &connection)?;
            tx.commit()?;
            Ok(connection)
        })
        .await
        .map_err(|_| ApiError::failed("Could not save the provider connection"))
}

fn same_login(provider: &str, current: &Value, incoming: Option<&Value>) -> bool {
    let incoming = incoming.unwrap_or(&Value::Null);
    let existing = &current["providerSpecificData"];
    let identity = |key| match (
        existing.get(key).filter(|value| connections::truthy(value)),
        incoming.get(key).filter(|value| connections::truthy(value)),
    ) {
        (Some(a), Some(b)) => Some(a == b),
        (None, None) => None,
        _ => Some(false),
    };
    let workspace = identity("chatgptAccountId");
    if provider == "codex" {
        return workspace == Some(true);
    }
    if provider == "gitlab" {
        let existing_base = existing["baseUrl"]
            .as_str()
            .unwrap_or("https://gitlab.com")
            .trim_end_matches('/');
        if incoming["baseUrl"]
            .as_str()
            .is_some_and(|base| base.trim_end_matches('/') != existing_base)
        {
            return false;
        }
    }
    if provider == "zed" {
        return identity("userId").unwrap_or(true);
    }
    workspace.unwrap_or_else(|| identity("username").unwrap_or(true))
}

pub(super) fn expiry(now: DateTime<Utc>, expires_in: Option<&Value>) -> Result<Value, ApiError> {
    let Some(delta) = expires_in
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str()?.parse::<i64>().ok())
        })
        .filter(|delta| *delta != 0)
    else {
        return Ok(Value::Null);
    };
    let expires = TimeDelta::try_seconds(delta)
        .and_then(|delta| now.checked_add_signed(delta))
        .ok_or_else(|| ApiError::failed("Invalid token expiry"))?;
    Ok(json!(expires.to_rfc3339_opts(SecondsFormat::Millis, true)))
}
