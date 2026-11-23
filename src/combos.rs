use std::borrow::Cow;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::chat_output::js_text;
use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::storage::Storage;

const COLUMNS: &str = "id, name, kind, models, createdAt, updatedAt";
const BODY_LIMIT: usize = 128 * 1024 * 1024;
const INVALID_NAME: &str = "Name can only contain letters, numbers, -, _ and .";
const DUPLICATE_NAME: &str = "Combo name already exists";
const NOT_FOUND: &str = "Combo not found";

#[derive(Clone)]
struct ComboState {
    storage: Storage,
}

pub struct Combo {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub models: Value,
    pub created_at: String,
    pub updated_at: String,
}

impl Combo {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let models = match row.get_ref(3)? {
            ValueRef::Text(raw) => serde_json::from_slice(raw).unwrap_or_else(|_| json!([])),
            ValueRef::Integer(number) => json!(number),
            ValueRef::Real(number) => json!(number),
            _ => json!([]),
        };
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
            models,
            created_at: row.get(4)?,
            updated_at: row.get(5)?,
        })
    }

    fn into_json(self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "kind": self.kind,
            "models": self.models,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
        })
    }
}

pub async fn all(storage: &Storage) -> Result<Vec<Combo>, String> {
    storage
        .run(false, |db| {
            let mut statement = db.prepare(&format!(
                "SELECT {COLUMNS} FROM combos ORDER BY createdAt ASC"
            ))?;
            statement
                .query_map([], Combo::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
}

pub fn router(storage: Storage) -> Router {
    let state = ComboState { storage };
    Router::new()
        .route("/api/combos", get(list).post(create))
        .route(
            "/api/combos/presets",
            get(preview_presets).post(create_presets),
        )
        .route("/api/combos/{id}", get(show).put(update).delete(remove))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<ComboState>, request: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, request.headers()).await {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        axum::Json(json!({ "error": "Unauthorized" })),
    )
        .into_response()
}

fn reply(status: StatusCode, body: Value) -> Response {
    into_http(Ok(json!({
        "status": status.as_u16(),
        "headers": { "content-type": "application/json" },
        "body": body.to_string(),
    })))
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({ "error": message }))
}

fn by_id(db: &Connection, id: &str) -> rusqlite::Result<Option<Combo>> {
    db.query_row(
        &format!("SELECT {COLUMNS} FROM combos WHERE id = ?1"),
        [id],
        Combo::from_row,
    )
    .optional()
}

fn name_taken(db: &Connection, name: &str, except_id: Option<&str>) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM combos WHERE name = ?1 AND (?2 IS NULL OR id != ?2))",
        params![name, except_id],
        |row| row.get(0),
    )
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

async fn json_body(request: Request) -> Option<Value> {
    let bytes = to_bytes(request.into_body(), BODY_LIMIT).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn sql_text(value: &Value) -> Option<Cow<'_, str>> {
    match value {
        Value::String(value) => Some(Cow::Borrowed(value)),
        Value::Number(_) => Some(Cow::Owned(js_text(value))),
        _ => None,
    }
}

async fn list(State(state): State<ComboState>) -> Response {
    match all(&state.storage).await {
        Ok(combos) => {
            let combos: Vec<Value> = combos.into_iter().map(Combo::into_json).collect();
            reply(StatusCode::OK, json!({ "combos": combos }))
        }
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch combos"),
    }
}

async fn show(State(state): State<ComboState>, Path(id): Path<String>) -> Response {
    match state.storage.run(false, move |db| by_id(db, &id)).await {
        Ok(Some(combo)) => reply(StatusCode::OK, combo.into_json()),
        Ok(None) => error(StatusCode::NOT_FOUND, NOT_FOUND),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch combo"),
    }
}

enum Mutation {
    Duplicate,
    Missing,
    Combo(Value),
}

