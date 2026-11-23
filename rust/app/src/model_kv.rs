use std::collections::HashSet;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use provider_capabilities_rs::port::model_capabilities;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value, json};

use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::provider_index;
use crate::storage::Storage;

const DISABLED: &str = "disabledModels";
const CUSTOM: &str = "customModels";
const ALIASES: &str = "modelAliases";
const BODY_LIMIT: usize = 128 * 1024 * 1024;

#[derive(Clone)]
struct ModelKvState {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    let state = ModelKvState { storage };
    Router::new()
        .route(
            "/api/models",
            get(dashboard_models)
                .put(set_dashboard_alias)
                .options(dashboard_options),
        )
        .route(
            "/api/models/disabled",
            get(get_disabled).post(disable).delete(enable),
        )
        .route(
            "/api/models/custom",
            get(get_custom).post(add_custom).delete(remove_custom),
        )
        .route(
            "/api/models/alias",
            put(set_alias)
                .get(get_aliases)
                .delete(remove_alias)
                .options(alias_options),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<ModelKvState>, req: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, req.headers()).await {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::VARY, "Accept-Encoding")],
            Json(json!({"error": "Unauthorized"})),
        )
            .into_response()
    }
}

fn reply(status: StatusCode, value: Value) -> Response {
    into_http(Ok(json!({
        "status": status.as_u16(),
        "headers": {"content-type": "application/json"},
        "body": value.to_string(),
    })))
}

async fn dashboard_options() -> Response {
    allowed_methods("GET, HEAD, OPTIONS, PUT")
}

async fn alias_options() -> Response {
    allowed_methods("DELETE, GET, HEAD, OPTIONS, PUT")
}

fn allowed_methods(allow: &str) -> Response {
    into_http(Ok(
        json!({"status":204,"headers":{"allow":allow},"body":""}),
    ))
}

fn failed(message: &str) -> Response {
    reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": message}))
}

fn bad_request(message: &str) -> Response {
    reply(StatusCode::BAD_REQUEST, json!({"error": message}))
}

