use std::sync::OnceLock;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value, json};

use crate::chat_output::UsageCallback;
use crate::storage::Storage;

static RATES: OnceLock<Value> = OnceLock::new();

struct UsageEntry {
    provider: String,
    model: String,
    connection_id: String,
    endpoint: String,
    key: Option<String>,
    timestamp: String,
    day: String,
    status: &'static str,
    tokens: Value,
}

pub fn callback(
    storage: Storage,
    provider: String,
    model: String,
    connection_id: String,
    key: Option<String>,
    endpoint: String,
) -> UsageCallback {
    usage_callback(
        storage,
        provider,
        model,
        connection_id,
        key,
        endpoint,
        Kind::Chat,
    )
}

pub fn exact_callback(
    storage: Storage,
    provider: String,
    model: String,
    connection_id: String,
    key: Option<String>,
    endpoint: String,
) -> UsageCallback {
    usage_callback(
        storage,
        provider,
        model,
        connection_id,
        key,
        endpoint,
        Kind::Exact,
    )
}

enum Kind {
    Chat,
    Exact,
}

fn usage_callback(
    storage: Storage,
    provider: String,
    model: String,
    connection_id: String,
    key: Option<String>,
    endpoint: String,
    kind: Kind,
) -> UsageCallback {
    Box::new(move |raw| {
        let (tokens, status) = match kind {
            Kind::Chat => {
                let tokens = normalize(&raw);
                if number(&tokens["prompt_tokens"]) == 0.0
                    && number(&tokens["completion_tokens"]) == 0.0
                {
                    return;
                }
                (tokens, "ok")
            }
            Kind::Exact => (raw, "success"),
        };
        let now = chrono::Utc::now();
        let entry = UsageEntry {
            provider,
            model,
            connection_id,
            endpoint,
            key,
            timestamp: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            day: now
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string(),
            status,
            tokens,
        };
        tokio::spawn(async move {
            if storage
                .run(true, move |db| persist(db, entry))
                .await
                .is_err()
            {
                eprintln!("Failed to save native chat usage");
            } else {
                crate::usage_activity::usage_committed();
            }
        });
    })
}

fn normalize(usage: &Value) -> Value {
    let mut prompt = numeric(usage, &["prompt_tokens", "input_tokens"]);
    let completion = numeric(usage, &["completion_tokens", "output_tokens"]);
    let reasoning = number(
        usage
            .get("reasoning_tokens")
            .unwrap_or(&usage["completion_tokens_details"]["reasoning_tokens"]),
    );
    let creation = number(
        usage
            .get("cache_creation_input_tokens")
            .unwrap_or(&usage["prompt_tokens_details"]["cache_creation_tokens"]),
    );
    let cached = if usage.get("cached_tokens").is_none()
        && (usage.get("cache_read_input_tokens").is_some()
            || usage.get("cache_creation_input_tokens").is_some())
    {
        let cached = number(&usage["cache_read_input_tokens"]);
        prompt += cached + creation;
        cached
    } else {
        number(
            usage
                .get("cached_tokens")
                .filter(|value| !value.is_null())
                .unwrap_or(&usage["prompt_tokens_details"]["cached_tokens"]),
        )
    };
    let mut tokens = json!({
        "prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion,
        "cached_tokens": cached, "cache_creation_input_tokens": creation,
    });
    if reasoning > 0.0 {
        tokens["reasoning_tokens"] = json!(reasoning);
    }
    tokens
}

fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<f64>().ok())
        })
        .or_else(|| value.as_bool().map(|flag| if flag { 1.0 } else { 0.0 }))
        .filter(|n| n.is_finite())
        .unwrap_or(0.0)
}

fn numeric(value: &Value, names: &[&str]) -> f64 {
    names
        .iter()
        .find_map(|name| value.get(*name).filter(|n| !n.is_null()))
        .map_or(0.0, number)
}

pub(crate) fn rates() -> &'static Value {
    RATES.get_or_init(|| {
        serde_json::from_str(include_str!("pricing-data.json")).expect("embedded pricing table")
    })
}

fn pricing<'a>(provider: &str, model: &str, custom: &'a Value) -> Option<&'a Value> {
    if let Some(prices) = custom.get(model).filter(|v| !v.is_null()) {
        return Some(prices);
    }
    let static_rates = rates();
    if let Some(prices) = static_rates["provider"][provider].get(model) {
        return Some(prices);
    }
    let lowercase = model.to_lowercase();
    if static_rates["free"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .any(|ns| lowercase.starts_with(ns))
    {
        return static_rates.get("zero");
    }
    let tail = model.rsplit('/').next().unwrap_or(model);
    if let Some(prices) = static_rates["model"]
        .get(tail)
        .or_else(|| static_rates["model"].get(model))
    {
        return Some(prices);
    }
    static_rates["patterns"]
        .as_array()?
        .iter()
        .find(|entry| {
            entry["pattern"].as_str().is_some_and(|pattern| {
                crate::thinking_catalog::matches_pattern(pattern, tail)
                    || crate::thinking_catalog::matches_pattern(pattern, model)
            })
        })
        .and_then(|entry| entry.get("pricing"))
}

