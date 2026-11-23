use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::to_bytes;
use axum::extract::{Extension, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use key_access_rs::port::{
    key_access_from_columns, key_access_to_columns, validate_key_access_input,
};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params, params_from_iter};
use serde_json::{Map, Value, json};

use crate::catalog_guard::valid_cli_token;
use crate::chat_output::js_text;
use crate::dashboard_session::{has_session, verify_password};
use crate::provider_connections::{now, object, truthy};
use crate::settings::reply;
use crate::storage::Storage;

const BODY_LIMIT: usize = 128 * 1024 * 1024;
const VERSION_TTL: Duration = Duration::from_secs(3600);
const VERSION_TIMEOUT: Duration = Duration::from_secs(4);
const VERSION_URL: &str = "https://registry.npmjs.org/9router/latest";
const VERSION_FILE: &str = include_str!("../../../package.json");
const PASSWORD_HEADER: &str = "x-9r-password";
const CLI_HEADER: &str = "x-9r-cli-token";
const KV_OBJECTS: &[&str] = &["modelAliases", "mitmAlias", "pricing"];

#[derive(Clone)]
struct AdminState {
    storage: Storage,
    version: Arc<Mutex<Option<Version>>>,
    registry: reqwest::Client,
}

struct Version {
    latest: String,
    fetched: Instant,
}

#[derive(Clone, Copy)]
enum Access {
    Cli,
    Dashboard,
}

pub fn router(storage: Storage) -> Router {
    let registry = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(VERSION_TIMEOUT)
        .build()
        .expect("registry HTTP client");
    let state = AdminState {
        storage,
        version: Arc::new(Mutex::new(None)),
        registry,
    };
    Router::new()
        .route("/api/settings/database", get(export).post(restore))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .route("/api/version", get(version))
        .with_state(state)
}

async fn authorize(State(state): State<AdminState>, mut req: Request, next: Next) -> Response {
    let headers = req.headers();
    let access = if valid_cli_token(&state.storage, headers) {
        Access::Cli
    } else if has_session(&state.storage, headers) {
        Access::Dashboard
    } else {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::VARY, "Accept-Encoding")],
            Json(json!({ "error": "Unauthorized" })),
        )
            .into_response();
    };
    if matches!(access, Access::Dashboard) {
        req.headers_mut().remove(CLI_HEADER);
    }
    req.extensions_mut().insert(access);
    next.run(req).await
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    reply(status, json!({ "error": message.into() }), false)
}

fn raw_settings(db: &Connection) -> rusqlite::Result<Value> {
    let raw: Option<SqlValue> = db
        .query_row("SELECT data FROM settings WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    match raw {
        Some(SqlValue::Text(text)) => serde_json::from_str(&text).map_err(json_column_error),
        Some(SqlValue::Integer(value)) => Ok(json!(value)),
        Some(SqlValue::Real(value)) => Ok(json!(value)),
        _ => Ok(json!({})),
    }
}

fn conversion_error(message: String) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(message)))
}

fn json_column_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn permitted(access: Access, settings: &Value, password: Option<&Value>) -> rusqlite::Result<bool> {
    if matches!(access, Access::Cli) {
        Ok(true)
    } else {
        verify_password(settings, password).map_err(conversion_error)
    }
}

async fn export(
    State(state): State<AdminState>,
    Extension(access): Extension<Access>,
    req: Request,
) -> Response {
    let password = req
        .headers()
        .get(PASSWORD_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(|text| json!(text));
    let result = state
        .storage
        .run(false, move |db| {
            let tx = db.transaction()?;
            let settings = raw_settings(&tx)?;
            if !permitted(access, &settings, password.as_ref())? {
                return Ok(None);
            }
            let value = export_db(&tx, settings)?;
            tx.commit()?;
            Ok(Some(value))
        })
        .await;
    match result {
        Ok(Some(value)) => reply(StatusCode::OK, value, false),
        Ok(None) => error(StatusCode::UNAUTHORIZED, "Invalid password"),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to export database",
        ),
    }
}

enum Restored {
    Success,
    InvalidPassword,
}