fn with_result(result: Result<Value, String>, message: &str) -> Response {
    match result {
        Ok(data) => reply(StatusCode::OK, data),
        Err(_) => failed(message),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

async fn read_json(request: Request) -> Option<Value> {
    let bytes = to_bytes(request.into_body(), BODY_LIMIT).await.ok()?;
    let raw = String::from_utf8_lossy(&bytes);
    let value: Value = serde_json::from_str(raw.strip_prefix('\u{feff}').unwrap_or(&raw)).ok()?;
    (!value.is_null()).then_some(value)
}

fn storage_key(db: &Connection, value: &Value) -> rusqlite::Result<String> {
    match value {
        Value::String(text) => Ok(text.replace('\0', "")),
        Value::Number(number) => {
            let parameter = number.as_f64().ok_or(rusqlite::Error::InvalidQuery)?;
            db.query_row("SELECT CAST(?1 AS TEXT)", [parameter], |row| row.get(0))
        }
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn query(request: &Request, name: &str) -> Option<String> {
    url::form_urlencoded::parse(request.uri().query()?.as_bytes())
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
        .filter(|value| !value.is_empty())
}

fn js_index(key: &str) -> u32 {
    key.parse::<u32>()
        .ok()
        .filter(|index| index.to_string() == key)
        .unwrap_or(u32::MAX)
}

async fn read_scope(
    storage: &Storage,
    scope: &'static str,
    parse_fallback: Value,
) -> Result<Map<String, Value>, String> {
    storage
        .run(false, move |db| scope_values(db, scope, parse_fallback))
        .await
}

fn scope_values(
    db: &Connection,
    scope: &str,
    fallback: Value,
) -> rusqlite::Result<Map<String, Value>> {
    let mut query = db.prepare("SELECT key, value FROM kv WHERE scope = ?1")?;
    let mut rows = query
        .query_map([scope], |row| {
            let key = row.get::<_, String>(0)?;
            let raw = row.get::<_, String>(1)?;
            let value = serde_json::from_str(&raw).unwrap_or_else(|_| fallback.clone());
            Ok((key, value))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.sort_by_key(|(key, _)| js_index(key));
    Ok(rows
        .into_iter()
        .filter(|(key, _)| key != "__proto__")
        .collect())
}

pub async fn disabled_models(storage: &Storage) -> Result<Map<String, Value>, String> {
    read_scope(storage, DISABLED, json!([])).await
}

pub async fn custom_models(storage: &Storage) -> Result<Vec<Value>, String> {
    read_scope(storage, CUSTOM, Value::Null)
        .await
        .map(|models| models.into_values().collect())
}

pub async fn model_aliases(storage: &Storage) -> Result<Map<String, Value>, String> {
    read_scope(storage, ALIASES, Value::Null).await
}

fn value_for_key(db: &Connection, scope: &str, key: &str) -> rusqlite::Result<Option<Value>> {
    let raw = db
        .query_row(
            "SELECT value FROM kv WHERE scope = ?1 AND key = ?2",
            (scope, key.replace('\0', "")),
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(raw.and_then(|text| serde_json::from_str(&text).ok()))
}

fn save(db: &Connection, scope: &str, key: &str, value: &Value) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO kv(scope, key, value) VALUES(?1, ?2, ?3)
         ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
        (scope, key.replace('\0', ""), value.to_string()),
    )?;
    Ok(())
}

fn delete(db: &Connection, scope: &str, key: &str) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM kv WHERE scope = ?1 AND key = ?2",
        (scope, key.replace('\0', "")),
    )?;
    Ok(())
}

async fn remove(storage: &Storage, scope: &'static str, key: String) -> Result<Value, String> {
    storage
        .run(true, move |db| {
            delete(db, scope, &key)?;
            Ok(json!({"success": true}))
        })
        .await
}

async fn get_disabled(State(state): State<ModelKvState>, request: Request) -> Response {
    let alias = query(&request, "providerAlias");
    let result = disabled_models(&state.storage).await.map(|models| {
        if let Some(key) = alias {
            json!({"ids": models.get(&key).filter(|value| truthy(value)).cloned().unwrap_or_else(|| json!([]))})
        } else {
            json!({"disabled": models})
        }
    });
    with_result(result, "Failed to fetch disabled models")
}

#[derive(PartialEq, Eq, Hash)]
enum ScalarId<'a> {
    Null,
    Bool(bool),
    Number(u64),
    Text(&'a str),
}

fn scalar_id(value: &Value) -> Option<ScalarId<'_>> {
    match value {
        Value::Null => Some(ScalarId::Null),
        Value::Bool(value) => Some(ScalarId::Bool(*value)),
        Value::Number(value) => value
            .as_f64()
            .map(|n| ScalarId::Number(if n == 0.0 { 0 } else { n.to_bits() })),
        Value::String(value) => Some(ScalarId::Text(value)),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn current_ids(db: &Connection, key: &str, spread: bool) -> rusqlite::Result<Vec<Value>> {
    match value_for_key(db, DISABLED, key)?.filter(truthy) {
        None => Ok(Vec::new()),
        Some(Value::Array(ids)) => Ok(ids),
        Some(Value::String(ids)) if spread => Ok(ids
            .chars()
            .map(|id| Value::String(id.to_string()))
            .collect()),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

async fn disable(State(state): State<ModelKvState>, request: Request) -> Response {
    let Some(body) = read_json(request).await else {
        return failed("Failed to disable models");
    };
    if !truthy(&body["providerAlias"]) || !body["ids"].is_array() {
        return bad_request("providerAlias and ids[] required");
    }
    let provider_alias = body["providerAlias"].clone();
    let ids = body["ids"].as_array().cloned().unwrap_or_default();
    let result = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let key = storage_key(&tx, &provider_alias)?;
            let mut all = current_ids(&tx, &key, true)?;
            all.extend(ids);
            let mut seen = HashSet::new();
            let merged = all
                .iter()
                .filter(|value| scalar_id(value).is_none_or(|key| seen.insert(key)))
                .cloned()
                .collect();
            save(&tx, DISABLED, &key, &Value::Array(merged))?;
            tx.commit()?;
            Ok(json!({"success": true}))
        })
        .await;
    with_result(result, "Failed to disable models")
}

async fn enable(State(state): State<ModelKvState>, request: Request) -> Response {
    let Some(key) = query(&request, "providerAlias") else {
        return bad_request("providerAlias required");
    };
    let result = if let Some(id) = query(&request, "id") {
        state
            .storage
            .run(true, move |db| {
                let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut current = current_ids(&tx, &key, false)?;
                current.retain(|value| value.as_str() != Some(&id));
                if current.is_empty() {
                    delete(&tx, DISABLED, &key)?;
                } else {
                    save(&tx, DISABLED, &key, &Value::Array(current))?;
                }
                tx.commit()?;
                Ok(json!({"success": true}))
            })
            .await
    } else {
        remove(&state.storage, DISABLED, key).await
    };
    with_result(result, "Failed to enable models")
}

async fn get_custom(State(state): State<ModelKvState>) -> Response {
    with_result(
        custom_models(&state.storage)
            .await
            .map(|models| json!({"models": models})),
        "Failed to fetch custom models",
    )
}

fn custom_key(alias: &str, id: &str, kind: &str) -> String {
    format!("{alias}|{id}|{kind}")
}

fn custom_caps(raw: &Value) -> Option<Value> {
    let caps: Map<String, Value> = ["vision", "reasoning"]
        .into_iter()
        .filter_map(|key| {
            raw[key]
                .as_bool()
                .map(|value| (key.into(), Value::Bool(value)))
        })
        .collect();
    (!caps.is_empty()).then_some(Value::Object(caps))
}

fn custom_transport(raw: &Value, kind: &str) -> Option<Value> {
    let transport = raw
        .as_str()?
        .trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}');
    (kind == "stt" && transport == "gemini-live").then(|| json!("gemini-live"))
}

async fn add_custom(State(state): State<ModelKvState>, request: Request) -> Response {
    let Some(body) = read_json(request).await else {
        return failed("Failed to add custom model");
    };
    if !truthy(&body["providerAlias"]) || !truthy(&body["id"]) {
        return bad_request("providerAlias and id required");
    }
    let alias = &body["providerAlias"];
    let id = &body["id"];
    let kind = if truthy(&body["type"]) {
        body["type"].clone()
    } else {
        json!("llm")
    };
    let key = custom_key(
        &crate::chat_output::js_text(alias),
        &crate::chat_output::js_text(id),
        &crate::chat_output::js_text(&kind),
    )
    .replace('\0', "");
    let mut new = Map::new();
    new.insert("providerAlias".into(), alias.clone());
    new.insert("id".into(), id.clone());
    new.insert("type".into(), kind.clone());
    let has_name = truthy(&body["name"]);
    let name = if has_name {
        body["name"].clone()
    } else {
        id.clone()
    };
    new.insert("name".into(), name.clone());
    let caps = custom_caps(&body["caps"]);
    let transport = custom_transport(&body["transport"], kind.as_str().unwrap_or_default());
    let result = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let raw = tx
                .query_row(
                    "SELECT value FROM kv WHERE scope = ?1 AND key = ?2",
                    (CUSTOM, &key),
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let added = raw.is_none();
            let mut model = match raw {
                None => new,
                Some(raw) => match serde_json::from_str::<Value>(&raw) {
                    Ok(Value::Object(previous)) => previous,
                    Ok(Value::Array(previous)) => previous
                        .into_iter()
                        .enumerate()
                        .map(|(i, value)| (i.to_string(), value))
                        .collect(),
                    Ok(Value::String(previous)) => previous
                        .chars()
                        .enumerate()
                        .map(|(i, value)| (i.to_string(), Value::String(value.to_string())))
                        .collect(),
                    _ => Map::new(),
                },
            };
            if !added && has_name {
                model.insert("name".into(), name);
            }
            if let Some(caps) = caps {
                model.insert("caps".into(), caps);
            }
            if let Some(transport) = transport {
                model.insert("transport".into(), transport);
            }
            save(&tx, CUSTOM, &key, &Value::Object(model))?;
            tx.commit()?;
            Ok(json!({"success": true, "added": added}))
        })
        .await;
    with_result(result, "Failed to add custom model")
}

async fn remove_custom(State(state): State<ModelKvState>, request: Request) -> Response {
    let (Some(alias), Some(id)) = (query(&request, "providerAlias"), query(&request, "id")) else {
        return bad_request("providerAlias and id required");
    };
    let kind = query(&request, "type").unwrap_or_else(|| "llm".into());
    let result = remove(&state.storage, CUSTOM, custom_key(&alias, &id, &kind)).await;
    with_result(result, "Failed to delete custom model")
}

async fn get_aliases(State(state): State<ModelKvState>) -> Response {
    with_result(
        model_aliases(&state.storage)
            .await
            .map(|aliases| json!({"aliases": aliases})),
        "Failed to fetch aliases",
    )
}

async fn dashboard_models(State(state): State<ModelKvState>) -> Response {
    let result = state
        .storage
        .run(false, |db| {
            let transaction = db.transaction()?;
            let disabled = scope_values(&transaction, DISABLED, json!([]))?;
            let aliases = scope_values(&transaction, ALIASES, Value::Null)?;
            let custom = scope_values(&transaction, CUSTOM, Value::Null)?;
            transaction.commit()?;

            let catalog = crate::openai_models::data();
            let mut listed = Vec::new();
            let mut seen = HashSet::new();
            for (provider, models) in catalog["models"].as_object().into_iter().flatten() {
                let routed = provider_index::find(provider)
                    .and_then(|entry| entry["alias"].as_str())
                    .unwrap_or(provider);
                let disabled = disabled
                    .get(routed)
                    .filter(|value| truthy(value))
                    .or_else(|| disabled.get(provider));
                for entry in models.as_array().into_iter().flatten() {
                    let Some(model) = entry["id"].as_str() else {
                        continue;
                    };
                    if disabled
                        .and_then(Value::as_array)
                        .is_some_and(|models| models.iter().any(|entry| entry == model))
                    {
                        continue;
                    }
                    let full = format!("{provider}/{model}");
                    let routed = format!("{routed}/{model}");
                    let label = aliases
                        .get(&full)
                        .filter(|alias| truthy(alias))
                        .cloned()
                        .unwrap_or_else(|| json!(model));
                    let mut row = json!({"provider":provider, "model":model});
                    if let Some(name) = entry.get("name") {
                        row["name"] = name.clone();
                    }
                    row["fullModel"] = json!(full);
                    row["routedModel"] = json!(routed);
                    row["alias"] = label;
                    row["caps"] = dashboard_caps(provider, model);
                    listed.push(row);
                    seen.extend([full, routed]);
                }
            }
            for custom in custom.into_values() {
                let kind = custom
                    .get("kind")
                    .filter(|value| truthy(value))
                    .or_else(|| custom.get("type").filter(|value| truthy(value)));
                let (Some(provider), Some(model)) =
                    (custom["providerAlias"].as_str(), custom["id"].as_str())
                else {
                    continue;
                };
                let full = format!("{provider}/{model}");
                if model.is_empty()
                    || kind.is_some_and(|kind| kind != "llm")
                    || seen.contains(&full)
                {
                    continue;
                }
                let id = catalog["infoProviderByAlias"][provider]
                    .as_str()
                    .unwrap_or(provider);
                let alias = aliases
                    .get(&full)
                    .filter(|value| truthy(value))
                    .cloned()
                    .unwrap_or_else(|| json!(model));
                let name = custom
                    .get("name")
                    .filter(|value| truthy(value))
                    .cloned()
                    .unwrap_or_else(|| json!(model));
                let mut caps = dashboard_caps(id, model);
                if let (Some(caps), Some(overrides)) =
                    (caps.as_object_mut(), custom["caps"].as_object())
                {
                    caps.extend(overrides.clone());
                }
                listed.push(json!({
                    "provider":provider, "model":model, "name":name, "fullModel":full,
                    "routedModel":full, "alias":alias, "caps":caps,
                }));
            }
            Ok(json!({"models":listed}))
        })
        .await;
    with_result(result, "Failed to fetch models")
}

fn dashboard_caps(provider: &str, model: &str) -> Value {
    let caps = model_capabilities(Some(provider), model, None);
    let visible = [
        "vision",
        "search",
        "reasoning",
        "contextWindow",
        "maxOutput",
    ];
    Value::Object(
        visible
            .into_iter()
            .filter_map(|key| Some((key.into(), caps.get(key)?.clone())))
            .collect(),
    )
}

async fn set_dashboard_alias(State(state): State<ModelKvState>, request: Request) -> Response {
    let Some(body) = read_json(request).await else {
        return failed("Failed to update alias");
    };
    if !truthy(&body["model"]) || !truthy(&body["alias"]) {
        return bad_request("Model and alias required");
    }
    let model = body["model"].clone();
    let alias = body["alias"].clone();
    let result = state
        .storage
        .run(true, move |db| {
            let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let aliases = scope_values(&transaction, ALIASES, Value::Null)?;
            if aliases.iter().any(|(key, value)| {
                model.as_str() != Some(key)
                    && scalar_id(&alias)
                        .is_some_and(|alias| scalar_id(value).as_ref() == Some(&alias))
            }) {
                return Ok(None);
            }
            let key = storage_key(&transaction, &model)?;
            save(&transaction, ALIASES, &key, &alias)?;
            transaction.commit()?;
            Ok(Some(json!({"success":true,"model":model,"alias":alias})))
        })
        .await;
    match result {
        Ok(None) => bad_request("Alias already in use"),
        Ok(Some(value)) => reply(StatusCode::OK, value),
        Err(_) => failed("Failed to update alias"),
    }
}

async fn set_alias(State(state): State<ModelKvState>, request: Request) -> Response {
    let Some(body) = read_json(request).await else {
        return failed("Failed to update alias");
    };
    if !truthy(&body["model"]) || !truthy(&body["alias"]) {
        return bad_request("Model and alias required");
    }
    let alias = body["alias"].clone();
    let model = body["model"].clone();
    let result = state
        .storage
        .run(true, move |db| {
            let key = storage_key(db, &alias)?;
            save(db, ALIASES, &key, &model)?;
            Ok(json!({"success": true, "model": model, "alias": alias}))
        })
        .await;
    with_result(result, "Failed to update alias")
}

async fn remove_alias(State(state): State<ModelKvState>, request: Request) -> Response {
    let Some(alias) = query(&request, "alias") else {
        return bad_request("Alias required");
    };
    with_result(
        remove(&state.storage, ALIASES, alias).await,
        "Failed to delete alias",
    )
}
