use std::sync::Arc;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::{
    Engine, alphabet,
    engine::DecodePaddingMode,
    engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig},
};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, TransactionBehavior, types::Value as SqlValue};
use serde_json::{Map, Value, json};

use crate::chat_output::{js_add, js_text};
use crate::dashboard_session;
use crate::provider_connections::{self as connections, truthy};
use crate::provider_index::js_trim;
use crate::provider_oauth::token_claims;
use crate::settings::reply;
use crate::storage::Storage;

const LIMIT: usize = 128 * 1024 * 1024;
const AUTH_CLAIMS: &str = "https://api.openai.com/auth";
const PROFILE_CLAIMS: &str = "https://api.openai.com/profile";
const FIELDS: &[&str] = &[
    "displayName",
    "email",
    "globalPriority",
    "defaultModel",
    "accessToken",
    "refreshToken",
    "expiresAt",
    "tokenType",
    "scope",
    "projectId",
    "apiKey",
    "testStatus",
    "lastTested",
    "lastError",
    "lastErrorAt",
    "rateLimitedUntil",
    "expiresIn",
    "errorCode",
    "consecutiveUseCount",
    "idToken",
    "lastRefreshAt",
];
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

#[derive(Clone)]
struct Import {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    let state = Import { storage };
    Router::new()
        .route("/api/oauth/codex/import-token", post(single))
        .route("/api/oauth/codex/bulk-import", post(bulk))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<Import>, request: Request, next: Next) -> Response {
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

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    reply(status, json!({"error":message.into()}), false)
}

async fn read_json(request: Request) -> Result<Value, &'static str> {
    let bytes = to_bytes(request.into_body(), LIMIT)
        .await
        .map_err(|_| "Invalid JSON input")?;
    serde_json::from_slice(&bytes).map_err(|error| {
        if error.is_eof() {
            "Unexpected end of JSON input"
        } else {
            "Invalid JSON input"
        }
    })
}

async fn single(State(state): State<Import>, request: Request) -> Response {
    let body = match read_json(request).await {
        Ok(body) => body,
        Err(message) => return error(StatusCode::INTERNAL_SERVER_ERROR, message),
    };
    if body.is_null() {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot destructure property 'accessToken' of '(intermediate value)' as it is null.",
        );
    }
    let Some(token) = body["accessToken"]
        .as_str()
        .filter(|token| !token.is_empty())
    else {
        return error(StatusCode::BAD_REQUEST, "Access token is required");
    };
    let token = js_trim(token);
    let claims = claims(token);
    let auth = &claims[AUTH_CLAIMS];
    let mut email = first(&[
        &claims[PROFILE_CLAIMS]["email"],
        &claims["email"],
        &claims["preferred_username"],
    ])
    .cloned()
    .unwrap_or_default();
    let mut details = json!({"authMethod":"access_token"});
    let details_map = details.as_object_mut().expect("created provider details");
    copy_truthy(details_map, "chatgptAccountId", &auth["chatgpt_account_id"]);
    copy_truthy(details_map, "chatgptPlanType", &auth["chatgpt_plan_type"]);
    copy_truthy(details_map, "jwtExp", &claims["exp"]);
    if !truthy(&email) {
        email = claims["email"].clone();
        if let Some(account) = first(&[&auth["chatgpt_account_id"], &claims["account_id"]]) {
            details["chatgptAccountId"] = account.clone();
        }
        if let Some(plan) = first(&[&auth["chatgpt_plan_type"], &claims["plan_type"]]) {
            details["chatgptPlanType"] = plan.clone();
        }
    }
    let name = first(&[&body["name"], &email])
        .cloned()
        .unwrap_or_else(|| json!("ChatGPT Access Token"));
    let fields = connections::object(json!({
        "accessToken":token,"name":name,"email":email,"providerSpecificData":details,"testStatus":"active",
    }));
    let stored = match save_access_token(&state.storage, fields).await {
        Ok(stored) => stored,
        Err(message) => return error(StatusCode::INTERNAL_SERVER_ERROR, message),
    };
    let secret = [token];
    reply(
        StatusCode::OK,
        json!({
            "success":true,
            "connection":{
                "id":stored["id"],
                "provider":stored["provider"],
                "email":public_field(&stored["email"], &secret),
                "name":public_field(&stored["name"], &secret),
                "workspace":public_field(&details["chatgptAccountId"], &secret),
                "plan":public_field(&details["chatgptPlanType"], &secret),
            },
        }),
        false,
    )
}