async fn restore(
    State(state): State<AdminState>,
    Extension(access): Extension<Access>,
    req: Request,
) -> Response {
    let Ok(bytes) = to_bytes(req.into_body(), BODY_LIMIT).await else {
        return error(StatusCode::BAD_REQUEST, "Invalid database payload");
    };
    let Ok(Value::Object(mut payload)) = serde_json::from_slice(&bytes) else {
        return error(StatusCode::BAD_REQUEST, "Invalid database payload");
    };
    let password = payload.shift_remove("password");
    let result = state
        .storage
        .run(true, move |db| {
            restore_db(db, access, password.as_ref(), &payload)
        })
        .await;
    match result {
        Ok(Restored::Success) => {
            crate::combo_chat::reset(None);
            crate::quota_auto_ping::wake();
            reply(StatusCode::OK, json!({ "success": true }), false)
        }
        Ok(Restored::InvalidPassword) => error(StatusCode::UNAUTHORIZED, "Invalid password"),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

fn restore_db(
    db: &mut Connection,
    access: Access,
    password: Option<&Value>,
    payload: &Map<String, Value>,
) -> rusqlite::Result<Restored> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = raw_settings(&tx)?;
    if !permitted(access, &current, password)? {
        return Ok(Restored::InvalidPassword);
    }
    let settings = payload.get("settings").filter(|value| truthy(value));

    tx.execute_batch(
        "DELETE FROM settings;
         DELETE FROM providerConnections;
         DELETE FROM providerNodes;
         DELETE FROM proxyPools;
         DELETE FROM apiKeys;
         DELETE FROM combos;
         DELETE FROM kv WHERE scope IN ('modelAliases', 'customModels', 'mitmAlias', 'pricing');",
    )?;
    if let Some(settings) = settings {
        tx.execute(
            "INSERT INTO settings(id, data) VALUES (1, ?1)",
            [settings.to_string()],
        )?;
    }
    for table in TABLES {
        for value in values(payload, table.name)? {
            import_row(&tx, table, value)?;
        }
    }
    for (scope, default_object) in [
        ("modelAliases", false),
        ("mitmAlias", true),
        ("pricing", true),
    ] {
        if let Some(value) = payload.get(scope) {
            for (key, value) in object(value.clone()) {
                let value = if default_object && !truthy(&value) {
                    json!({})
                } else {
                    value
                };
                put_kv(&tx, scope, &key, &value)?;
            }
        }
    }
    for model in values(payload, "customModels")? {
        if model.is_null() {
            return Err(conversion_error("Invalid custom model".into()));
        }
        let field = |key| model.get(key).map_or_else(|| "undefined".into(), js_text);
        let kind = model
            .get("type")
            .filter(|value| truthy(value))
            .map_or("llm".into(), js_text);
        let key = format!("{}|{}|{kind}", field("providerAlias"), field("id"));
        put_kv(&tx, "customModels", &key, model)?;
    }
    tx.commit()?;
    Ok(Restored::Success)
}

fn values<'a>(payload: &'a Map<String, Value>, key: &str) -> rusqlite::Result<&'a [Value]> {
    match payload.get(key).filter(|value| truthy(value)) {
        None => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        Some(_) => Err(conversion_error(
            "object is not iterable (cannot read property Symbol(Symbol.iterator))".into(),
        )),
    }
}

fn put_kv(db: &Connection, scope: &str, key: &str, value: &Value) -> rusqlite::Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO kv(scope, key, value) VALUES(?1, ?2, ?3)",
        params![scope, strip_nul(key), value.to_string()],
    )?;
    Ok(())
}

struct Table {
    name: &'static str,
    columns: &'static [&'static str],
    extra_data: bool,
}

const TABLES: &[Table] = &[
    Table {
        name: "providerConnections",
        columns: &[
            "id",
            "provider",
            "authType",
            "name",
            "email",
            "priority",
            "isActive",
            "createdAt",
            "updatedAt",
        ],
        extra_data: true,
    },
    Table {
        name: "providerNodes",
        columns: &["id", "type", "name", "createdAt", "updatedAt"],
        extra_data: true,
    },
    Table {
        name: "proxyPools",
        columns: &["id", "isActive", "testStatus", "createdAt", "updatedAt"],
        extra_data: true,
    },
    Table {
        name: "apiKeys",
        columns: &["id", "key", "name", "machineId", "isActive", "createdAt"],
        extra_data: false,
    },
    Table {
        name: "combos",
        columns: &["id", "name", "kind", "models", "createdAt", "updatedAt"],
        extra_data: false,
    },
];

