use std::collections::HashMap;

use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::{Map, Value, json};

use crate::dashboard_session;
use crate::provider_connections::{now, object};
use crate::provider_index::js_trim;
use crate::settings::reply;
use crate::storage::{Storage, is_active};

#[path = "proxy_pool_probe.rs"]
mod probe;
pub(crate) use probe::{connection_problem, settings_test};
#[path = "proxy_cloudflare_relay.rs"]
mod cloudflare;
#[path = "proxy_deno_relay.rs"]
mod deno;
#[path = "proxy_managed_relay.rs"]
pub(crate) mod managed_relay;
#[path = "proxy_netlify_relay.rs"]
mod netlify;
#[path = "proxy_vercel_relay.rs"]
mod vercel;

const BODY_LIMIT: usize = 128 * 1024 * 1024;
const COLUMNS: &str = "id, isActive, testStatus, data, createdAt, updatedAt";

#[derive(Clone)]
struct StateData {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    let state = StateData { storage };
    Router::new()
        .route("/api/proxy-pools", get(list).post(create))
        .route(
            "/api/proxy-pools/{id}",
            get(show).put(update).delete(remove),
        )
        .route("/api/proxy-pools/{id}/test", post(probe::test))
        .route(
            "/api/proxy-pools/cloudflare-deploy",
            post(cloudflare::deploy),
        )
        .route("/api/proxy-pools/deno-deploy", post(deno::deploy))
        .route("/api/proxy-pools/netlify-deploy", post(netlify::deploy))
        .route("/api/proxy-pools/vercel-deploy", post(vercel::deploy))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<StateData>, request: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, request.headers()).await {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        Json(json!({"error": "Unauthorized"})),
    )
        .into_response()
}

pub fn relay(kind: &str) -> bool {
    matches!(kind, "vercel" | "cloudflare" | "deno" | "netlify")
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<Value> {
    let raw: String = row.get(3)?;
    let mut fields = serde_json::from_str(&raw).map(object).unwrap_or_default();
    fields.insert("id".into(), json!(row.get::<_, String>(0)?));
    fields.insert("isActive".into(), json!(is_active(row.get_ref(1)?)));
    fields.insert("testStatus".into(), json!(row.get::<_, Option<String>>(2)?));
    fields.insert("createdAt".into(), json!(row.get::<_, Option<String>>(4)?));
    fields.insert("updatedAt".into(), json!(row.get::<_, Option<String>>(5)?));
    Ok(Value::Object(fields))
}

pub fn by_id(db: &Connection, id: &str) -> rusqlite::Result<Option<Value>> {
    db.query_row(
        &format!("SELECT {COLUMNS} FROM proxyPools WHERE id=?1"),
        [id],
        from_row,
    )
    .optional()
}

pub(super) fn rotation_pool_ids(
    db: &Connection,
    allow_relay: bool,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = db.prepare(&format!(
        "SELECT {COLUMNS} FROM proxyPools WHERE isActive=1 ORDER BY updatedAt DESC, rowid",
    ))?;
    let mut rows = stmt.query([])?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        let pool = from_row(row)?;
        if (allow_relay || !pool["type"].as_str().is_some_and(relay))
            && pool["proxyUrl"]
                .as_str()
                .is_some_and(|url| !url.trim().is_empty())
            && let Some(id) = pool["id"].as_str()
        {
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}

fn public_pool(mut pool: Value) -> Value {
    if let Some(fields) = pool.as_object_mut() {
        for name in managed_relay::PRIVATE_FIELDS {
            fields.shift_remove(name);
        }
    }
    pool
}

fn count_bindings(db: &Connection) -> rusqlite::Result<HashMap<String, u64>> {
    let mut statement = db.prepare(
        "SELECT pool, COUNT(*) FROM (
            SELECT CASE WHEN json_valid(data) THEN json_extract(data, '$.providerSpecificData.proxyPoolId') END AS pool
            FROM providerConnections
        ) WHERE typeof(pool)='text' GROUP BY pool",
    )?;
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

fn error(status: StatusCode, error: &str) -> Response {
    reply(status, json!({ "error": error }), false)
}

fn missing() -> Response {
    error(StatusCode::NOT_FOUND, "Proxy pool not found")
}

async fn list(State(state): State<StateData>, request: Request) -> Response {
    let query = request.uri().query().unwrap_or_default();
    let mut include_usage = None;
    let mut active = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "includeUsage" if include_usage.is_none() => include_usage = Some(value == "true"),
            "isActive" if active.is_none() => {
                active = Some(match value.as_ref() {
                    "true" => Some(1_i64),
                    "false" => Some(0),
                    _ => None,
                })
            }
            _ => (),
        }
    }
    let active = active.flatten();
    let pools = state.storage.run(false, move |db| {
        let mut stmt = db.prepare(&format!(
            "SELECT {COLUMNS} FROM proxyPools WHERE ?1 IS NULL OR isActive=?1 ORDER BY updatedAt DESC, rowid",
        ))?;
        let mut pools = stmt.query_map([active], from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        if include_usage == Some(true) {
            let bindings = count_bindings(db)?;
            for pool in &mut pools {
                let count = pool["id"].as_str().and_then(|id| bindings.get(id)).copied().unwrap_or(0);
                pool["boundConnectionCount"] = json!(count);
            }
        }
        Ok(pools)
    }).await;
    match pools {
        Ok(pools) => reply(
            StatusCode::OK,
            json!({ "proxyPools": pools.into_iter().map(public_pool).collect::<Vec<_>>() }),
            false,
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch proxy pools",
        ),
    }
}

async fn show(State(state): State<StateData>, Path(id): Path<String>) -> Response {
    match state.storage.run(false, move |db| by_id(db, &id)).await {
        Ok(Some(pool)) => reply(
            StatusCode::OK,
            json!({ "proxyPool": public_pool(pool) }),
            false,
        ),
        Ok(None) => missing(),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch proxy pool",
        ),
    }
}

