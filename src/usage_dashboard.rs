use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use rusqlite::{Connection, params_from_iter};
use serde_json::{Map, Value, json};

use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::provider_index::{self, js_trim};
use crate::storage::Storage;

#[path = "usage_statistics.rs"]
mod calculations;

#[derive(Clone)]
struct DashboardUsage {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    let state = DashboardUsage { storage };
    Router::new()
        .route("/api/usage/history", get(history))
        .route("/api/usage/stats", get(stats))
        .route("/api/usage/chart", get(chart))
        .route("/api/usage/providers", get(providers))
        .route("/api/usage/request-details", get(details))
        .route("/api/usage/logs", get(logs))
        .route("/api/usage/request-logs", get(logs))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<DashboardUsage>, request: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, request.headers()).await {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        Json(json!({"error":"Unauthorized"})),
    )
        .into_response()
}

fn response(status: StatusCode, value: Value) -> Response {
    into_http(Ok(json!({
        "status":status.as_u16(),
        "headers":{"content-type":"application/json"},
        "body":value.to_string(),
    })))
}

fn error(status: StatusCode, message: &str) -> Response {
    response(status, json!({"error":message}))
}

fn query(request: &Request, name: &str) -> Option<String> {
    url::form_urlencoded::parse(request.uri().query()?.as_bytes())
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
        .filter(|value| !value.is_empty())
}

fn period(request: &Request) -> Option<String> {
    let sample = query(request, "period").unwrap_or_else(|| "7d".into());
    matches!(
        sample.as_str(),
        "today" | "24h" | "7d" | "30d" | "60d" | "all"
    )
    .then_some(sample)
}

pub async fn statistics(storage: &Storage, period: &str) -> Result<Value, String> {
    let period = period.to_owned();
    let mut stats = storage
        .run(false, move |db| {
            calculations::statistics(db, &period, Utc::now())
        })
        .await?;
    crate::usage_activity::apply(&mut stats);
    Ok(stats)
}

async fn stats(State(state): State<DashboardUsage>, request: Request) -> Response {
    let Some(period) = period(&request) else {
        return error(StatusCode::BAD_REQUEST, "Invalid period");
    };
    stats_response(&state.storage, &period).await
}

async fn history(State(state): State<DashboardUsage>) -> Response {
    stats_response(&state.storage, "all").await
}

async fn stats_response(storage: &Storage, period: &str) -> Response {
    match statistics(storage, period).await {
        Ok(stats) => response(StatusCode::OK, stats),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch usage stats",
        ),
    }
}

async fn chart(State(state): State<DashboardUsage>, request: Request) -> Response {
    let Some(period) = period(&request) else {
        return error(StatusCode::BAD_REQUEST, "Invalid period");
    };
    match state
        .storage
        .run(false, move |db| {
            calculations::chart(db, &period, Utc::now())
        })
        .await
    {
        Ok(value) => response(StatusCode::OK, value),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch chart data",
        ),
    }
}

async fn logs(State(state): State<DashboardUsage>) -> Response {
    response(
        StatusCode::OK,
        state
            .storage
            .run(false, read_logs)
            .await
            .unwrap_or_else(|_| json!([])),
    )
}