fn strip_nul(text: &str) -> String {
    text.replace('\0', "")
}

fn scalar(value: &Value) -> rusqlite::Result<SqlValue> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::String(text) => Ok(SqlValue::Text(strip_nul(text))),
        Value::Number(number) => {
            let value = number.as_f64().ok_or(rusqlite::Error::InvalidQuery)?;
            if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
                Ok(SqlValue::Integer(value as i64))
            } else {
                Ok(SqlValue::Real(value))
            }
        }
        _ => Err(conversion_error(
            "SQLite3 can only bind numbers, strings, bigints, buffers, and null".into(),
        )),
    }
}

fn value_or(value: &Value, fallback: Value) -> Value {
    if truthy(value) {
        value.clone()
    } else {
        fallback
    }
}

fn import_field(table: &Table, field: &str, item: &Value) -> rusqlite::Result<SqlValue> {
    let value = &item[field];
    match field {
        "isActive" => Ok(SqlValue::Integer(i64::from(value != false))),
        "createdAt" | "updatedAt" => scalar(&value_or(value, json!(now()))),
        "authType" => scalar(&value_or(value, json!("oauth"))),
        "testStatus" => scalar(&value_or(value, json!("unknown"))),
        "models" => Ok(SqlValue::Text(value_or(value, json!([])).to_string())),
        "id" | "provider" | "key" => scalar(value),
        "name" if table.name == "combos" => scalar(value),
        _ => scalar(&value_or(value, Value::Null)),
    }
}

fn import_row(db: &Connection, table: &Table, item: &Value) -> rusqlite::Result<()> {
    if item.is_null() {
        return Err(conversion_error(format!("Invalid {} row", table.name)));
    }
    let mut columns = table.columns.to_vec();
    let mut args = columns
        .iter()
        .map(|column| import_field(table, column, item))
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if table.extra_data {
        let mut data = object(item.clone());
        for core in table.columns {
            data.shift_remove(*core);
        }
        columns.push("data");
        args.push(SqlValue::Text(Value::Object(data).to_string()));
    }
    if table.name == "apiKeys" {
        let access = if let Some(access) = item.get("access") {
            let checked = validate_key_access_input(access.clone()).map_err(conversion_error)?;
            if checked["ok"] != true {
                let id = item.get("id").map_or_else(|| "undefined".into(), js_text);
                return Err(conversion_error(format!(
                    "apiKeys {id}: {}",
                    js_text(&checked["error"])
                )));
            }
            checked["value"].clone()
        } else {
            Value::Null
        };
        let values = key_access_to_columns(access).map_err(conversion_error)?;
        columns.extend(["accessRestricted", "accessAllow"]);
        args.push(scalar(&values["accessRestricted"])?);
        args.push(scalar(&values["accessAllow"])?);
    }
    let placeholders = vec!["?"; args.len()].join(", ");
    db.execute(
        &format!(
            "INSERT OR REPLACE INTO {} ({}) VALUES ({placeholders})",
            table.name,
            columns.join(", ")
        ),
        params_from_iter(args),
    )?;
    Ok(())
}

fn sql_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => json!(value),
        ValueRef::Real(value) => json!(value),
        ValueRef::Text(value) => json!(String::from_utf8_lossy(value)),
        ValueRef::Blob(value) => json!({ "type": "Buffer", "data": value }),
    }
}

fn parsed_json(value: ValueRef<'_>, fallback: Value) -> rusqlite::Result<Value> {
    match value {
        ValueRef::Text(text) => serde_json::from_slice(text).map_err(json_column_error),
        ValueRef::Null => Ok(fallback),
        value => Ok(sql_json(value)),
    }
}

