use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value, json};
use std::sync::OnceLock;

use crate::chat_output::js_text;
use crate::dashboard_session::{self, compare_password, hash_password};
use crate::observed_response::into_http;
use crate::provider_connections::{object, truthy};
use crate::provider_index::js_trim;
use crate::storage::Storage;

const BODY_LIMIT: usize = 128 * 1024 * 1024;
static DEFAULTS: OnceLock<Map<String, Value>> = OnceLock::new();

#[derive(Clone)]
struct SettingsState {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    let state = SettingsState { storage };
    Router::new()
        .route("/api/settings", get(read).patch(update))
        .route(
            "/api/settings/provider-proxy/{id}",
            axum::routing::patch(update_public_proxy),
        )
        .route(
            "/api/settings/proxy-test",
            axum::routing::post(crate::proxy_pools::settings_test),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .route("/api/settings/require-login", get(require_login))
        .route(
            "/api/auth/reset-password",
            axum::routing::post(reset_password),
        )
        .with_state(state)
}

async fn reset_password(State(state): State<SettingsState>, request: Request) -> Response {
    let cli = crate::catalog_guard::valid_cli_token(&state.storage, request.headers());
    if !cli
        && !(crate::catalog_guard::is_direct_loopback(&request)
            && dashboard_session::authenticated(&state.storage, request.headers()).await)
    {
        return reply(
            StatusCode::FORBIDDEN,
            json!({"error":"Local only: CLI token required"}),
            true,
        );
    }
    let result = state
        .storage
        .run(true, |db| {
            let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut raw = read_raw(&transaction)?;
            raw.insert("password".into(), Value::Null);
            write_raw(&transaction, &Value::Object(raw))?;
            transaction.commit()
        })
        .await;
    match result {
        Ok(()) => reply(StatusCode::OK, json!({"success":true}), true),
        Err(_) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"Failed to reset dashboard password"}),
            true,
        ),
    }
}

fn defaults() -> &'static Map<String, Value> {
    DEFAULTS.get_or_init(|| {
        let mut values: Map<String, Value> =
            serde_json::from_str(include_str!("settings-defaults.json"))
                .expect("generated settings defaults");
        if let Ok(url) = std::env::var("HEADROOM_URL")
            && !url.is_empty()
        {
            values.insert("headroomUrl".into(), json!(url));
        }
        values
    })
}

pub(crate) fn effective(raw: Value) -> Value {
    let mut settings = defaults().clone();
    settings.extend(object(raw));
    if let Some(adapters) = settings
        .get_mut("capacityAdapter")
        .and_then(Value::as_object_mut)
    {
        for entry in adapters.values_mut() {
            if let Some(models) = entry.get_mut("models").and_then(Value::as_array_mut) {
                for model in models {
                    if model == "oc/mimo-v2.5-free" {
                        *model = json!("oc/mimo-v2.6-flash-free");
                    }
                }
            }
        }
    }
    Value::Object(settings)
}

async fn authorize(State(state): State<SettingsState>, request: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, request.headers()).await {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        Json(json!({ "error": "Unauthorized" })),
    )
        .into_response()
}

pub(crate) fn reply(status: StatusCode, body: Value, no_store: bool) -> Response {
    let mut headers = json!({ "content-type": "application/json" });
    if no_store {
        headers["cache-control"] = json!("no-store");
    }
    into_http(Ok(json!({
        "status": status.as_u16(),
        "headers": headers,
        "body": body.to_string(),
    })))
}

fn safe_settings(value: Value, include_runtime: bool) -> Value {
    let mut settings = object(value);
    settings.shift_remove("currentPassword");
    settings.shift_remove("newPassword");
    let has_password = settings
        .shift_remove("password")
        .is_some_and(|value| truthy(&value));
    let secret = settings.shift_remove("oidcClientSecret");
    let configured = settings.get("oidcIssuerUrl").is_some_and(truthy)
        && settings.get("oidcClientId").is_some_and(truthy)
        && secret.as_ref().is_some_and(truthy);
    settings.insert("oidcConfigured".into(), json!(configured));
    if include_runtime {
        for (name, var) in [
            ("enableRequestLogs", "ENABLE_REQUEST_LOGS"),
            ("enableTranslator", "ENABLE_TRANSLATOR"),
        ] {
            settings.insert(
                name.into(),
                json!(std::env::var(var).as_deref() == Ok("true")),
            );
        }
        settings.insert("hasPassword".into(), json!(has_password));
    }
    Value::Object(settings)
}