fn read_logs(db: &mut Connection) -> rusqlite::Result<Value> {
    let mut statement = db.prepare(
        "SELECT timestamp, provider, model, connectionId, promptTokens, completionTokens, status, tokens
         FROM usageHistory ORDER BY id DESC LIMIT 200",
    )?;
    let logs = statement
        .query_map([], |row| {
            let time: String = row.get(0)?;
            let provider: Option<String> = row.get(1)?;
            let model: Option<String> = row.get(2)?;
            let account: Option<String> = row.get(3)?;
            let status: Option<String> = row.get(6)?;
            let tokens = parse_json(row.get::<_, Option<String>>(7)?.as_deref());
            let sent = calculations::scalar(row.get_ref(4)?);
            let received = calculations::scalar(row.get_ref(5)?);
            let as_text = |v: Value, key: &str| {
                let value = if v.is_null() {
                    tokens
                        .get(key)
                        .filter(|value| !value.is_null())
                        .cloned()
                        .unwrap_or_else(|| json!("-"))
                } else {
                    v
                };
                crate::chat_output::js_text(&value)
            };
            Ok((
                time,
                provider,
                model,
                account,
                status,
                as_text(sent, "prompt_tokens"),
                as_text(received, "completion_tokens"),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if logs.is_empty() {
        return Ok(json!([]));
    }
    let names = calculations::connection_names(db, false).unwrap_or_default();
    let lines: Vec<_> = logs
        .into_iter()
        .map(|(time, provider, model, account, status, sent, received)| {
            let time = parse_timestamp(&time)
                .map(|time| {
                    time.with_timezone(&Local)
                        .format("%d-%m-%Y %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_else(|| "NaN-NaN-NaN NaN:NaN:NaN".to_owned());
            let provider = provider
                .filter(|value| !value.is_empty())
                .map(|value| value.to_uppercase())
                .unwrap_or_else(|| "-".into());
            let model = model
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "-".into());
            let account = account
                .filter(|value| !value.is_empty())
                .map(|id| {
                    names
                        .get(&id)
                        .filter(|value| !value.is_empty())
                        .cloned()
                        .unwrap_or_else(|| short_prefix(&id, 8))
                })
                .unwrap_or_else(|| "-".into());
            let status = status
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "-".into());
            format!("{time} | {model} | {provider} | {account} | {sent} | {received} | {status}")
        })
        .collect();
    Ok(json!(lines))
}

async fn providers(State(state): State<DashboardUsage>) -> Response {
    match state.storage.run(false, |db| {
        let mut statement = db.prepare("SELECT DISTINCT provider FROM requestDetails WHERE provider IS NOT NULL ORDER BY provider ASC")?;
        let providers = statement.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let names = calculations::provider_names(db)?;
        let rows: Vec<_> = providers.into_iter().map(|id| {
            let name = names.get(&id).cloned()
                .or_else(|| provider_index::entries().iter()
                    .find(|provider| provider["alias"] == id || provider["id"] == id)
                    .and_then(|provider| provider["name"].as_str()).map(str::to_owned))
                .unwrap_or_else(|| id.clone());
            json!({"id":id,"name":name})
        }).collect();
        Ok(json!({"providers":rows}))
    }).await {
        Ok(value) => response(StatusCode::OK, value),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch providers"),
    }
}

fn query_integer(value: Option<String>, default: i64) -> i64 {
    let Some(value) = value
        .as_deref()
        .map(js_trim)
        .filter(|value| !value.is_empty())
    else {
        return default;
    };
    let (negative, raw) = if let Some(raw) = value.strip_prefix('-') {
        (true, raw)
    } else {
        (false, value.strip_prefix('+').unwrap_or(value))
    };
    let (radix, raw) = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .map_or((10, raw), |raw| (16, raw));
    let length = raw
        .chars()
        .take_while(|c| c.is_ascii() && c.is_digit(radix))
        .count();
    if length == 0 {
        return default;
    }
    let text = &raw[..length];
    let parsed = i64::from_str_radix(text, radix).unwrap_or(i64::MAX);
    if negative { -parsed } else { parsed }
}

async fn details(State(state): State<DashboardUsage>, request: Request) -> Response {
    let page = query_integer(query(&request, "page"), 1);
    let page_size = query_integer(query(&request, "pageSize"), 20);
    if page < 1 {
        return error(StatusCode::BAD_REQUEST, "Page must be >= 1");
    }
    if !(1..=100).contains(&page_size) {
        return error(
            StatusCode::BAD_REQUEST,
            "PageSize must be between 1 and 100",
        );
    }
    let mut conditions = Vec::new();
    let mut values = Vec::new();
    for key in ["provider", "model", "connectionId", "status"] {
        if let Some(value) = query(&request, key) {
            conditions.push(format!("{key} = ?"));
            values.push(rusqlite::types::Value::Text(value));
        }
    }
    for (key, relation) in [("startDate", ">="), ("endDate", "<=")] {
        if let Some(value) = query(&request, key) {
            let Some(timestamp) = parse_timestamp(&value) else {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to fetch request details",
                );
            };
            conditions.push(format!("timestamp {relation} ?"));
            values.push(rusqlite::types::Value::Text(iso(timestamp)));
        }
    }
    let Some(offset) = (page - 1).checked_mul(page_size) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch request details",
        );
    };
    let where_sql = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    match state.storage.run(false, move |db| {
        let count = db.query_row(
            &format!("SELECT COUNT(*) FROM requestDetails {where_sql}"),
            params_from_iter(&values),
            |row| row.get::<_, i64>(0),
        )?;
        let pages = count / page_size + i64::from(count % page_size != 0);
        values.push(rusqlite::types::Value::Integer(page_size));
        values.push(rusqlite::types::Value::Integer(offset));
        let mut statement = db.prepare(&format!(
            "SELECT data FROM requestDetails {where_sql} ORDER BY timestamp DESC LIMIT ? OFFSET ?"
        ))?;
        let entries = statement.query_map(params_from_iter(&values), |row| row.get::<_, String>(0))?
            .map(|raw| raw.map(|raw| redact_details(parse_json(Some(&raw)))))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({
            "details":entries,
            "pagination":{"page":page,"pageSize":page_size,"totalItems":count,"totalPages":pages,
                          "hasNext":page<pages,"hasPrev":page>1},
        }))
    }).await {
        Ok(value) => response(StatusCode::OK, value),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch request details"),
    }
}

fn redact_details(value: Value) -> Value {
    let mut fields = match value {
        Value::Object(fields) => fields,
        Value::Array(values) => values
            .into_iter()
            .enumerate()
            .map(|(index, value)| (index.to_string(), value))
            .collect(),
        Value::String(text) => text
            .chars()
            .enumerate()
            .map(|(index, c)| (index.to_string(), json!(c.to_string())))
            .collect(),
        _ => Map::new(),
    };
    for key in ["request", "providerRequest", "providerResponse", "response"] {
        if let Some(payload) = fields.get_mut(key) {
            *payload = json!({"redacted":true});
        }
    }
    Value::Object(fields)
}

fn parse_json(raw: Option<&str>) -> Value {
    raw.and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or_else(|| json!({}))
}

fn short_prefix(text: &str, len: usize) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(len).collect::<Vec<_>>())
}

fn iso(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let raw = js_trim(raw);
    if let Ok(timestamp) =
        DateTime::parse_from_rfc3339(raw).or_else(|_| DateTime::parse_from_rfc2822(raw))
    {
        return Some(timestamp.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc());
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(local) = NaiveDateTime::parse_from_str(raw, format) {
            return Local
                .from_local_datetime(&local)
                .earliest()
                .map(|time| time.with_timezone(&Utc));
        }
    }
    None
}