pub(super) async fn exchange_access_token(storage: &Storage, token: &str) -> Result<Value, String> {
    let claims = claims(token);
    let auth = &claims[AUTH_CLAIMS];
    let mut details = Map::from_iter([("authMethod".into(), json!("access_token"))]);
    for (name, nested, legacy) in [
        ("chatgptAccountId", "chatgpt_account_id", "account_id"),
        ("chatgptPlanType", "chatgpt_plan_type", "plan_type"),
    ] {
        if let Some(value) = first(&[&auth[nested], &claims[legacy]]) {
            details.insert(name.into(), value.clone());
        }
    }
    let fields = connections::object(json!({
        "accessToken":token,
        "email":claims.get("email").filter(|value| truthy(value)),
        "providerSpecificData":details,
        "testStatus":"active",
    }));
    let stored = save_access_token(storage, fields).await?;
    Ok(json!({"success":true,"connection":{
        "id":stored["id"],
        "provider":stored["provider"],
        "email":public_field(&stored["email"], &[token]),
    }}))
}

async fn save_access_token(storage: &Storage, fields: Map<String, Value>) -> Result<Value, String> {
    storage
        .run(true, move |db| {
            Ok(save(db, fields, "access_token", &connections::now()))
        })
        .await
        .map_err(|_| "Could not save the provider connection".to_owned())?
}

async fn bulk(State(state): State<Import>, request: Request) -> Response {
    let body = match read_json(request).await {
        Ok(body) => body,
        Err(message) => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("Invalid JSON body: {message}"),
            );
        }
    };
    let inputs = match body {
        Value::Array(accounts) => accounts,
        Value::Object(mut fields) => match fields.get_mut("accounts") {
            Some(Value::Array(accounts)) => std::mem::take(accounts),
            _ => vec![Value::Object(fields)],
        },
        _ => Vec::new(),
    };
    if inputs.is_empty() {
        return error(StatusCode::BAD_REQUEST, "No accounts provided");
    }

    let inputs = Arc::new(inputs);
    let pending = inputs.clone();
    let result = state
        .storage
        .run(true, move |db| {
            Ok(pending
                .iter()
                .enumerate()
                .map(|(index, raw)| {
                    let now = Utc::now();
                    let stamp = now.to_rfc3339_opts(SecondsFormat::Millis, true);
                    let result = bulk_item(raw, now, &stamp)
                        .and_then(|fields| save(db, fields, "oauth", &stamp));
                    item_result(index, result)
                })
                .collect::<Vec<_>>())
        })
        .await;
    let results = result.unwrap_or_else(|_| {
        inputs
            .iter()
            .enumerate()
            .map(|(index, raw)| {
                let now = Utc::now();
                let stamp = now.to_rfc3339_opts(SecondsFormat::Millis, true);
                let rejected = bulk_item(raw, now, &stamp)
                    .err()
                    .unwrap_or_else(|| "Could not save the provider connection".into());
                item_result(index, Err(rejected))
            })
            .collect()
    });
    let success = results.iter().filter(|item| item["ok"] == true).count();
    reply(
        StatusCode::OK,
        json!({"success":success,"failed":results.len() - success,"results":results}),
        false,
    )
}

fn item_result(index: usize, result: Result<Value, String>) -> Value {
    match result {
        Ok(stored) => json!({"index":index,"ok":true,"id":stored["id"]}),
        Err(message) => json!({"index":index,"ok":false,"error":message}),
    }
}

