use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value, json};

use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::provider_connections::{self as connections, object, truthy};
use crate::provider_index::{self, Compatible, compatible, js_trim};
use crate::storage::Storage;
use provider_response_rs::port::to_provider_connection_response;
use std::sync::Arc;
use tokio::sync::OnceCell;

#[path = "provider_client.rs"]
mod client;
#[path = "provider_suggested_models.rs"]
mod suggested;
#[path = "provider_web_login.rs"]
pub(crate) mod web_login;
pub(crate) use suggested::normalize_nous_models;

const BODY_LIMIT: usize = 128 * 1024 * 1024;
const PATCH_FIELDS: &[&str] = &[
    "name",
    "priority",
    "globalPriority",
    "defaultModel",
    "isActive",
    "testStatus",
    "lastError",
    "lastErrorAt",
];

#[derive(Clone)]
pub(crate) struct ProviderState {
    pub(crate) storage: Storage,
    backfilled: Arc<OnceCell<()>>,
    web_login: web_login::Sessions,
}

pub fn router(storage: Storage) -> Router {
    let state = ProviderState {
        storage,
        backfilled: Arc::new(OnceCell::new()),
        web_login: web_login::Sessions::default(),
    };
    Router::new()
        .route("/api/providers", get(list).post(create))
        .route("/api/providers/client", get(client::list))
        .route("/api/providers/{id}", get(show).put(update).delete(remove))
        .route(
            "/api/providers/{id}/overrides",
            get(crate::provider_overrides::show).put(crate::provider_overrides::store),
        )
        .route("/api/providers/suggested-models", get(suggested::list))
        .route("/api/providers/kilo/free-models", get(suggested::kilo))
        .route(
            "/api/providers/web-login",
            get(web_login::status)
                .post(web_login::start)
                .delete(web_login::cancel),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<ProviderState>, req: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, req.headers()).await {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        axum::Json(json!({ "error": "Unauthorized" })),
    )
        .into_response()
}

fn respond(status: StatusCode, body: Value) -> Response {
    into_http(Ok(json!({
        "status": status.as_u16(),
        "headers": { "content-type": "application/json" },
        "body": body.to_string(),
    })))
}

fn error(status: StatusCode, message: &str) -> Response {
    respond(status, json!({ "error": message }))
}

fn redact(conn: Value) -> rusqlite::Result<Value> {
    let mut result = to_provider_connection_response(conn).map_err(|message| {
        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(message)))
    })?;
    let (private_field, marker) = match result["provider"].as_str() {
        Some("xiaomi-mimo") => (Some("mimoPassToken"), None),
        Some("grok-cli") => (
            Some("idToken"),
            Some(crate::provider_oauth::GROK_CLI_REFRESH_ATTEMPT),
        ),
        Some("iflow") => (Some("cookie"), None),
        Some("kiro") => (
            Some("clientSecret"),
            Some(crate::provider_oauth::KIRO_REFRESH_ATTEMPT),
        ),
        Some("nous") => (None, Some(crate::provider_oauth::NOUS_REFRESH_ATTEMPT)),
        Some("kimi") => (None, Some(crate::provider_oauth::kimi::ATTEMPT)),
        Some("minimax-code" | "minimax-code-global") => {
            (None, Some(crate::provider_oauth::minimax_code::ATTEMPT))
        }
        Some("muse") => (Some("oauthAccessToken"), None),
        Some("windsurf") => (Some("firebaseIdToken"), None),
        _ => (None, None),
    };
    if let Some(field) = private_field
        && let Some(data) = result
            .get_mut("providerSpecificData")
            .and_then(Value::as_object_mut)
    {
        data.shift_remove(field);
    }
    if let Some(marker) = marker
        && let Some(row) = result.as_object_mut()
    {
        row.shift_remove(marker);
    }
    Ok(result)
}

async fn body(req: Request) -> Option<Value> {
    let raw = to_bytes(req.into_body(), BODY_LIMIT).await.ok()?;
    serde_json::from_slice::<Value>(&raw)
        .ok()
        .filter(|value| !value.is_null())
}