fn cost(tokens: &Value, rates: Option<&Value>) -> f64 {
    let Some(rates) = rates else {
        return 0.0;
    };
    let (Some(input), Some(output)) = (rates["input"].as_f64(), rates["output"].as_f64()) else {
        return 0.0;
    };
    let cached = number(&tokens["cached_tokens"]);
    let creation = number(&tokens["cache_creation_input_tokens"]);
    let reasoning = number(&tokens["reasoning_tokens"]);
    let rate = |field: &str, default| {
        rates[field]
            .as_f64()
            .filter(|rate| *rate != 0.0)
            .unwrap_or(default)
            / 1_000_000.0
    };
    let mut value =
        (number(&tokens["prompt_tokens"]) - cached - creation).max(0.0) * (input / 1_000_000.0);
    if cached > 0.0 {
        value += cached * rate("cached", input);
    }
    value += number(&tokens["completion_tokens"]) * (output / 1_000_000.0);
    if reasoning > 0.0 {
        value += reasoning * rate("reasoning", output);
    }
    if creation > 0.0 {
        value += creation * rate("cache_creation", input);
    }
    value
}

fn persist(db: &mut Connection, entry: UsageEntry) -> rusqlite::Result<()> {
    let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let prompt = number(&entry.tokens["prompt_tokens"]);
    let completion = number(&entry.tokens["completion_tokens"]);
    let connection = (!entry.connection_id.is_empty()).then_some(&entry.connection_id);
    let existing = transaction.query_row(
        "SELECT id, endpoint FROM usageHistory WHERE timestamp = ?1 AND provider = ?2 AND model = ?3
         AND COALESCE(connectionId, '') = COALESCE(?4, '') AND COALESCE(apiKey, '') = COALESCE(?5, '')
         AND promptTokens = ?6 AND completionTokens = ?7 ORDER BY id DESC LIMIT 1",
        params![entry.timestamp, entry.provider, entry.model, connection, entry.key, prompt, completion],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
    ).optional()?;
    if let Some((id, endpoint)) = existing {
        if endpoint.as_deref().is_none_or(str::is_empty) {
            transaction.execute(
                "UPDATE usageHistory SET endpoint = ?1 WHERE id = ?2",
                params![entry.endpoint, id],
            )?;
        }
        return transaction.commit();
    }

    let custom: Option<String> = transaction
        .query_row(
            "SELECT value FROM kv WHERE scope = 'pricing' AND key = ?1",
            [&entry.provider],
            |row| row.get(0),
        )
        .optional()?;
    let custom: Value = custom
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let cost = cost(
        &entry.tokens,
        pricing(&entry.provider, &entry.model, &custom),
    );
    transaction.execute(
        "INSERT INTO usageHistory(timestamp, provider, model, connectionId, apiKey, endpoint, promptTokens,
            completionTokens, cost, status, tokens, meta) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'{}')",
        params![entry.timestamp, entry.provider, entry.model, connection, entry.key, entry.endpoint,
            prompt, completion, cost, entry.status, entry.tokens.to_string()],
    )?;

    let raw: Option<String> = transaction
        .query_row(
            "SELECT data FROM usageDaily WHERE dateKey = ?1",
            [&entry.day],
            |row| row.get(0),
        )
        .optional()?;
    let mut day = raw
        .and_then(|text| serde_json::from_str::<Map<String, Value>>(&text).ok())
        .unwrap_or_default();
    aggregate(&mut day, &entry, prompt, completion, cost);
    transaction.execute(
        "INSERT INTO usageDaily(dateKey,data) VALUES(?1,?2) ON CONFLICT(dateKey) DO UPDATE SET data=excluded.data",
        params![entry.day, Value::Object(day).to_string()],
    )?;
    transaction.execute(
        "INSERT INTO _meta(key,value) VALUES('totalRequestsLifetime','1')
         ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1",
        [],
    )?;
    transaction.commit()
}

fn aggregate(
    day: &mut Map<String, Value>,
    entry: &UsageEntry,
    prompt: f64,
    completion: f64,
    cost: f64,
) {
    let counts = [
        ("requests", 1.0),
        ("promptTokens", prompt),
        ("completionTokens", completion),
        ("cachedTokens", number(&entry.tokens["cached_tokens"])),
        ("cost", cost),
    ];
    add_counts(day, &counts);
    let model_data = json!({ "rawModel": entry.model, "provider": entry.provider });
    for (group, id, data) in [
        ("byProvider", entry.provider.clone(), Value::Null),
        (
            "byModel",
            format!("{}|{}", entry.model, entry.provider),
            model_data.clone(),
        ),
        ("byAccount", entry.connection_id.clone(), model_data),
        (
            "byApiKey",
            format!(
                "{}|{}|{}",
                entry.key.as_deref().unwrap_or("local-no-key"),
                entry.model,
                entry.provider
            ),
            json!({ "rawModel": entry.model, "provider": entry.provider, "apiKey": entry.key }),
        ),
        (
            "byEndpoint",
            format!("{}|{}|{}", entry.endpoint, entry.model, entry.provider),
            json!({ "endpoint": entry.endpoint, "rawModel": entry.model, "provider": entry.provider }),
        ),
    ] {
        let group = day.entry(group).or_insert_with(|| json!({}));
        if !group.is_object() {
            *group = json!({});
        }
        if id.is_empty() {
            continue;
        }
        let Some(map) = group.as_object_mut() else {
            continue;
        };
        let counter = map.entry(id).or_insert_with(|| json!({}));
        if !counter.is_object() {
            *counter = json!({});
        }
        let Some(counter) = counter.as_object_mut() else {
            continue;
        };
        add_counts(counter, &counts);
        if let Value::Object(data) = data {
            counter.extend(data);
        }
    }
}

fn add_counts(bucket: &mut Map<String, Value>, counts: &[(&str, f64)]) {
    for (field, inc) in counts {
        let previous = bucket.get(*field).map_or(0.0, number);
        bucket.insert((*field).to_owned(), json!(previous + inc));
    }
}