fn bulk_item(raw: &Value, now: DateTime<Utc>, stamp: &str) -> Result<Map<String, Value>, String> {
    let mut item = raw.as_object().cloned().ok_or("Item is not an object")?;
    for field in ["id", "provider", "authType", "createdAt", "updatedAt"] {
        item.shift_remove(field);
    }
    let access = item
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or("Missing accessToken")?;
    let explicit = item
        .get("providerSpecificData")
        .filter(|details| truthy(details));
    let mut details = match explicit {
        Some(details) => details
            .as_object()
            .cloned()
            .ok_or("Invalid providerSpecificData")?,
        None => Map::new(),
    };
    let need_email = !item.get("email").is_some_and(truthy);
    let need_workspace = !details.get("chatgptAccountId").is_some_and(truthy);
    let need_plan = !details.get("chatgptPlanType").is_some_and(truthy);
    if need_email || need_workspace || need_plan {
        let claims = match item.get("idToken").filter(|token| truthy(token)) {
            Some(token) => token.as_str().map_or(Value::Null, claims),
            None => claims(access),
        };
        let auth = &claims[AUTH_CLAIMS];
        if need_email {
            copy_truthy(&mut item, "email", &claims["email"]);
        }
        if need_workspace
            && let Some(value) = first(&[&auth["chatgpt_account_id"], &claims["account_id"]])
        {
            details.insert("chatgptAccountId".into(), value.clone());
        }
        if need_plan && let Some(value) = first(&[&auth["chatgpt_plan_type"], &claims["plan_type"]])
        {
            details.insert("chatgptPlanType".into(), value.clone());
        }
    }
    if !details.is_empty() {
        item.insert("providerSpecificData".into(), Value::Object(details));
    }
    if !item.get("expiresAt").is_some_and(truthy)
        && let Some(seconds) = item
            .get("expiresIn")
            .and_then(Value::as_f64)
            .filter(|seconds| *seconds > 0.0)
    {
        let millis = now.timestamp_millis() as f64 + seconds * 1000.0;
        if !millis.is_finite() || millis.abs() > 8_640_000_000_000_000.0 {
            return Err("Invalid time value".into());
        }
        let date =
            DateTime::<Utc>::from_timestamp_millis(millis as i64).ok_or("Invalid time value")?;
        item.insert(
            "expiresAt".into(),
            json!(date.to_rfc3339_opts(SecondsFormat::Millis, true)),
        );
    }
    item.entry("testStatus").or_insert(json!("active"));
    item.entry("isActive").or_insert(json!(true));
    if !item.get("lastRefreshAt").is_some_and(truthy) {
        item.insert("lastRefreshAt".into(), json!(stamp));
    }
    Ok(item)
}

fn claims(token: &str) -> Value {
    let decoded = token_claims(token);
    if !decoded.is_null() {
        return decoded;
    }
    let mut parts = token.split('.');
    let _header = parts.next();
    let Some(payload) = parts
        .next()
        .filter(|_| parts.next().is_some() && parts.next().is_none())
    else {
        return Value::Null;
    };
    let payload: Vec<_> = payload
        .bytes()
        .take_while(|byte| *byte != b'=')
        .filter_map(|byte| match byte {
            b'-' => Some(b'+'),
            b'_' => Some(b'/'),
            b'+' | b'/' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => Some(byte),
            _ => None,
        })
        .collect();
    BASE64
        .decode(payload)
        .ok()
        .and_then(|bytes| serde_json::from_str(&String::from_utf8_lossy(&bytes)).ok())
        .unwrap_or_default()
}

fn first<'a>(values: &[&'a Value]) -> Option<&'a Value> {
    values.iter().copied().find(|value| truthy(value))
}

fn copy_truthy(target: &mut Map<String, Value>, name: &str, value: &Value) {
    if truthy(value) {
        target.insert(name.into(), value.clone());
    }
}

fn same_workspace(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        _ => false,
    }
}

fn existing(db: &Connection, fields: &Map<String, Value>) -> rusqlite::Result<Option<Value>> {
    let Some(email) = fields
        .get("email")
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
    else {
        return Ok(None);
    };
    let Some(workspace) = fields
        .get("providerSpecificData")
        .and_then(|metadata| metadata.get("chatgptAccountId"))
        .filter(|workspace| truthy(workspace))
    else {
        return Ok(None);
    };
    let mut statement = db.prepare(
        "SELECT id,data FROM providerConnections
         WHERE provider='codex' AND authType='oauth' AND email=?1 ORDER BY rowid",
    )?;
    let mut rows = statement.query([email])?;
    while let Some(row) = rows.next()? {
        let data: String = row.get(1)?;
        let details: Value = serde_json::from_str(&data).unwrap_or_default();
        if same_workspace(
            workspace,
            &details["providerSpecificData"]["chatgptAccountId"],
        ) {
            return connections::by_id(db, &row.get::<_, String>(0)?);
        }
    }
    Ok(None)
}