async fn list(State(state): State<ProviderState>) -> Response {
    let result = state
        .storage
        .run(false, |db| {
            connections::read_all(db, None)?
                .into_iter()
                .map(|conn| {
                    let id = conn["provider"].as_str().unwrap_or_default().to_owned();
                    let mut safe = redact(conn)?;
                    if matches!(
                        compatible(&id),
                        Some(Compatible::OpenAi | Compatible::Anthropic)
                    ) && !truthy(&safe["name"])
                    {
                        let node_name = db
                            .query_row(
                                "SELECT name FROM providerNodes WHERE id = ?1",
                                [&id],
                                |row| row.get::<_, Option<String>>(0),
                            )
                            .optional()
                            .ok()
                            .flatten()
                            .flatten()
                            .filter(|s| !s.is_empty());
                        safe["name"] = node_name.map_or_else(
                            || {
                                first_truthy([
                                    safe["providerSpecificData"]["nodeName"].clone(),
                                    json!(id),
                                ])
                            },
                            Value::String,
                        );
                    }
                    Ok(safe)
                })
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await;
    match result {
        Ok(connections) => respond(StatusCode::OK, json!({ "connections": connections })),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch providers",
        ),
    }
}

async fn show(State(state): State<ProviderState>, Path(id): Path<String>) -> Response {
    match state
        .storage
        .run(false, move |db| {
            connections::by_id(db, &id)?.map(redact).transpose()
        })
        .await
    {
        Ok(Some(connection)) => respond(StatusCode::OK, json!({ "connection": connection })),
        Ok(None) => missing(),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch connection",
        ),
    }
}

enum Mutation {
    Connection(Value),
    Error(StatusCode, Value),
}

impl Mutation {
    fn error(status: StatusCode, message: &str) -> Self {
        Self::Error(status, json!({ "error": message }))
    }
}

fn finish(result: Result<Mutation, String>, status: StatusCode, failed: &str) -> Response {
    match result {
        Ok(Mutation::Connection(connection)) => {
            respond(status, json!({ "connection": connection }))
        }
        Ok(Mutation::Error(status, body)) => respond(status, body),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, failed),
    }
}

fn missing() -> Response {
    error(StatusCode::NOT_FOUND, "Connection not found")
}

async fn create(State(state): State<ProviderState>, req: Request) -> Response {
    let Some(body) = body(req).await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create provider",
        );
    };
    let result = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = create_in(&tx, body)?;
            tx.commit()?;
            Ok(result)
        })
        .await;
    finish(result, StatusCode::CREATED, "Failed to create provider")
}

