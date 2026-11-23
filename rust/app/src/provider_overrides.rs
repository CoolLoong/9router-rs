use std::sync::OnceLock;

use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value, json};

use crate::provider_connections::object;
use crate::providers::ProviderState;
use crate::settings::reply;

const LIMIT: usize = 20;
const VALUE_LIMIT: usize = 8192;
const BODY_LIMIT: usize = 1024 * 1024;
const BLOCKED: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "connection",
    "transfer-encoding",
    "authorization",
    "cookie",
    "x-relay-target",
    "x-relay-path",
];
static CATALOG: OnceLock<Value> = OnceLock::new();

fn catalog() -> &'static Value {
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("provider-headers.json"))
            .expect("pinned Provider headers")
    })
}

fn canonical(id: &str) -> &str {
    catalog()["aliases"][id].as_str().unwrap_or(id)
}

pub(crate) async fn show(State(state): State<ProviderState>, Path(id): Path<String>) -> Response {
    let id = canonical(&id);
    match state.storage.read_settings().await {
        Some(settings) => reply(
            StatusCode::OK,
            json!({
                "headers":settings["providerOverrides"][id]["headers"].as_object().cloned().unwrap_or_default(),
                "builtinHeaders":catalog()["builtin"][id].as_object().cloned().unwrap_or_default(),
            }),
            true,
        ),
        None => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to get overrides"),
    }
}

pub(crate) async fn store(
    State(state): State<ProviderState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let input = match to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value,
            Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid JSON body"),
        },
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid JSON body"),
    };
    let fields = match normalize(&input["headers"]) {
        Ok(fields) => fields,
        Err(reason) => return error(StatusCode::BAD_REQUEST, reason),
    };
    let id = canonical(&id).to_owned();
    let input = fields.clone();
    let saved = state.storage.run(true, move |db| {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw: Option<String> = tx.query_row("SELECT data FROM settings WHERE id=1", [], |row| row.get(0)).optional()?;
        let mut settings = object(raw.and_then(|value| serde_json::from_str(&value).ok()).unwrap_or_default());
        let mut overrides = object(settings.shift_remove("providerOverrides").unwrap_or_default());
        if input.is_empty() {
            overrides.shift_remove(&id);
        } else {
            overrides.insert(id, json!({"headers":input}));
        }
        settings.insert("providerOverrides".into(), Value::Object(overrides));
        tx.execute(
            "INSERT INTO settings (id,data) VALUES (1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
            [Value::Object(settings).to_string()],
        )?;
        tx.commit()
    }).await;
    match saved {
        Ok(()) => reply(StatusCode::OK, json!({"headers":fields}), true),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save overrides",
        ),
    }
}

fn normalize(raw: &Value) -> Result<Map<String, Value>, String> {
    if raw.is_null() {
        return Ok(Map::new());
    }
    let fields = raw.as_object().ok_or("headers must be an object")?;
    let fields: Vec<_> = fields
        .iter()
        .filter(|(_, value)| !value.is_null() && *value != "")
        .collect();
    if fields.len() > LIMIT {
        return Err(format!("Too many headers (max {LIMIT})"));
    }
    let mut clean = Map::new();
    for (name, value) in fields {
        if header_name(name).is_none() {
            return Err(format!("Invalid header name: {name}"));
        }
        let value_text = value
            .as_str()
            .ok_or_else(|| format!("Invalid value for header {name}"))?;
        if value_text.contains(['\r', '\n']) || header_value(value_text).is_none() {
            return Err(format!("Invalid value for header {name}"));
        }
        if value_text.encode_utf16().count() > VALUE_LIMIT {
            return Err(format!("Header {name} value too long (max {VALUE_LIMIT})"));
        }
        if blocked(name) {
            return Err(format!("Header {name} cannot be overridden"));
        }
        clean.insert(name.clone(), value.clone());
    }
    Ok(clean)
}

fn header_name(name: &str) -> Option<HeaderName> {
    name.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        .then(|| HeaderName::from_bytes(name.as_bytes()).ok())
        .flatten()
}

fn header_value(value: &str) -> Option<HeaderValue> {
    let mut header = if value.is_ascii() {
        HeaderValue::from_str(value).ok()?
    } else {
        let latin1 = value
            .chars()
            .map(|ch| u8::try_from(ch as u32))
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        HeaderValue::from_bytes(&latin1).ok()?
    };
    header.set_sensitive(true);
    Some(header)
}

fn blocked(name: &str) -> bool {
    BLOCKED.iter().any(|value| name.eq_ignore_ascii_case(value))
}

pub(crate) fn transport_headers(settings: &Value, provider: &str) -> HeaderMap {
    if matches!(
        provider,
        "github" | "vertex" | "vertex-partner" | "bedrock" | "bedrock-xai"
    ) {
        return HeaderMap::new();
    }
    let mut headers = HeaderMap::new();
    if let Some(fields) = settings["providerOverrides"][provider]["headers"].as_object() {
        for (name, value) in fields.iter().take(LIMIT) {
            let Some(value) = value
                .as_str()
                .filter(|value| !value.is_empty() && value.encode_utf16().count() <= VALUE_LIMIT)
            else {
                continue;
            };
            if !blocked(name)
                && let (Some(name), Some(value)) = (header_name(name), header_value(value))
            {
                headers.insert(name, value);
            }
        }
    }
    headers
}

fn error(status: StatusCode, error: impl Into<String>) -> Response {
    reply(status, json!({"error":error.into()}), true)
}
