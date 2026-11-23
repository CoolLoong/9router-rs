use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::{Map, Value, json};

use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::provider_connections::{self as connections, now, object, truthy};
use crate::provider_index::js_trim;
use crate::storage::Storage;

#[path = "provider_node_probe.rs"]
mod probe;

const COLS: &str = "id, type, name, data, createdAt, updatedAt";
const BODY_LIMIT: usize = 128 * 1024 * 1024;
const OPENAI: &str = "openai-compatible";
const ANTHROPIC: &str = "anthropic-compatible";
const EMBEDDING: &str = "custom-embedding";
const NOT_FOUND: &str = "Provider node not found";
const INVALID_API_TYPE: &str = "Invalid OpenAI compatible API type";
const CREATE_ERROR: &str = "Failed to create provider node";
const UPDATE_ERROR: &str = "Failed to update provider node";

type Node = Map<String, Value>;

#[derive(Clone)]
struct NodeState {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    let state = NodeState { storage };
    Router::new()
        .route("/api/provider-nodes", get(list).post(create))
        .route("/api/provider-nodes/validate", post(probe::run))
        .route("/api/provider-nodes/{id}", put(update).delete(remove))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<NodeState>, req: Request, next: Next) -> Response {
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

fn read_row(row: &Row<'_>) -> rusqlite::Result<Node> {
    let data: Option<String> = row.get(3)?;
    let mut node = spread(
        data.as_deref()
            .and_then(|data| serde_json::from_str(data).ok())
            .unwrap_or_default(),
    );
    node.insert("id".into(), json!(row.get::<_, String>(0)?));
    node.insert("type".into(), json!(row.get::<_, Option<String>>(1)?));
    node.insert("name".into(), json!(row.get::<_, Option<String>>(2)?));
    node.insert("createdAt".into(), json!(row.get::<_, String>(4)?));
    node.insert("updatedAt".into(), json!(row.get::<_, String>(5)?));
    Ok(node)
}

fn spread(value: Value) -> Node {
    match value {
        Value::String(text) => text
            .chars()
            .enumerate()
            .map(|(index, character)| (index.to_string(), json!(character.to_string())))
            .collect(),
        value => object(value),
    }
}

pub(crate) fn by_id(db: &Connection, id: &str) -> rusqlite::Result<Option<Node>> {
    db.query_row(
        &format!("SELECT {COLS} FROM providerNodes WHERE id = ?1"),
        [id],
        read_row,
    )
    .optional()
}

fn write(db: &Connection, node: &Node) -> rusqlite::Result<()> {
    let mut data = node.clone();
    for key in ["id", "type", "name", "createdAt", "updatedAt"] {
        data.shift_remove(key);
    }
    let text = |key| node.get(key).and_then(Value::as_str);
    db.execute(
        "INSERT INTO providerNodes(id, type, name, data, createdAt, updatedAt)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
           type = excluded.type, name = excluded.name, data = excluded.data, updatedAt = excluded.updatedAt",
        params![
            text("id"),
            text("type"),
            text("name"),
            Value::Object(data).to_string(),
            text("createdAt"),
            text("updatedAt"),
        ],
    )?;
    Ok(())
}

async fn read_body(req: Request) -> Option<Value> {
    let bytes = to_bytes(req.into_body(), BODY_LIMIT).await.ok()?;
    let raw = String::from_utf8_lossy(&bytes);
    serde_json::from_str(raw.strip_prefix('\u{feff}').unwrap_or(&raw)).ok()
}

enum Validation {
    Invalid(&'static str),
    InvalidType,
}

impl Validation {
    fn response(self, internal: &str) -> Response {
        match self {
            Self::Invalid(message) => error(StatusCode::BAD_REQUEST, message),
            Self::InvalidType => error(StatusCode::INTERNAL_SERVER_ERROR, internal),
        }
    }
}

fn required<'a>(body: &'a Value, key: &str, message: &'static str) -> Result<&'a str, Validation> {
    match body.get(key) {
        None | Some(Value::Null) => Err(Validation::Invalid(message)),
        Some(Value::String(text)) if js_trim(text).is_empty() => Err(Validation::Invalid(message)),
        Some(Value::String(text)) => Ok(js_trim(text)),
        _ => Err(Validation::InvalidType),
    }
}

fn api_type(body: &Value, kind: &str) -> Result<Option<&'static str>, Validation> {
    if kind != OPENAI {
        return Ok(None);
    }
    match body["apiType"].as_str() {
        Some("chat") => Ok(Some("chat")),
        Some("responses") => Ok(Some("responses")),
        _ => Err(Validation::Invalid(INVALID_API_TYPE)),
    }
}

fn sanitize_base<'a>(kind: &str, base: &'a str) -> &'a str {
    let suffix = match kind {
        ANTHROPIC => "/messages",
        EMBEDDING => "/embeddings",
        _ => return base,
    };
    let bare = base.strip_suffix('/').unwrap_or(base);
    bare.strip_suffix(suffix).unwrap_or(bare)
}

