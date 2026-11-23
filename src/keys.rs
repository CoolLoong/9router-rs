use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use key_access_rs::port::{
    key_access_from_columns, key_access_to_columns, validate_key_access_input,
};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::dashboard_session;
use crate::key_identity;
use crate::observed_response::into_http;
use crate::storage::{Storage, is_active};

const KEY_COLUMNS: &str =
    "id, key, name, machineId, isActive, createdAt, accessRestricted, accessAllow";
const REQUEST_BODY_LIMIT: usize = 128 * 1024 * 1024;

#[derive(Clone)]
struct KeyState {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    let state = KeyState { storage };
    Router::new()
        .route("/api/keys", get(list).post(create))
        .route("/api/keys/{id}", get(show).put(update).delete(remove))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<KeyState>, req: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, req.headers()).await {
        return next.run(req).await;
    }
    reply(
        StatusCode::UNAUTHORIZED,
        json!({ "error": "Unauthorized" }),
        true,
    )
}

fn reply(status: StatusCode, body: Value, from_guard: bool) -> Response {
    if from_guard {
        use axum::response::IntoResponse;
        return (
            status,
            [(header::VARY, "Accept-Encoding")],
            axum::Json(body),
        )
            .into_response();
    }
    into_http(Ok(json!({
        "status": status.as_u16(),
        "headers": { "content-type": "application/json" },
        "body": body.to_string(),
    })))
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({ "error": message }), false)
}

fn key_row(row: &Row<'_>) -> rusqlite::Result<Value> {
    let restricted = is_active(row.get_ref(6)?);
    let allow: Option<String> = row.get(7)?;
    let access =
        key_access_from_columns(json!(restricted), json!(allow)).map_err(conversion_error)?;
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "key": row.get::<_, String>(1)?,
        "name": row.get::<_, Option<String>>(2)?,
        "machineId": row.get::<_, Option<String>>(3)?,
        "isActive": is_active(row.get_ref(4)?),
        "createdAt": row.get::<_, String>(5)?,
        "access": access,
    }))
}

fn conversion_error(error: String) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(error)))
}

pub(super) fn backup_key_access(item: &Value) -> rusqlite::Result<Value> {
    let Some(access) = item.get("access") else {
        return key_access_to_columns(Value::Null).map_err(conversion_error);
    };
    let checked = validate_key_access_input(access.clone()).map_err(conversion_error)?;
    if checked["ok"] != true {
        let text = crate::chat_output::js_text;
        let id = item.get("id").map_or_else(|| "undefined".into(), text);
        return Err(conversion_error(format!(
            "apiKeys {id}: {}",
            text(&checked["error"])
        )));
    }
    key_access_to_columns(checked["value"].clone()).map_err(conversion_error)
}

fn by_id(db: &Connection, id: &str) -> rusqlite::Result<Option<Value>> {
    db.query_row(
        &format!("SELECT {KEY_COLUMNS} FROM apiKeys WHERE id = ?1"),
        [id],
        key_row,
    )
    .optional()
}

async fn create(State(state): State<KeyState>, req: Request) -> Response {
    let Some(body) = read_body(req).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create key");
    };
    let name = body["name"].clone();
    if !js_truthy(&name) {
        return error(StatusCode::BAD_REQUEST, "Name is required");
    }
    let column = match &name {
        Value::String(value) => rusqlite::types::Value::Text(value.clone()),
        Value::Number(value) => rusqlite::types::Value::Real(value.as_f64().unwrap_or_default()),
        _ => return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create key"),
    };

    let root = state.storage.clone();
    let result = state.storage.run(true, move |db| {
        let (machine_id, key) = key_identity::generate(root.root()).map_err(|error| conversion_error(error.to_string()))?;
        let id = uuid::Uuid::new_v4().to_string();
        let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        db.execute(
            "INSERT INTO apiKeys (id, key, name, machineId, isActive, createdAt, accessRestricted, accessAllow)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, 0, '[]')",
            params![id, key, column, machine_id, created_at],
        )?;
        Ok(json!({ "key": key, "name": name, "id": id, "machineId": machine_id }))
    }).await;
    match result {
        Ok(body) => reply(StatusCode::CREATED, body, false),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create key"),
    }
}

async fn read_body(req: Request) -> Option<Value> {
    let body = to_bytes(req.into_body(), REQUEST_BODY_LIMIT).await.ok()?;
    serde_json::from_slice::<Value>(&body)
        .ok()
        .filter(|value| !value.is_null())
}

async fn list(State(state): State<KeyState>) -> Response {
    match state
        .storage
        .run(false, |db| {
            let mut stmt = db.prepare(&format!(
                "SELECT {KEY_COLUMNS} FROM apiKeys ORDER BY createdAt ASC"
            ))?;
            stmt.query_map([], key_row)?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
    {
        Ok(keys) => reply(StatusCode::OK, json!({ "keys": keys }), false),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch keys"),
    }
}

async fn show(State(state): State<KeyState>, Path(id): Path<String>) -> Response {
    match state.storage.run(false, move |db| by_id(db, &id)).await {
        Ok(Some(key)) => reply(StatusCode::OK, json!({ "key": key }), false),
        Ok(None) => error(StatusCode::NOT_FOUND, "Key not found"),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch key"),
    }
}

enum Updated {
    Missing,
    Invalid(String),
    Key(Value),
}

async fn update(State(state): State<KeyState>, Path(id): Path<String>, req: Request) -> Response {
    let Some(body) = read_body(req).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update key");
    };

    let updated = state.storage.run(true, move |db| {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(current) = by_id(&tx, &id)? else {
            return Ok(Updated::Missing);
        };
        let access = if let Some(value) = body.get("access") {
            let checked = validate_key_access_input(value.clone()).map_err(conversion_error)?;
            if checked["ok"] != true {
                return Ok(Updated::Invalid(checked["error"].as_str().unwrap_or_default().to_owned()));
            }
            checked["value"].clone()
        } else {
            current["access"].clone()
        };
        let cols = key_access_to_columns(access).map_err(conversion_error)?;
        let active = body.get("isActive").map_or(current["isActive"] == true, js_truthy);
        tx.execute(
            "UPDATE apiKeys SET isActive = ?1, accessRestricted = ?2, accessAllow = ?3 WHERE id = ?4",
            params![active, cols["accessRestricted"].as_i64(), cols["accessAllow"].as_str(), id],
        )?;
        let key = by_id(&tx, &id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        tx.commit()?;
        Ok(Updated::Key(key))
    }).await;

    match updated {
        Ok(Updated::Key(key)) => reply(StatusCode::OK, json!({ "key": key }), false),
        Ok(Updated::Missing) => error(StatusCode::NOT_FOUND, "Key not found"),
        Ok(Updated::Invalid(message)) => error(StatusCode::BAD_REQUEST, &message),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update key"),
    }
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

async fn remove(State(state): State<KeyState>, Path(id): Path<String>) -> Response {
    match state
        .storage
        .run(true, move |db| {
            db.execute("DELETE FROM apiKeys WHERE id = ?1", [id])
        })
        .await
    {
        Ok(0) => error(StatusCode::NOT_FOUND, "Key not found"),
        Ok(_) => reply(
            StatusCode::OK,
            json!({ "message": "Key deleted successfully" }),
            false,
        ),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to delete key"),
    }
}