fn create_in(db: &Connection, body: Value) -> rusqlite::Result<Mutation> {
    let proxy = match proxy_config(&body) {
        Ok(proxy) => proxy,
        Err(message) => return Ok(Mutation::error(StatusCode::BAD_REQUEST, message)),
    };
    let pool = match proxy_pool(db, body.get("proxyPoolId")) {
        Ok(pool) => pool.flatten(),
        Err(message) => return Ok(Mutation::error(StatusCode::BAD_REQUEST, message)),
    };
    let id = provider_index::normalize(body["provider"].as_str().unwrap_or_default());
    let definition = provider_index::find(&id);
    let compatible = compatible(&id);
    if id.is_empty()
        || (compatible.is_none() && definition.is_none_or(|p| p["canCreateWithKey"] != true))
    {
        return Ok(Mutation::error(StatusCode::BAD_REQUEST, "Invalid provider"));
    }
    let def = definition.cloned().unwrap_or_default();
    let is_cookie = def["webCookie"] == true;
    let fallback_key = def["apiKeyOptionalWith"]
        .as_str()
        .is_some_and(|name| truthy(&body["providerSpecificData"][name]));
    if !truthy(&body["apiKey"]) && id != "ollama-local" && !fallback_key {
        let message = if is_cookie {
            "Cookie value is required"
        } else {
            "API Key is required"
        };
        return Ok(Mutation::error(StatusCode::BAD_REQUEST, message));
    }
    let name = first_truthy([
        body["name"].clone(),
        body["displayName"].clone(),
        def["name"].clone(),
    ]);
    if !truthy(&name) {
        return Ok(Mutation::error(StatusCode::BAD_REQUEST, "Name is required"));
    }

    let mut details = object(body["providerSpecificData"].clone());
    if id == "ollama-local" {
        let base = first_truthy([
            details.get("baseUrl").cloned().unwrap_or_default(),
            body["baseUrl"].clone(),
            body["baseURL"].clone(),
            body["ollamaHostUrl"].clone(),
            json!(""),
        ]);
        if !base.is_null() && !base.is_string() {
            return Err(rusqlite::Error::InvalidQuery);
        }
        if let Some(base) = base.as_str().map(js_trim).filter(|base| !base.is_empty()) {
            details.insert("baseUrl".into(), json!(base));
        }
    }
    if let Some(kind) = compatible {
        let node = db
            .query_row(
                "SELECT name, data FROM providerNodes WHERE id = ?1",
                [&id],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((name, data)) = node else {
            let message = match kind {
                Compatible::OpenAi => "OpenAI Compatible node not found",
                Compatible::Anthropic => "Anthropic Compatible node not found",
                Compatible::Embedding => "Custom Embedding node not found",
            };
            return Ok(Mutation::error(StatusCode::NOT_FOUND, message));
        };
        let node: Value = serde_json::from_str(&data).unwrap_or_default();
        details = Map::new();
        for key in ["prefix", "apiType", "baseUrl"] {
            if (key != "apiType" || kind == Compatible::OpenAi)
                && let Some(value) = node.get(key)
            {
                details.insert(key.into(), value.clone());
            }
        }
        details.insert("nodeName".into(), json!(name));
    }
    details.extend(proxy);
    if let Some(pool) = pool {
        details.insert("proxyPoolId".into(), json!(pool));
    }

    let now = connections::now();
    let auth_type = if is_cookie { "cookie" } else { "apikey" };
    let priority = first_truthy([body["priority"].clone(), json!(1)]);
    let status = first_truthy([body["testStatus"].clone(), json!("unknown")]);
    let api_key = if truthy(&body["apiKey"]) {
        body["apiKey"].clone()
    } else {
        json!("")
    };
    let global = first_truthy([body["globalPriority"].clone()]);
    let model = first_truthy([body["defaultModel"].clone()]);
    let overwrite =
        truthy(&body["id"]) || body["allowOverwrite"] == true || body["overwrite"] == true;

    if !is_cookie && let Some(existing) = connections::api_key_by_name(db, &id, &name)? {
        if !overwrite {
            let existing_name = existing["name"].as_str().unwrap_or_default();
            return Ok(Mutation::Error(
                StatusCode::CONFLICT,
                json!({
                    "error": format!("A connection named \"{existing_name}\" already exists for provider \"{id}\". Pass allowOverwrite: true to replace it."),
                    "code": "PROVIDER_NAME_CONFLICT",
                    "existingId": existing["id"],
                    "existingName": existing["name"],
                }),
            ));
        }
        let updated = connections::patch(
            existing,
            object(json!({
                "provider": id, "authType": auth_type, "name": name, "apiKey": api_key, "priority": priority,
                "globalPriority": global, "defaultModel": model, "providerSpecificData": details,
                "isActive": true, "testStatus": status, "allowOverwrite": overwrite,
            })),
            &now,
        );
        connections::write(db, &updated)?;
        return Ok(Mutation::Connection(redact(updated)?));
    }

    let mut connection = object(json!({
        "id": uuid::Uuid::new_v4().to_string(), "provider": id, "authType": auth_type, "name": name,
        "priority": priority, "isActive": true, "createdAt": now, "updatedAt": now,
    }));
    for (key, value) in [
        ("globalPriority", global),
        ("defaultModel", model),
        ("apiKey", api_key),
        ("testStatus", status),
        ("providerSpecificData", Value::Object(details)),
    ] {
        if !value.is_null() {
            connection.insert(key.into(), value);
        }
    }
    let connection = Value::Object(connection);
    connections::write(db, &connection)?;
    Ok(Mutation::Connection(redact(connection)?))
}

async fn update(
    State(state): State<ProviderState>,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    let Some(body) = body(req).await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update connection",
        );
    };
    let result = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = update_in(&tx, &id, body)?;
            tx.commit()?;
            Ok(result)
        })
        .await;
    finish(result, StatusCode::OK, "Failed to update connection")
}