fn export_row(row: &Row<'_>, table: &Table) -> rusqlite::Result<Value> {
    let mut data = if table.extra_data {
        object(parsed_json(row.get_ref("data")?, json!({}))?)
    } else {
        Map::new()
    };
    for column in table.columns {
        let value = row.get_ref(*column)?;
        let value = match *column {
            "isActive" => json!(matches!(value, ValueRef::Integer(1))),
            "models" => parsed_json(value, json!([]))?,
            _ => sql_json(value),
        };
        data.insert((*column).into(), value);
    }
    if table.name == "apiKeys" {
        let restricted = sql_json(row.get_ref("accessRestricted")?);
        let allow = sql_json(row.get_ref("accessAllow")?);
        data.insert(
            "access".into(),
            key_access_from_columns(restricted, allow).map_err(conversion_error)?,
        );
    }
    Ok(Value::Object(data))
}

fn export_db(db: &Connection, settings: Value) -> rusqlite::Result<Value> {
    let mut value = Map::new();
    value.insert("settings".into(), settings);
    for table in TABLES {
        let mut query = db.prepare(&format!("SELECT * FROM {}", table.name))?;
        let rows = query
            .query_map([], |row| export_row(row, table))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        value.insert(table.name.into(), Value::Array(rows));
    }

    let mut query = db.prepare("SELECT key, value FROM kv WHERE scope = ?1")?;
    for scope in ["modelAliases", "customModels", "mitmAlias", "pricing"] {
        let mut rows = query
            .query_map([scope], |row| -> rusqlite::Result<_> {
                Ok((
                    row.get::<_, String>(0)?,
                    parsed_json(row.get_ref(1)?, Value::Null)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let contents = if KV_OBJECTS.contains(&scope) {
            rows.retain(|(key, _)| key != "__proto__");
            rows.sort_by_key(|(key, _)| {
                key.parse::<u32>()
                    .ok()
                    .filter(|value| value.to_string() == *key)
                    .unwrap_or(u32::MAX)
            });
            Value::Object(rows.into_iter().collect())
        } else {
            Value::Array(rows.into_iter().map(|(_, value)| value).collect())
        };
        value.insert(scope.into(), contents);
    }
    Ok(Value::Object(value))
}

async fn version(State(state): State<AdminState>) -> Response {
    if cfg!(all(
        any(target_os = "linux", target_os = "macos", windows),
        feature = "native-package"
    )) {
        #[cfg(all(
            any(target_os = "linux", target_os = "macos", windows),
            feature = "native-package"
        ))]
        return crate::native_update_source::version().await;
    }
    let current = serde_json::from_str::<Value>(VERSION_FILE)
        .ok()
        .and_then(|package| package["version"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let cached = state
        .version
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .filter(|version| version.fetched.elapsed() < VERSION_TTL)
        .map(|version| version.latest.clone());
    let latest = match cached {
        Some(version) => Some(version),
        None => {
            let fresh = registry_version(&state.registry).await;
            if let Some(latest) = &fresh {
                *state
                    .version
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(Version {
                    latest: latest.clone(),
                    fetched: Instant::now(),
                });
            }
            fresh
        }
    };
    let update = latest
        .as_deref()
        .is_some_and(|latest| newer(latest, &current));
    reply(
        StatusCode::OK,
        json!({
            "currentVersion": current, "latestVersion": latest, "hasUpdate": update,
        }),
        false,
    )
}

async fn registry_version(client: &reqwest::Client) -> Option<String> {
    let mut response = client.get(VERSION_URL).send().await.ok()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len().saturating_add(chunk.len()) > 64 * 1024 {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice::<Value>(&bytes).ok()?["version"]
        .as_str()
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
}

fn newer(a: &str, b: &str) -> bool {
    let mut a = a.split('.');
    let mut b = b.split('.');
    let parse = |part: Option<&str>| {
        part.map(str::trim)
            .map(|part| {
                if part.is_empty() {
                    0.0
                } else {
                    part.parse().unwrap_or(f64::NAN)
                }
            })
            .unwrap_or(f64::NAN)
    };
    for _ in 0..3 {
        let a = parse(a.next());
        let b = parse(b.next());
        if a > b {
            return true;
        }
        if a < b {
            return false;
        }
    }
    false
}