fn save(
    db: &mut Connection,
    mut fields: Map<String, Value>,
    auth_type: &str,
    now: &str,
) -> Result<Value, String> {
    const STORAGE_ERROR: &str = "Could not save the provider connection";
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| STORAGE_ERROR)?;
    let matched = if auth_type == "oauth" {
        existing(&tx, &fields).map_err(|_| STORAGE_ERROR)?
    } else {
        None
    };
    fields.insert("provider".into(), json!("codex"));
    fields.insert("authType".into(), json!(auth_type));
    let connection = match matched {
        Some(previous) => {
            if fields.get("allowOverwrite") == Some(&Value::Bool(false)) {
                let secrets = ["accessToken", "refreshToken", "idToken", "apiKey"];
                let private: Vec<_> = secrets
                    .iter()
                    .flat_map(|field| [previous.get(*field), fields.get(*field)])
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let name = public_field(&previous["name"], &private);
                return Err(format!(
                    "A connection named \"{}\" already exists for provider \"codex\". Pass allowOverwrite: true to replace it.",
                    js_text(&name),
                ));
            }
            connections::patch(previous, fields, now)
        }
        None => new_connection(&tx, &fields, auth_type, now).map_err(|_| STORAGE_ERROR)?,
    };
    connections::write(&tx, &connection).map_err(|_| STORAGE_ERROR)?;
    tx.commit().map_err(|_| STORAGE_ERROR)?;
    Ok(connection)
}

fn new_connection(
    db: &Connection,
    fields: &Map<String, Value>,
    auth_type: &str,
    now: &str,
) -> rusqlite::Result<Value> {
    let name = fields
        .get("name")
        .filter(|name| truthy(name))
        .or_else(|| fields.get("email").filter(|email| truthy(email)))
        .cloned();
    let name = match name {
        Some(name) => name,
        None => {
            let number: usize = db.query_row(
                "SELECT count(*) FROM providerConnections WHERE provider='codex'",
                [],
                |row| row.get(0),
            )?;
            json!(format!("Account {}", number + 1))
        }
    };
    let priority = match fields.get("priority").filter(|priority| truthy(priority)) {
        Some(priority) => priority.clone(),
        None => {
            let maximum: SqlValue = db.query_row(
                "SELECT max(priority) FROM providerConnections WHERE provider='codex'",
                [],
                |row| row.get(0),
            )?;
            let maximum = match maximum {
                SqlValue::Integer(value) => json!(value),
                SqlValue::Real(value) => json!(value),
                SqlValue::Text(value) => json!(value),
                _ => json!(0),
            };
            js_add(&maximum, &json!(1))
        }
    };
    let mut row = connections::object(json!({
        "id":uuid::Uuid::new_v4().to_string(),"provider":"codex","authType":auth_type,
        "name":name,"priority":priority,"isActive":fields.get("isActive").unwrap_or(&Value::Bool(true)),
        "createdAt":now,"updatedAt":now,
    }));
    for field in FIELDS {
        if let Some(value) = fields.get(*field).filter(|value| !value.is_null()) {
            row.insert((*field).into(), value.clone());
        }
    }
    if let Some(details) = fields
        .get("providerSpecificData")
        .and_then(Value::as_object)
        .filter(|details| !details.is_empty())
    {
        row.insert("providerSpecificData".into(), json!(details));
    }
    if let Some(email) = fields.get("email") {
        row.insert("email".into(), email.clone());
    }
    Ok(Value::Object(row))
}

fn public_field(value: &Value, secrets: &[&str]) -> Value {
    match value {
        Value::String(value) => {
            let mut visible = value.clone();
            for secret in secrets {
                if !secret.is_empty() && visible.contains(secret) {
                    visible = visible.replace(secret, "[redacted]");
                }
            }
            json!(visible)
        }
        Value::Number(_) | Value::Bool(_) => value.clone(),
        _ => Value::Null,
    }
}
