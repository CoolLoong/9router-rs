use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Map, Value, json};

use crate::chat_accounts::date_ms;
use crate::chat_output::js_text;
use crate::cli_video::redact_bearer;
use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::provider_connections::{self, truthy};
use crate::storage::Storage;

const LOCK_PREFIX: &str = "modelLock_";
const DATE_LIMIT: u64 = 8_640_000_000_000_000;
const BODY_LIMIT: usize = 128 * 1024 * 1024;

#[derive(Clone)]
struct AvailabilityState {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    let state = AvailabilityState { storage };
    Router::new()
        .route("/api/models/availability", get(availability).post(clear))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(
    State(state): State<AvailabilityState>,
    request: Request,
    next: Next,
) -> Response {
    if dashboard_session::authenticated(&state.storage, request.headers()).await {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        axum::Json(json!({"error":"Unauthorized"})),
    )
        .into_response()
}

fn reply(status: StatusCode, value: Value) -> Response {
    into_http(Ok(
        json!({"status":status.as_u16(),"headers":{"content-type":"application/json"},"body":value.to_string()}),
    ))
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}))
}

async fn availability(State(state): State<AvailabilityState>) -> Response {
    match state.storage.run(false, read_models).await {
        Ok(models) => reply(
            StatusCode::OK,
            json!({"models":models,"unavailableCount":models.len()}),
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch model availability",
        ),
    }
}

fn read_models(db: &mut Connection) -> rusqlite::Result<Vec<Value>> {
    let mut models = Vec::new();
    let now = chrono::Utc::now().timestamp_millis();
    for connection in provider_connections::read_all(db, None)? {
        let Some(fields) = connection.as_object() else {
            continue;
        };
        let name = [&connection["name"], &connection["email"], &connection["id"]]
            .into_iter()
            .find(|value| truthy(value))
            .unwrap_or(&Value::Null);
        let last_error = safe_error(&connection);
        let mut has_cooldown = false;
        for (key, until) in fields {
            let Some(model) = key.strip_prefix(LOCK_PREFIX) else {
                continue;
            };
            if !truthy(until)
                || !date_ms(until)
                    .is_some_and(|expiry| expiry > now && expiry.unsigned_abs() <= DATE_LIMIT)
            {
                continue;
            }
            has_cooldown = true;
            let model = if model.is_empty() { "__all" } else { model };
            models.push(json!({
                "provider":connection["provider"], "model":model, "status":"cooldown", "until":until,
                "connectionId":connection["id"], "connectionName":name, "lastError":last_error,
            }));
        }
        if !has_cooldown && connection["testStatus"] == "unavailable" {
            models.push(json!({
                "provider":connection["provider"], "model":"__all", "status":"unavailable",
                "connectionId":connection["id"], "connectionName":name, "lastError":last_error,
            }));
        }
    }
    Ok(models)
}

fn safe_error(connection: &Value) -> Value {
    let value = &connection["lastError"];
    if !truthy(value) {
        return Value::Null;
    }
    let Some(message) = value.as_str() else {
        return value.clone();
    };
    let safe = ["apiKey", "accessToken", "refreshToken", "idToken"]
        .into_iter()
        .filter_map(|field| connection[field].as_str())
        .filter(|token| token.encode_utf16().count() >= 8)
        .fold(redact_bearer(message), |safe, token| {
            safe.replace(token, "[redacted]")
        });
    json!(safe)
}

async fn clear(State(state): State<AvailabilityState>, request: Request) -> Response {
    let failed = || {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to clear cooldown",
        )
    };
    let Ok(bytes) = to_bytes(request.into_body(), BODY_LIMIT).await else {
        return failed();
    };
    let text = String::from_utf8_lossy(&bytes);
    let Ok(value) = serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(&text))
    else {
        return failed();
    };
    if value.is_null() {
        return failed();
    }
    if value["action"] != "clearCooldown" || !truthy(&value["provider"]) || !truthy(&value["model"])
    {
        return error(StatusCode::BAD_REQUEST, "Invalid request");
    }
    let provider = match &value["provider"] {
        Value::String(provider) => ProviderFilter::Text(provider.clone()),
        Value::Number(number) => {
            let Some(number) = number.as_f64() else {
                return failed();
            };
            ProviderFilter::Number(number)
        }
        Value::Object(fields) if fields.is_empty() => {
            return reply(StatusCode::OK, json!({"ok":true}));
        }
        Value::Array(items) if items.is_empty() => {
            return reply(StatusCode::OK, json!({"ok":true}));
        }
        _ => return failed(),
    };
    let lock = format!("{LOCK_PREFIX}{}", js_text(&value["model"]));
    match state
        .storage
        .run(true, move |db| clear_cooldown(db, provider, &lock))
        .await
    {
        Ok(()) => reply(StatusCode::OK, json!({"ok":true})),
        Err(_) => failed(),
    }
}

enum ProviderFilter {
    Text(String),
    Number(f64),
}

fn clear_cooldown(
    db: &mut Connection,
    provider: ProviderFilter,
    lock: &str,
) -> rusqlite::Result<()> {
    let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let provider = match provider {
        ProviderFilter::Text(text) => text,
        ProviderFilter::Number(number) => {
            transaction.query_row("SELECT CAST(?1 AS TEXT)", [number], |row| row.get(0))?
        }
    };
    for connection in provider_connections::read_all(&transaction, Some(&provider))? {
        if !truthy(&connection[lock]) {
            continue;
        }
        let mut fields = Map::from_iter([(lock.to_owned(), Value::Null)]);
        if connection["testStatus"] == "unavailable" {
            fields.insert("testStatus".to_owned(), json!("active"));
        }
        let updated = provider_connections::patch(connection, fields, &provider_connections::now());
        provider_connections::write(&transaction, &updated)?;
    }
    transaction.commit()
}