async fn create(State(state): State<ComboState>, request: Request) -> Response {
    let Some(body) = json_body(request).await.filter(|body| !body.is_null()) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create combo");
    };
    let Some(name) = body.get("name").filter(|value| js_truthy(value)) else {
        return error(StatusCode::BAD_REQUEST, "Name is required");
    };
    if !valid_name(&js_text(name)) {
        return error(StatusCode::BAD_REQUEST, INVALID_NAME);
    }
    let Some(stored_name) = sql_text(name).map(Cow::into_owned) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create combo");
    };
    let kind = body.get("kind").filter(|value| js_truthy(value));
    let stored_kind = match kind {
        None => None,
        Some(value) => match sql_text(value) {
            Some(value) => Some(value.into_owned()),
            None => return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create combo"),
        },
    };
    let models = body
        .get("models")
        .filter(|value| js_truthy(value))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let name = name.clone();
    let kind = kind.cloned().unwrap_or_default();
    let created = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if name_taken(&tx, &stored_name, None)? {
                return Ok(Mutation::Duplicate);
            }
            let mut combo = insert_combo(&tx, stored_name, stored_kind, models)?.into_json();
            combo["name"] = name;
            combo["kind"] = kind;
            tx.commit()?;
            Ok(Mutation::Combo(combo))
        })
        .await;

    match created {
        Ok(Mutation::Combo(combo)) => reply(StatusCode::CREATED, combo),
        Ok(Mutation::Duplicate) => error(StatusCode::BAD_REQUEST, DUPLICATE_NAME),
        _ => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create combo"),
    }
}

fn insert_combo(
    db: &rusqlite::Connection,
    name: String,
    kind: Option<String>,
    models: Value,
) -> rusqlite::Result<Combo> {
    let timestamp = now();
    let combo = Combo {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        kind,
        models,
        created_at: timestamp.clone(),
        updated_at: timestamp,
    };
    db.execute(
        "INSERT INTO combos (id, name, kind, models, createdAt, updatedAt) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![combo.id, combo.name, combo.kind, combo.models.to_string(), combo.created_at, combo.updated_at],
    )?;
    Ok(combo)
}

fn preset_source(value: &str) -> Option<&str> {
    matches!(value, "cursor" | "claude").then_some(value)
}

fn preset_rows(source: &str) -> &'static [Value] {
    static PRESETS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    PRESETS.get_or_init(|| {
        serde_json::from_str(include_str!("combo-presets.json")).expect("pinned presets")
    })[source]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn preset_names(db: &rusqlite::Connection) -> rusqlite::Result<std::collections::HashSet<String>> {
    let mut query = db.prepare("SELECT name FROM combos")?;
    query.query_map([], |row| row.get(0))?.collect()
}

async fn resolved_presets(state: &ComboState, source: &str) -> Vec<Value> {
    if source == "cursor"
        && let Ok(accounts) =
            crate::chat_accounts::active_connections(&state.storage, "cursor").await
        && let Some(account) = accounts.first()
        && let Some(settings) = state.storage.read_settings().await
        && let Some(models) =
            crate::openai_models::cursor_live(&state.storage, &settings, account, false).await
    {
        let mut seen = std::collections::HashSet::new();
        return models
            .iter()
            .filter_map(|model| {
                let id = model["id"].as_str()?;
                if id.is_empty()
                    || !id.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                    })
                    || !seen.insert(id)
                {
                    return None;
                }
                Some(json!({"name":id,"models":[format!("cu/{id}")]}))
            })
            .collect();
    }
    preset_rows(source).to_vec()
}

async fn preview_presets(State(state): State<ComboState>, request: Request) -> Response {
    let query = request.uri().query().unwrap_or_default();
    let source = url::form_urlencoded::parse(query.as_bytes())
        .find_map(|(key, value)| (key == "source").then(|| value.into_owned()));
    let Some(source) = source.filter(|source| preset_source(source).is_some()) else {
        return error(
            StatusCode::BAD_REQUEST,
            "source must be 'cursor' or 'claude'",
        );
    };
    let mut items = resolved_presets(&state, &source).await;
    let result = state.storage.run(false, move |db| {
        let existing = preset_names(db)?;
        let mut skipped = 0;
        for item in &mut items {
            let present = item["name"].as_str().is_some_and(|name| existing.contains(name));
            item["exists"] = json!(present);
            skipped += usize::from(present);
        }
        Ok(json!({"source":source,"items":items,"toCreate":items.len() - skipped,"toSkip":skipped}))
    }).await;
    match result {
        Ok(value) => reply(StatusCode::OK, value),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to preview combo presets",
        ),
    }
}