async fn require_login(State(state): State<SettingsState>) -> Response {
    let body = if let Some(settings) = state.storage.read_settings().await {
        json!({
            "requireLogin": settings["requireLogin"] != Value::Bool(false),
            "tunnelDashboardAccess": settings["tunnelDashboardAccess"] != Value::Bool(false),
            "tunnelUrl": url_or_empty(&settings["tunnelUrl"]),
            "tailscaleUrl": url_or_empty(&settings["tailscaleUrl"]),
        })
    } else {
        json!({ "requireLogin": true })
    };
    reply(StatusCode::OK, body, false)
}

fn url_or_empty(value: &Value) -> Value {
    if truthy(value) {
        value.clone()
    } else {
        json!("")
    }
}

async fn read(State(state): State<SettingsState>) -> Response {
    match state.storage.read_settings().await {
        Some(raw) => reply(StatusCode::OK, safe_settings(effective(raw), true), true),
        None => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"Could not load settings"}),
            false,
        ),
    }
}

fn read_raw(connection: &Connection) -> rusqlite::Result<Map<String, Value>> {
    let raw: Option<String> = connection
        .query_row("SELECT data FROM settings WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(raw
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .map(object)
        .unwrap_or_default())
}

enum Patched {
    Success(Value),
    Error(StatusCode, String),
}

fn update_password(
    raw: &Map<String, Value>,
    updates: &mut Map<String, Value>,
) -> Result<(), (StatusCode, String)> {
    let Some(new_password) = updates.get("newPassword").filter(|value| truthy(value)) else {
        return Ok(());
    };
    let old = updates.get("currentPassword").filter(|value| truthy(value));
    if let Some(hash) = raw.get("password").filter(|value| truthy(value)) {
        let Some(old) = old else {
            return Err((StatusCode::BAD_REQUEST, "Current password required".into()));
        };
        let verified = compare_password(Some(old), hash)
            .map_err(|message| (StatusCode::INTERNAL_SERVER_ERROR, message))?;
        if !verified {
            return Err((StatusCode::UNAUTHORIZED, "Invalid current password".into()));
        }
    } else if old.is_some_and(|value| value != "123456") {
        return Err((StatusCode::UNAUTHORIZED, "Invalid current password".into()));
    }
    let hash = hash_password(new_password)
        .map_err(|message| (StatusCode::INTERNAL_SERVER_ERROR, message))?;
    updates.shift_remove("currentPassword");
    updates.shift_remove("newPassword");
    updates.insert("password".into(), json!(hash));
    Ok(())
}

fn patch(
    connection: &mut Connection,
    mut updates: Map<String, Value>,
) -> rusqlite::Result<Patched> {
    updates.shift_remove("password");
    updates.shift_remove("mitmSudoEncrypted");
    if updates
        .get("oidcClientSecret")
        .is_some_and(|value| !truthy(value) || js_trim(&js_text(value)).is_empty())
    {
        updates.shift_remove("oidcClientSecret");
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut raw = read_raw(&tx)?;
    if let Err((status, message)) = update_password(&raw, &mut updates) {
        return Ok(Patched::Error(status, message));
    }
    raw.shift_remove("currentPassword");
    raw.shift_remove("newPassword");
    updates.shift_remove("currentPassword");
    updates.shift_remove("newPassword");
    raw.extend(updates);
    let raw = Value::Object(raw);
    write_raw(&tx, &raw)?;
    tx.commit()?;
    Ok(Patched::Success(safe_settings(effective(raw), false)))
}

fn write_raw(connection: &Connection, raw: &Value) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO settings(id, data) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET data = excluded.data",
        [raw.to_string()],
    )?;
    Ok(())
}