fn new_node(body: &Value) -> Result<Node, Validation> {
    if body.is_null() {
        return Err(Validation::InvalidType);
    }
    let name = required(body, "name", "Name is required")?;
    let prefix = required(body, "prefix", "Prefix is required")?;
    let kind = body.get("type").filter(|value| truthy(value));
    let kind = match kind {
        None => OPENAI,
        Some(Value::String(value)) if matches!(value.as_str(), OPENAI | ANTHROPIC | EMBEDDING) => {
            value
        }
        _ => return Err(Validation::Invalid("Invalid provider node type")),
    };
    let api = api_type(body, kind)?;
    let base = match body.get("baseUrl").filter(|value| truthy(value)) {
        None if kind == ANTHROPIC => "https://api.anthropic.com/v1",
        None => "https://api.openai.com/v1",
        Some(Value::String(text)) => js_trim(text),
        _ => return Err(Validation::InvalidType),
    };
    let uuid = uuid::Uuid::new_v4();
    let id = api.map_or_else(
        || format!("{kind}-{uuid}"),
        |api| format!("{kind}-{api}-{uuid}"),
    );
    let timestamp = now();
    let mut node = object(json!({
        "id": id, "type": kind, "name": name, "prefix": prefix, "apiType": api,
        "baseUrl": sanitize_base(kind, base), "createdAt": timestamp, "updatedAt": timestamp,
    }));
    if api.is_none() {
        node.shift_remove("apiType");
    }
    Ok(node)
}

async fn list(State(state): State<NodeState>) -> Response {
    let result = state
        .storage
        .run(false, |db| {
            let mut statement = db.prepare(&format!("SELECT {COLS} FROM providerNodes"))?;
            statement
                .query_map([], read_row)?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await;
    match result {
        Ok(nodes) => respond(StatusCode::OK, json!({ "nodes": nodes })),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch provider nodes",
        ),
    }
}

async fn create(State(state): State<NodeState>, req: Request) -> Response {
    let Some(body) = read_body(req).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, CREATE_ERROR);
    };
    let node = match new_node(&body) {
        Ok(node) => node,
        Err(validation) => return validation.response(CREATE_ERROR),
    };
    match state
        .storage
        .run(true, move |db| {
            write(db, &node)?;
            Ok(node)
        })
        .await
    {
        Ok(node) => respond(StatusCode::CREATED, json!({ "node": node })),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, CREATE_ERROR),
    }
}

fn apply_update(node: &mut Node, body: &Value) -> Result<Option<&'static str>, Validation> {
    let name = required(body, "name", "Name is required")?;
    let prefix = required(body, "prefix", "Prefix is required")?;
    let kind = node.get("type").and_then(Value::as_str).unwrap_or_default();
    let api = api_type(body, kind)?;
    let base = required(body, "baseUrl", "Base URL is required")?;
    let base = sanitize_base(kind, base);
    node.insert("name".into(), json!(name));
    node.insert("prefix".into(), json!(prefix));
    node.insert("baseUrl".into(), json!(base));
    if let Some(api) = api {
        node.insert("apiType".into(), json!(api));
    }
    node.insert("updatedAt".into(), json!(now()));
    Ok(api)
}

