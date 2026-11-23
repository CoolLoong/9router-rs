use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{MethodRouter, get};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value, json};

use crate::{chat_usage, settings, storage::Storage};

pub fn routes() -> MethodRouter<Storage> {
    get(read).patch(update).delete(reset)
}

async fn read(State(storage): State<Storage>) -> Response {
    let result = storage.run(false, |db| user_prices(db).map(merge)).await;
    reply(result, "Failed to fetch pricing")
}

async fn update(State(storage): State<Storage>, request: Request) -> Response {
    let bytes = to_bytes(request.into_body(), 128 * 1024 * 1024).await;
    let Some(value) = bytes
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    else {
        return problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to update pricing",
        );
    };
    let updates = match validate(&value) {
        Ok(updates) => updates,
        Err(message) => return problem(StatusCode::BAD_REQUEST, &message),
    };
    let result = storage
        .run(true, move |db| {
            let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for (provider, models) in updates {
                let mut current = provider_prices(&transaction, &provider)?;
                current.extend(models);
                save(&transaction, &provider, current)?;
            }
            let updated = user_prices(&transaction)?;
            transaction.commit()?;
            Ok(updated)
        })
        .await;
    reply(result, "Failed to update pricing")
}

async fn reset(State(storage): State<Storage>, request: Request) -> Response {
    let query = request.uri().query().unwrap_or_default();
    let mut pairs = url::form_urlencoded::parse(query.as_bytes());
    let provider = pairs
        .clone()
        .find(|(key, _)| key == "provider")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty());
    let model = pairs
        .find(|(key, _)| key == "model")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty());
    let result = storage
        .run(true, move |db| {
            let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            match (provider, model) {
                (Some(provider), Some(model)) => {
                    let mut current = provider_prices(&transaction, &provider)?;
                    current.shift_remove(&model);
                    if current.is_empty() {
                        remove(&transaction, &provider)?;
                    } else {
                        save(&transaction, &provider, current)?;
                    }
                }
                (Some(provider), None) => remove(&transaction, &provider)?,
                _ => {
                    transaction.execute("DELETE FROM kv WHERE scope = 'pricing'", [])?;
                }
            }
            let current = user_prices(&transaction)?;
            transaction.commit()?;
            Ok(merge(current))
        })
        .await;
    reply(result, "Failed to reset pricing")
}

fn properties(value: &Value) -> Option<Map<String, Value>> {
    match value {
        Value::Object(fields) => Some(fields.clone()),
        Value::Array(items) => Some(
            items
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value))
                .collect(),
        ),
        _ => None,
    }
}

type ProviderPricing = (String, Map<String, Value>);

fn validate(value: &Value) -> Result<Vec<ProviderPricing>, String> {
    let providers = properties(value).ok_or("Invalid pricing data format")?;
    providers.into_iter().map(|(provider, value)| {
        let models = properties(&value).ok_or_else(|| format!("Invalid pricing for provider: {provider}"))?;
        for (model, value) in &models {
            let prices = properties(value).ok_or_else(|| format!("Invalid pricing for model: {provider}/{model}"))?;
            for (key, price) in prices {
                if !matches!(key.as_str(), "input" | "output" | "cached" | "reasoning" | "cache_creation") {
                    return Err(format!("Invalid pricing field: {key} for {provider}/{model}"));
                }
                if price.as_f64().is_none_or(|price| price < 0.0) {
                    return Err(format!("Invalid pricing value for {key} in {provider}/{model}: must be non-negative number"));
                }
            }
        }
        Ok((provider, models))
    }).collect()
}

fn user_prices(db: &Connection) -> rusqlite::Result<Value> {
    let mut rows = db.prepare("SELECT key, value FROM kv WHERE scope = 'pricing'")?;
    let rows = rows.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| row.map(|(key, text)| (key, serde_json::from_str(&text).unwrap_or(Value::Null))))
        .collect::<rusqlite::Result<Map<String, Value>>>()
        .map(Value::Object)
}

fn provider_prices(db: &Connection, provider: &str) -> rusqlite::Result<Map<String, Value>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM kv WHERE scope = 'pricing' AND key = ?",
            [provider],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .and_then(|value| properties(&value))
        .unwrap_or_default())
}

fn save(db: &Connection, provider: &str, prices: Map<String, Value>) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO kv(scope, key, value) VALUES('pricing', ?, ?) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
        rusqlite::params![provider, Value::Object(prices).to_string()],
    )?;
    Ok(())
}

fn remove(db: &Connection, provider: &str) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM kv WHERE scope = 'pricing' AND key = ?",
        [provider],
    )?;
    Ok(())
}

fn merge(user: Value) -> Value {
    let mut prices = properties(&chat_usage::rates()["provider"]).unwrap_or_default();
    for (provider, value) in properties(&user).unwrap_or_default() {
        let custom = properties(&value).unwrap_or_default();
        if let Some(defaults) = prices.get_mut(&provider).and_then(Value::as_object_mut) {
            for (model, value) in custom {
                if let Some(default) = defaults.get_mut(&model).and_then(Value::as_object_mut) {
                    default.extend(properties(&value).unwrap_or_default());
                } else {
                    defaults.insert(model, value);
                }
            }
        } else {
            prices.insert(provider, Value::Object(custom));
        }
    }
    Value::Object(prices)
}

fn problem(status: StatusCode, message: &str) -> Response {
    settings::reply(status, json!({"error":message}), false)
}

fn reply(result: Result<Value, String>, message: &str) -> Response {
    match result {
        Ok(value) => settings::reply(StatusCode::OK, value, false),
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}