async fn create_presets(State(state): State<ComboState>, request: Request) -> Response {
    let body = json_body(request).await.unwrap_or_default();
    let Some(source) = body["source"]
        .as_str()
        .and_then(preset_source)
        .map(str::to_owned)
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "source must be 'cursor' or 'claude'",
        );
    };
    let items = resolved_presets(&state, &source).await;
    let result = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing = preset_names(&tx)?;
            let mut created = Vec::new();
            let mut skipped = Vec::new();
            for preset in &items {
                let name = preset["name"].as_str().expect("pinned preset name");
                if existing.contains(name) {
                    skipped.push(name);
                } else {
                    created.push(
                        insert_combo(&tx, name.into(), None, preset["models"].clone())?.into_json(),
                    );
                }
            }
            tx.commit()?;
            Ok(json!({"source":source,"created":created,"skipped":skipped,
            "createdCount":created.len(),"skippedCount":skipped.len()}))
        })
        .await;
    match result {
        Ok(value) => reply(StatusCode::OK, value),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create combo presets",
        ),
    }
}

async fn update(
    State(state): State<ComboState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let Some(Value::Object(body)) = json_body(request).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update combo");
    };
    if let Some(name) = body.get("name").filter(|name| js_truthy(name))
        && !valid_name(&js_text(name))
    {
        return error(StatusCode::BAD_REQUEST, INVALID_NAME);
    }

    let updated = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(name) = body
                .get("name")
                .filter(|name| js_truthy(name))
                .and_then(sql_text)
                && name_taken(&tx, &name, Some(&id))?
            {
                return Ok(Mutation::Duplicate);
            }
            let Some(current) = by_id(&tx, &id)? else {
                return Ok(Mutation::Missing);
            };
            let previous_name = current.name.clone();
            let Value::Object(mut result) = current.into_json() else {
                return Err(rusqlite::Error::InvalidQuery);
            };
            result.extend(body);
            let updated_at = now();
            result.insert("updatedAt".into(), json!(updated_at));
            let result = Value::Object(result);
            let name = sql_text(&result["name"]).ok_or(rusqlite::Error::InvalidQuery)?;
            let kind = match &result["kind"] {
                Value::Null => None,
                value => Some(sql_text(value).ok_or(rusqlite::Error::InvalidQuery)?),
            };
            let stored_models = if js_truthy(&result["models"]) {
                result["models"].to_string()
            } else {
                "[]".to_owned()
            };
            tx.execute(
                "UPDATE combos SET name = ?1, kind = ?2, models = ?3, updatedAt = ?4 WHERE id = ?5",
                params![
                    name.as_ref(),
                    kind.as_deref(),
                    stored_models,
                    updated_at,
                    id
                ],
            )?;
            tx.commit()?;
            if !previous_name.is_empty() {
                crate::combo_chat::reset(Some(&previous_name));
            }
            if !name.is_empty() && name != previous_name {
                crate::combo_chat::reset(Some(&name));
            }
            Ok(Mutation::Combo(result))
        })
        .await;

    match updated {
        Ok(Mutation::Combo(combo)) => reply(StatusCode::OK, combo),
        Ok(Mutation::Missing) => error(StatusCode::NOT_FOUND, NOT_FOUND),
        Ok(Mutation::Duplicate) => error(StatusCode::BAD_REQUEST, DUPLICATE_NAME),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update combo"),
    }
}

async fn remove(State(state): State<ComboState>, Path(id): Path<String>) -> Response {
    match state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let deleted = tx
                .query_row(
                    "DELETE FROM combos WHERE id = ?1 RETURNING name",
                    [id],
                    |row| Ok(row.get::<_, String>(0).ok()),
                )
                .optional()?;
            tx.commit()?;
            if let Some(name) = deleted.as_ref().and_then(Option::as_deref)
                && !name.is_empty()
            {
                crate::combo_chat::reset(Some(name));
            }
            Ok(deleted.is_some())
        })
        .await
    {
        Ok(false) => error(StatusCode::NOT_FOUND, NOT_FOUND),
        Ok(true) => reply(StatusCode::OK, json!({ "success": true })),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to delete combo"),
    }
}