fn connections_decode_without_loss(db: &Connection, id: &str) -> rusqlite::Result<bool> {
    let mut statement = db.prepare("SELECT data FROM providerConnections WHERE provider = ?1")?;
    let rows = statement.query_map([id], |row| row.get::<_, String>(0))?;
    for row in rows {
        let data = row?;
        if !matches!(serde_json::from_str::<Value>(&data), Ok(Value::Object(_))) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn update_connections(
    db: &Connection,
    id: &str,
    node: &Node,
    api: Option<&str>,
) -> rusqlite::Result<()> {
    for connection in connections::read_all(db, Some(id))? {
        let raw = &connection["providerSpecificData"];
        let stored = raw
            .as_str()
            .and_then(|value| serde_json::from_str::<Value>(value).ok())
            .filter(Value::is_object);
        let mut config = spread(stored.unwrap_or_else(|| raw.clone()));
        config.insert("prefix".into(), node["prefix"].clone());
        if let Some(api) = api {
            config.insert("apiType".into(), json!(api));
        } else {
            config.shift_remove("apiType");
        }
        config.insert("baseUrl".into(), node["baseUrl"].clone());
        config.insert("nodeName".into(), node["name"].clone());
        let patch = object(json!({ "providerSpecificData": config }));
        let conn = connections::patch(connection, patch, &now());
        connections::write(db, &conn)?;
    }
    Ok(())
}

enum Update {
    Missing,
    Unreadable,
    Invalid(Validation),
    Node(Node),
}

fn update_node(db: &mut Connection, id: &str, body: &Value) -> rusqlite::Result<Update> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut node = match by_id(&tx, id) {
        Ok(Some(node)) => node,
        Ok(None) => return Ok(Update::Missing),
        Err(
            rusqlite::Error::FromSqlConversionFailure(..) | rusqlite::Error::InvalidColumnType(..),
        ) => return Ok(Update::Unreadable),
        Err(error) => return Err(error),
    };
    let api = match apply_update(&mut node, body) {
        Ok(api) => api,
        Err(validation) => return Ok(Update::Invalid(validation)),
    };
    match connections_decode_without_loss(&tx, id) {
        Ok(true) => {}
        Ok(false) | Err(rusqlite::Error::InvalidColumnType(..)) => return Ok(Update::Unreadable),
        Err(error) => return Err(error),
    }
    write(&tx, &node)?;
    update_connections(&tx, id, &node, api)?;
    tx.commit()?;
    Ok(Update::Node(node))
}

async fn update(State(state): State<NodeState>, Path(id): Path<String>, req: Request) -> Response {
    let Some(body) = read_body(req).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, UPDATE_ERROR);
    };
    if body.is_null() {
        return error(StatusCode::INTERNAL_SERVER_ERROR, UPDATE_ERROR);
    }
    match state
        .storage
        .run(true, move |db| update_node(db, &id, &body))
        .await
    {
        Ok(Update::Node(node)) => respond(StatusCode::OK, json!({ "node": node })),
        Ok(Update::Missing) => error(StatusCode::NOT_FOUND, NOT_FOUND),
        Ok(Update::Unreadable) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read the saved provider node or connection",
        ),
        Ok(Update::Invalid(validation)) => validation.response(UPDATE_ERROR),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, UPDATE_ERROR),
    }
}

async fn remove(State(state): State<NodeState>, Path(id): Path<String>) -> Response {
    let result = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if tx.execute("DELETE FROM providerNodes WHERE id = ?1", [&id])? == 0 {
                return Ok(false);
            }
            tx.execute("DELETE FROM providerConnections WHERE provider = ?1", [&id])?;
            tx.execute(
                "DELETE FROM kv WHERE scope = 'customModels' AND substr(key, 1, length(?1)) = ?1",
                [format!("{id}|")],
            )?;
            tx.commit()?;
            Ok(true)
        })
        .await;
    match result {
        Ok(true) => respond(StatusCode::OK, json!({ "success": true })),
        Ok(false) => error(StatusCode::NOT_FOUND, NOT_FOUND),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to delete provider node",
        ),
    }
}