async fn update_public_proxy(
    State(state): State<SettingsState>,
    Path(provider): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if !crate::proxy_transport::supports_public_proxy_pool(&provider) {
        return reply(
            StatusCode::NOT_FOUND,
            json!({"error":"Provider proxy settings are unavailable"}),
            false,
        );
    }
    let pool = match body.get("proxyPoolId") {
        Some(Value::Null) => None,
        Some(Value::String(id)) if !id.trim().is_empty() && id != "__none__" => Some(id.clone()),
        _ => {
            return reply(
                StatusCode::BAD_REQUEST,
                json!({"error":"proxyPoolId must be an active pool ID or null"}),
                false,
            );
        }
    };
    let rotation = match body.get("rotateStrategy") {
        None | Some(Value::Null) => "none",
        Some(Value::String(value)) => match value.as_str() {
            "none" => "none",
            "round-robin" => "round-robin",
            "random" => "random",
            _ => "",
        },
        _ => "",
    };
    if rotation.is_empty() {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"Invalid proxy rotation strategy"}),
            false,
        );
    }
    let result = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(id) = &pool {
                let Some(entry) =
                    crate::proxy_pools::by_id(&tx, id)?.filter(|entry| entry["isActive"] == true)
                else {
                    return Ok(Patched::Error(
                        StatusCode::BAD_REQUEST,
                        "Proxy pool is unavailable".to_owned(),
                    ));
                };
                if matches!(provider.as_str(), "edge-tts" | "google-tts")
                    && entry["type"]
                        .as_str()
                        .is_some_and(crate::proxy_pools::relay)
                {
                    return Ok(Patched::Error(
                        StatusCode::BAD_REQUEST,
                        "This provider requires an HTTP or SOCKS proxy pool".to_owned(),
                    ));
                }
            }
            let mut raw = read_raw(&tx)?;
            let mut providers = object(raw.shift_remove("providerStrategies").unwrap_or_default());
            let mut strategy = object(providers.shift_remove(&provider).unwrap_or_default());
            if rotation == "none" {
                strategy.shift_remove("rotateStrategy");
            } else {
                strategy.insert("rotateStrategy".to_owned(), json!(rotation));
            }
            match &pool {
                Some(id) => {
                    strategy.insert("proxyPoolId".to_owned(), json!(id));
                }
                None => {
                    strategy.shift_remove("proxyPoolId");
                }
            }
            if !strategy.is_empty() {
                providers.insert(provider, Value::Object(strategy));
            }
            raw.insert("providerStrategies".to_owned(), Value::Object(providers));
            write_raw(&tx, &Value::Object(raw))?;
            tx.commit()?;
            Ok(Patched::Success(
                json!({"proxyPoolId":pool,"rotateStrategy":rotation}),
            ))
        })
        .await;
    match result {
        Ok(Patched::Success(settings)) => reply(StatusCode::OK, settings, true),
        Ok(Patched::Error(status, message)) => reply(status, json!({"error":message}), false),
        Err(_) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"Could not save provider proxy settings"}),
            false,
        ),
    }
}

async fn update(State(state): State<SettingsState>, request: Request) -> Response {
    let updates = to_bytes(request.into_body(), BODY_LIMIT)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Map<String, Value>>(&bytes).ok());
    let Some(updates) = updates else {
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"Invalid JSON body"}),
            false,
        );
    };
    let resets_combo = [
        "comboStrategy",
        "comboStickyRoundRobinLimit",
        "comboStrategies",
    ]
    .iter()
    .any(|key| updates.contains_key(*key));
    let updates_auto_ping = crate::quota_auto_ping::SETTING_KEYS
        .iter()
        .any(|key| updates.contains_key(*key));
    match state.storage.run(true, move |db| patch(db, updates)).await {
        Ok(Patched::Success(settings)) => {
            if resets_combo {
                crate::combo_chat::reset(None);
            }
            if updates_auto_ping {
                crate::quota_auto_ping::wake();
            }
            reply(StatusCode::OK, settings, true)
        }
        Ok(Patched::Error(status, message)) => reply(status, json!({ "error": message }), false),
        Err(error) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": error }),
            false,
        ),
    }
}