fn update_in(db: &Connection, id: &str, body: Value) -> rusqlite::Result<Mutation> {
    let Some(existing) = connections::by_id(db, id)? else {
        return Ok(Mutation::error(
            StatusCode::NOT_FOUND,
            "Connection not found",
        ));
    };
    let proxy = match proxy_fields(&body) {
        Ok(proxy) => proxy,
        Err(message) => return Ok(Mutation::error(StatusCode::BAD_REQUEST, message)),
    };
    let pool = match proxy_pool(db, body.get("proxyPoolId")) {
        Ok(pool) => pool,
        Err(message) => return Ok(Mutation::error(StatusCode::BAD_REQUEST, message)),
    };
    let provider_id = existing["provider"].as_str().unwrap_or_default().to_owned();
    let mut patch = Map::new();
    for name in PATCH_FIELDS {
        if let Some(value) = body.get(*name) {
            patch.insert((*name).into(), value.clone());
        }
    }
    if truthy(&body["apiKey"]) && existing["authType"] == "apikey" {
        patch.insert("apiKey".into(), body["apiKey"].clone());
    }
    if existing.get("providerSpecificData").is_some()
        || body.get("providerSpecificData").is_some()
        || !proxy.is_empty()
        || pool.is_some()
    {
        let is_aws =
            provider_index::find(&provider_id).is_some_and(|p| p["credentialForm"] == "aws");
        let mut incoming = object(body["providerSpecificData"].clone());
        if is_aws
            && incoming
                .get("sessionToken")
                .is_some_and(|value| value == "")
        {
            incoming.shift_remove("sessionToken");
        }
        let mut merged = object(existing["providerSpecificData"].clone());
        if is_aws && incoming.get("sessionToken").is_some_and(Value::is_null) {
            incoming.shift_remove("sessionToken");
            merged.shift_remove("sessionToken");
        }
        merged.extend(incoming);
        merged.extend(proxy);
        if let Some(pool) = pool {
            match pool {
                Some(id) => {
                    merged.insert("proxyPoolId".into(), json!(id));
                }
                None => {
                    merged.shift_remove("proxyPoolId");
                }
            }
        }
        patch.insert("providerSpecificData".into(), Value::Object(merged));
    }

    let reorder = patch.contains_key("priority");
    let updated = connections::patch(existing, patch, &connections::now());
    connections::write(db, &updated)?;
    if reorder {
        connections::reorder(db, &provider_id)?;
    }
    Ok(Mutation::Connection(redact(updated)?))
}

async fn remove(State(state): State<ProviderState>, Path(id): Path<String>) -> Response {
    let result = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let provider = tx
                .query_row(
                    "SELECT provider FROM providerConnections WHERE id = ?1",
                    [&id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(provider) = provider else {
                return Ok(false);
            };
            tx.execute("DELETE FROM providerConnections WHERE id = ?1", [&id])?;
            connections::reorder(&tx, &provider)?;
            tx.commit()?;
            Ok(true)
        })
        .await;
    match result {
        Ok(true) => respond(
            StatusCode::OK,
            json!({ "message": "Connection deleted successfully" }),
        ),
        Ok(false) => missing(),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to delete connection",
        ),
    }
}

fn first_truthy<const N: usize>(values: [Value; N]) -> Value {
    values.into_iter().find(truthy).unwrap_or_default()
}

fn proxy_fields(body: &Value) -> Result<Map<String, Value>, &'static str> {
    let fields = [
        "connectionProxyEnabled",
        "connectionProxyUrl",
        "connectionNoProxy",
    ];
    if !fields.iter().any(|key| body.get(key).is_some()) {
        return Ok(Map::new());
    }
    proxy_config(body)
}

fn proxy_config(body: &Value) -> Result<Map<String, Value>, &'static str> {
    let enabled = body["connectionProxyEnabled"] == true;
    let url = body["connectionProxyUrl"]
        .as_str()
        .map(js_trim)
        .unwrap_or_default();
    let no_proxy = body["connectionNoProxy"]
        .as_str()
        .map(js_trim)
        .unwrap_or_default();
    if enabled && url.is_empty() {
        return Err("Connection proxy URL is required when connection proxy is enabled");
    }
    Ok(object(json!({
        "connectionProxyEnabled": enabled, "connectionProxyUrl": url, "connectionNoProxy": no_proxy,
    })))
}

fn proxy_pool(
    db: &Connection,
    value: Option<&Value>,
) -> Result<Option<Option<String>>, &'static str> {
    let Some(value) = value else {
        return Ok(None);
    };
    let id = value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string());
    if value.is_null() || id == "__none__" || js_trim(&id).is_empty() {
        return Ok(Some(None));
    }
    let id = js_trim(&id);
    let exists = db
        .query_row("SELECT 1 FROM proxyPools WHERE id = ?1", [id], |_| Ok(()))
        .optional()
        .map_err(|_| "Proxy pool not found")?
        .is_some();
    if !exists {
        return Err("Proxy pool not found");
    }
    Ok(Some(Some(id.to_owned())))
}