fn normalize(body: &Value, create: bool) -> Result<Map<String, Value>, &'static str> {
    let mut result = Map::new();
    for (key, missing) in [
        ("name", "Name is required"),
        ("proxyUrl", "Proxy URL is required"),
    ] {
        if create || body.get(key).is_some() {
            let text = body[key]
                .as_str()
                .map(js_trim)
                .filter(|v| !v.is_empty())
                .ok_or(missing)?;
            result.insert(key.into(), json!(text));
        }
    }
    if create || body.get("noProxy").is_some() {
        result.insert(
            "noProxy".into(),
            json!(body["noProxy"].as_str().map(js_trim).unwrap_or("")),
        );
    }
    if create || body.get("isActive").is_some() {
        result.insert(
            "isActive".into(),
            json!(body.get("isActive").is_none_or(|flag| *flag == true)),
        );
    }
    if create || body.get("strictProxy").is_some() {
        result.insert("strictProxy".into(), json!(body["strictProxy"] == true));
    }
    if create || body.get("type").is_some() {
        let kind = body["type"]
            .as_str()
            .filter(|kind| relay(kind))
            .unwrap_or("http");
        result.insert("type".into(), json!(kind));
    }
    Ok(result)
}

fn persist(db: &Connection, pool: &Value) -> rusqlite::Result<()> {
    let mut extra = object(pool.clone());
    for key in ["id", "isActive", "testStatus", "createdAt", "updatedAt"] {
        extra.shift_remove(key);
    }
    db.execute(
        "INSERT INTO proxyPools (id,isActive,testStatus,data,createdAt,updatedAt)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(id) DO UPDATE SET isActive=excluded.isActive,
           testStatus=excluded.testStatus,data=excluded.data,updatedAt=excluded.updatedAt",
        params![
            pool["id"].as_str(),
            i64::from(pool["isActive"] != false),
            pool["testStatus"].as_str(),
            Value::Object(extra).to_string(),
            pool["createdAt"].as_str(),
            pool["updatedAt"].as_str()
        ],
    )?;
    Ok(())
}

async fn read_body(request: Request) -> Option<Value> {
    let bytes = to_bytes(request.into_body(), BODY_LIMIT).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn create(State(state): State<StateData>, request: Request) -> Response {
    let Some(body) = read_body(request).await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create proxy pool",
        );
    };
    let fields = match normalize(&body, true) {
        Ok(fields) => fields,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let time = now();
    let pool = json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "name": fields["name"], "proxyUrl": fields["proxyUrl"], "noProxy": fields["noProxy"],
        "type": fields["type"], "isActive": fields["isActive"], "strictProxy": fields["strictProxy"],
        "testStatus": "unknown", "lastTestedAt": null, "lastError": null,
        "createdAt": time, "updatedAt": time,
    });
    match state
        .storage
        .run(true, move |db| {
            persist(db, &pool)?;
            Ok(pool)
        })
        .await
    {
        Ok(pool) => reply(StatusCode::CREATED, json!({ "proxyPool": pool }), false),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create proxy pool",
        ),
    }
}

async fn update(
    State(state): State<StateData>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let body = read_body(request).await;
    let result = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(pool) = by_id(&tx, &id)? else {
                return Ok(Mutation::Missing);
            };
            let Some(body) = body.filter(|body| !body.is_null()) else {
                return Ok(Mutation::Error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to update proxy pool",
                ));
            };
            let changes = match normalize(&body, false) {
                Ok(changes) => changes,
                Err(message) => return Ok(Mutation::Error(StatusCode::BAD_REQUEST, message)),
            };
            if managed_relay::managed(&pool)
                && ["proxyUrl", "type"]
                    .iter()
                    .any(|key| changes.get(*key).is_some_and(|value| *value != pool[*key]))
            {
                return Ok(Mutation::Error(
                    StatusCode::CONFLICT,
                    "A managed relay URL and type cannot be changed",
                ));
            }
            let mut pool = object(pool);
            pool.extend(changes);
            pool.insert("updatedAt".into(), json!(now()));
            let pool = Value::Object(pool);
            persist(&tx, &pool)?;
            tx.commit()?;
            Ok(Mutation::Success(json!({ "proxyPool": public_pool(pool) })))
        })
        .await;
    response(result, "Failed to update proxy pool")
}

async fn remove(State(state): State<StateData>, Path(id): Path<String>) -> Response {
    let result = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if by_id(&tx, &id)?.is_none() {
                return Ok(Mutation::Missing);
            }
            if let Some(count) = count_bindings(&tx)?
                .get(&id)
                .copied()
                .filter(|count| *count > 0)
            {
                return Ok(Mutation::Blocked(count));
            }
            tx.execute("DELETE FROM proxyPools WHERE id=?1", [id])?;
            tx.commit()?;
            Ok(Mutation::Success(json!({ "success": true })))
        })
        .await;
    response(result, "Failed to delete proxy pool")
}

enum Mutation {
    Missing,
    Error(StatusCode, &'static str),
    Blocked(u64),
    Success(Value),
}

fn response(result: Result<Mutation, String>, failure: &str) -> Response {
    match result {
        Ok(Mutation::Missing) => missing(),
        Ok(Mutation::Error(status, message)) => error(status, message),
        Ok(Mutation::Blocked(count)) => reply(
            StatusCode::CONFLICT,
            json!({
                "error": "Proxy pool is currently in use", "boundConnectionCount": count,
            }),
            false,
        ),
        Ok(Mutation::Success(data)) => reply(StatusCode::OK, data, false),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, failure),
    }
}
