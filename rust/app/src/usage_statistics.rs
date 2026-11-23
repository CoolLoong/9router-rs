use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Utc};
use rusqlite::{Connection, Row, params, params_from_iter, types::ValueRef};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::chat_output::{js_add, js_text};
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;

use super::{iso, parse_json, parse_timestamp, short_prefix};

const CORE: &[&str] = &[
    "requests",
    "promptTokens",
    "completionTokens",
    "cachedTokens",
    "cost",
];
const GLOBAL: &[&str] = &["promptTokens", "completionTokens", "cachedTokens", "cost"];
const MINUTE: &[&str] = &["requests", "promptTokens", "completionTokens", "cost"];
const HIST_COLUMNS: &str = "timestamp, provider, model, connectionId, apiKey, endpoint, promptTokens, completionTokens, cost, tokens";
const LOCAL: &str = "local-no-key";

pub(super) fn scalar(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Integer(value) => json!(value),
        ValueRef::Real(value) if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 => {
            json!(value as i64)
        }
        ValueRef::Real(value) => json!(value),
        ValueRef::Text(value) => json!(String::from_utf8_lossy(value)),
        _ => Value::Null,
    }
}

pub(super) fn connection_names(
    db: &Connection,
    fallback_to_id: bool,
) -> rusqlite::Result<HashMap<String, String>> {
    let mut statement = db.prepare("SELECT id, name, email FROM providerConnections")?;
    statement
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let name: Option<String> = row.get(1)?;
            let email: Option<String> = row.get(2)?;
            let name = name
                .filter(|value| !value.is_empty())
                .or_else(|| email.filter(|value| !value.is_empty()))
                .unwrap_or_else(|| {
                    if fallback_to_id {
                        id.clone()
                    } else {
                        String::new()
                    }
                });
            Ok((id, name))
        })?
        .collect()
}

pub(super) fn provider_names(db: &Connection) -> rusqlite::Result<HashMap<String, String>> {
    let mut statement =
        db.prepare("SELECT id, name FROM providerNodes WHERE name IS NOT NULL AND name <> ''")?;
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

struct Labels {
    connections: HashMap<String, String>,
    providers: HashMap<String, String>,
    keys: HashMap<String, String>,
}

impl Labels {
    fn from_db(db: &Connection) -> Self {
        let keys = (|| -> rusqlite::Result<HashMap<String, String>> {
            let mut statement =
                db.prepare("SELECT key, name FROM apiKeys ORDER BY createdAt ASC")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            Ok(rows
                .filter_map(|row| match row {
                    Ok((key, Some(name))) if !key.is_empty() && !name.is_empty() => {
                        Some((js_trim(&key).to_owned(), name))
                    }
                    _ => None,
                })
                .collect())
        })()
        .unwrap_or_default();
        Self {
            connections: connection_names(db, true).unwrap_or_default(),
            providers: provider_names(db).unwrap_or_default(),
            keys,
        }
    }

    fn provider(&self, provider: Option<&str>) -> Value {
        match provider {
            Some(provider) => json!(
                self.providers
                    .get(provider)
                    .map_or(provider, String::as_str)
            ),
            None => Value::Null,
        }
    }

    fn account(&self, id: &str) -> String {
        self.connections
            .get(id)
            .filter(|name| !name.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Account {}...", short_prefix(id, 8)))
    }

    fn key_name(&self, key: Option<&str>, masked: Option<&str>) -> String {
        let Some(key) = key else {
            return "Local (No API Key)".to_owned();
        };
        self.keys
            .get(js_trim(key))
            .cloned()
            .or_else(|| masked.map(str::to_owned))
            .unwrap_or_else(|| format!("{}...", short_prefix(key, 8)))
    }
}

struct History {
    timestamp: String,
    provider: Option<String>,
    model: Option<String>,
    connection: Option<String>,
    key: Option<String>,
    endpoint: Option<String>,
    prompt: Value,
    completion: Value,
    cost: Value,
    tokens: Value,
}

impl History {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let tokens: Option<String> = row.get(9)?;
        Ok(Self {
            timestamp: row.get(0)?,
            provider: row.get(1)?,
            model: row.get(2)?,
            connection: row.get(3)?,
            key: row.get(4)?,
            endpoint: row.get(5)?,
            prompt: scalar(row.get_ref(6)?),
            completion: scalar(row.get_ref(7)?),
            cost: scalar(row.get_ref(8)?),
            tokens: parse_json(tokens.as_deref()),
        })
    }

    fn model_key(&self) -> String {
        if let Some(provider) = self.provider.as_deref().filter(|value| !value.is_empty()) {
            format!("{} ({provider})", self.model.as_deref().unwrap_or("null"))
        } else {
            self.model.clone().unwrap_or_else(|| "null".into())
        }
    }

    fn account_key(&self, account: &str) -> String {
        format!(
            "{} ({} - {account})",
            self.model.as_deref().unwrap_or("null"),
            self.provider.as_deref().unwrap_or("null")
        )
    }

    fn group_key(&self) -> String {
        match self.key.as_deref().filter(|key| !key.is_empty()) {
            Some(key) => format!(
                "{key}|{}|{}",
                self.model.as_deref().unwrap_or("null"),
                self.provider
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("unknown")
            ),
            None => LOCAL.to_owned(),
        }
    }

    fn endpoint(&self) -> &str {
        self.endpoint
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("Unknown")
    }

    fn endpoint_key(&self) -> String {
        format!(
            "{}|{}|{}",
            self.endpoint(),
            self.model.as_deref().unwrap_or("null"),
            self.provider
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or("unknown")
        )
    }

    fn token_or_columns(&self) -> Value {
        let prompt = first(
            [
                &self.tokens["prompt_tokens"],
                &self.tokens["input_tokens"],
                &self.prompt,
            ],
            json!(0),
        );
        let completion = first(
            [
                &self.tokens["completion_tokens"],
                &self.tokens["output_tokens"],
                &self.completion,
            ],
            json!(0),
        );
        let cached = first(
            [
                &self.tokens["cached_tokens"],
                &self.tokens["cache_read_input_tokens"],
            ],
            json!(0),
        );
        json!({
            "requests":1,"promptTokens":prompt,"completionTokens":completion,"cachedTokens":cached,
            "cost":one(&self.cost),
        })
    }
}

fn records(db: &Connection, tail: &str, time: &[String]) -> rusqlite::Result<Vec<History>> {
    let mut statement = db.prepare(&format!("SELECT {HIST_COLUMNS} FROM usageHistory {tail}"))?;
    statement
        .query_map(params_from_iter(time), History::from_row)?
        .collect()
}

fn first<'a>(values: impl IntoIterator<Item = &'a Value>, default: Value) -> Value {
    values
        .into_iter()
        .find(|value| truthy(value))
        .cloned()
        .unwrap_or(default)
}

fn one(value: &Value) -> Value {
    first([value], json!(0))
}

fn nonempty(value: &Value) -> Option<String> {
    truthy(value).then(|| js_text(value))
}

fn mask(key: &str) -> Option<String> {
    let key = js_trim(key);
    if key.is_empty() {
        return None;
    }
    let utf16: Vec<_> = key.encode_utf16().collect();
    Some(if utf16.len() <= 12 {
        format!("{}***", String::from_utf16_lossy(&utf16[..1]))
    } else {
        format!(
            "{}***{}",
            String::from_utf16_lossy(&utf16[..8]),
            String::from_utf16_lossy(&utf16[utf16.len() - 4..])
        )
    })
}

fn empty_counter(fields: &[&str]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|field| ((*field).to_owned(), json!(0)))
            .collect(),
    )
}

fn bump(target: &mut Value, delta: &Value, fields: &[&str]) {
    for &field in fields {
        target[field] = js_add(&target[field], &one(&delta[field]));
    }
}

fn aggregate(group: &mut Map<String, Value>, key: String, delta: &Value, metadata: Value) {
    let metadata = metadata.as_object().cloned().unwrap_or_default();
    let stamp = metadata
        .get("lastUsed")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let row = group.entry(key).or_insert_with(|| {
        let mut row = empty_counter(CORE);
        if let Some(fields) = row.as_object_mut() {
            fields.extend(metadata);
        }
        row
    });
    bump(row, delta, CORE);
    if let Some(stamp) = stamp
        && row["lastUsed"]
            .as_str()
            .is_none_or(|old| stamp.as_str() > old)
    {
        row["lastUsed"] = json!(stamp);
    }
}

fn overlay(group: &mut Map<String, Value>, key: &str, stamp: &str) {
    if let Some(row) = group.get_mut(key)
        && let (Some(previous), Some(current)) = (
            row["lastUsed"].as_str().and_then(parse_timestamp),
            parse_timestamp(stamp),
        )
        && current > previous
    {
        row["lastUsed"] = json!(stamp);
    }
}

fn recent(db: &Connection) -> rusqlite::Result<Vec<Value>> {
    let mut statement = db.prepare("SELECT timestamp, provider, model, tokens, status FROM usageHistory ORDER BY id DESC LIMIT 100")?;
    let rows = statement.query_map([], |row| {
        let tokens: Option<String> = row.get(3)?;
        let tokens = parse_json(tokens.as_deref());
        let provider: Option<String> = row.get(1)?;
        let status: Option<String> = row.get(4)?;
        Ok(json!({
            "timestamp":row.get::<_,String>(0)?,
            "model":row.get::<_,Option<String>>(2)?,
            "provider":provider.unwrap_or_default(),
            "promptTokens":first([&tokens["prompt_tokens"], &tokens["input_tokens"]], json!(0)),
            "completionTokens":first([&tokens["completion_tokens"], &tokens["output_tokens"]], json!(0)),
            "cachedTokens":first([&tokens["cached_tokens"], &tokens["cache_read_input_tokens"]], json!(0)),
            "status":status.filter(|status| !status.is_empty()).unwrap_or_else(|| "ok".into()),
        }))
    })?;
    let mut seen = HashSet::new();
    let mut recent = Vec::new();
    for row in rows {
        let row = row?;
        if row["promptTokens"].as_f64() == Some(0.0)
            && row["completionTokens"].as_f64() == Some(0.0)
        {
            continue;
        }
        let minute = short_prefix(row["timestamp"].as_str().unwrap_or_default(), 16);
        let identity = format!(
            "{}|{}|{}|{}|{minute}",
            js_text(&row["model"]),
            js_text(&row["provider"]),
            js_text(&row["promptTokens"]),
            js_text(&row["completionTokens"])
        );
        if seen.insert(identity) {
            recent.push(row);
            if recent.len() == 20 {
                break;
            }
        }
    }
    Ok(recent)
}

struct Stats {
    totals: Value,
    providers: Map<String, Value>,
    models: Map<String, Value>,
    accounts: Map<String, Value>,
    keys: Map<String, Value>,
    endpoints: Map<String, Value>,
}

impl Stats {
    fn new() -> Self {
        Self {
            totals: empty_counter(GLOBAL),
            providers: Map::new(),
            models: Map::new(),
            accounts: Map::new(),
            keys: Map::new(),
            endpoints: Map::new(),
        }
    }

    fn daily(&mut self, label: &Labels, date: &str, day: &Value) {
        bump(&mut self.totals, day, GLOBAL);
        for (key, row) in entries(&day["byProvider"]) {
            aggregate(&mut self.providers, key.clone(), row, Value::Null);
        }
        for (key, row) in entries(&day["byModel"]) {
            let mut pieces = key.split('|');
            let model = nonempty(&row["rawModel"])
                .unwrap_or_else(|| pieces.next().unwrap_or_default().to_owned());
            let provider = nonempty(&row["provider"])
                .unwrap_or_else(|| key.split('|').nth(1).unwrap_or_default().to_owned());
            let title = if provider.is_empty() {
                model.clone()
            } else {
                format!("{model} ({provider})")
            };
            aggregate(
                &mut self.models,
                title,
                row,
                json!({"rawModel":model,"provider":label.provider(Some(&provider)),"lastUsed":date}),
            );
        }
        for (id, row) in entries(&day["byAccount"]) {
            let model = nonempty(&row["rawModel"]).unwrap_or_default();
            let provider = nonempty(&row["provider"]).unwrap_or_default();
            let account = label.account(id);
            let title = format!("{model} ({provider} - {account})");
            aggregate(
                &mut self.accounts,
                title,
                row,
                json!({
                    "rawModel":model,"provider":label.provider(Some(&provider)),
                    "connectionId":id,"accountName":account,"lastUsed":date,
                }),
            );
        }
        for (key, row) in entries(&day["byApiKey"]) {
            let model = nonempty(&row["rawModel"]).unwrap_or_default();
            let provider = nonempty(&row["provider"]).unwrap_or_default();
            let real = row["apiKey"]
                .as_str()
                .map(js_trim)
                .filter(|value| !value.is_empty());
            let masked = real.and_then(mask);
            let name = label.key_name(real, masked.as_deref());
            aggregate(
                &mut self.keys,
                key.clone(),
                row,
                json!({
                    "rawModel":model,"provider":label.provider(Some(&provider)),"apiKeyMasked":masked,
                    "keyName":name,"apiKeyKey":masked.as_deref().unwrap_or(LOCAL),"lastUsed":date,
                }),
            );
        }
        for (key, row) in entries(&day["byEndpoint"]) {
            let endpoint = nonempty(&row["endpoint"]).unwrap_or_else(|| {
                key.split('|')
                    .next()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("Unknown")
                    .to_owned()
            });
            let model = nonempty(&row["rawModel"]).unwrap_or_default();
            let provider = nonempty(&row["provider"]).unwrap_or_default();
            aggregate(
                &mut self.endpoints,
                key.clone(),
                row,
                json!({
                    "endpoint":endpoint,"rawModel":model,"provider":label.provider(Some(&provider)),"lastUsed":date,
                }),
            );
        }
    }

    fn history(&mut self, label: &Labels, row: &History) {
        let vals = row.token_or_columns();
        bump(&mut self.totals, &vals, GLOBAL);
        let provider = label.provider(row.provider.as_deref());
        aggregate(
            &mut self.providers,
            row.provider.clone().unwrap_or_else(|| "null".into()),
            &vals,
            Value::Null,
        );
        aggregate(
            &mut self.models,
            row.model_key(),
            &vals,
            json!({
                "rawModel":row.model,"provider":provider,"lastUsed":row.timestamp,
            }),
        );
        if let Some(connection) = row.connection.as_deref().filter(|id| !id.is_empty()) {
            let name = label.account(connection);
            aggregate(
                &mut self.accounts,
                row.account_key(&name),
                &vals,
                json!({
                    "rawModel":row.model,"provider":provider,"connectionId":connection,
                    "accountName":name,"lastUsed":row.timestamp,
                }),
            );
        }
        let stored = row.key.as_deref().filter(|key| !key.is_empty());
        let masked = stored.and_then(mask);
        let name = label.key_name(stored, masked.as_deref());
        aggregate(
            &mut self.keys,
            row.group_key(),
            &vals,
            json!({
                "rawModel":row.model,"provider":provider,"apiKeyMasked":masked,
                "keyName":name,"apiKeyKey":masked.as_deref().unwrap_or(LOCAL),"lastUsed":row.timestamp,
            }),
        );
        aggregate(
            &mut self.endpoints,
            row.endpoint_key(),
            &vals,
            json!({
                "endpoint":row.endpoint(),"rawModel":row.model,"provider":provider,"lastUsed":row.timestamp,
            }),
        );
    }

    fn precision(&mut self, label: &Labels, row: &History) {
        let time = &row.timestamp;
        overlay(&mut self.models, &row.model_key(), time);
        if let Some(connection) = row.connection.as_deref().filter(|id| !id.is_empty()) {
            overlay(
                &mut self.accounts,
                &row.account_key(&label.account(connection)),
                time,
            );
        }
        overlay(&mut self.keys, &row.group_key(), time);
        overlay(&mut self.endpoints, &row.endpoint_key(), time);
    }

    fn response(self, last10: Vec<Value>, recent: Vec<Value>) -> Value {
        let requests = self.providers.values().fold(json!(0), |total, row| {
            js_add(&total, &one(&row["requests"]))
        });
        let ids: Map<String, Value> = self
            .keys
            .into_iter()
            .map(|(key, row)| {
                let id = if key == LOCAL
                    || (row["apiKeyMasked"].is_null() && key.starts_with("local-no-key|"))
                {
                    key
                } else {
                    format!("key-{:x}", Sha256::digest(key.as_bytes()))
                };
                (id, row)
            })
            .collect();
        json!({
            "totalRequests": requests,
            "totalPromptTokens":self.totals["promptTokens"],"totalCompletionTokens":self.totals["completionTokens"],
            "totalCachedTokens":self.totals["cachedTokens"],"totalCost":self.totals["cost"],
            "byProvider":self.providers,"byModel":self.models,"byAccount":self.accounts,
            "byApiKey":ids,"byEndpoint":self.endpoints,
            "last10Minutes":last10,
            "pending":{"byModel":{},"byAccount":{}},
            "activeRequests":[],"recentRequests":recent,"errorProvider":"",
        })
    }
}

fn entries(value: &Value) -> impl Iterator<Item = (&String, &Value)> {
    value.as_object().into_iter().flatten()
}

fn rolling_days(count: i64, now: DateTime<Utc>) -> NaiveDate {
    let today = now.with_timezone(&Local).date_naive();
    today
        .checked_sub_signed(Duration::days(count - 1))
        .unwrap_or(today)
}

fn midnight(now: DateTime<Utc>) -> DateTime<Utc> {
    let local = now.with_timezone(&Local).date_naive();
    let start = local.and_hms_opt(0, 0, 0);
    start
        .and_then(|start| {
            (0..120).find_map(|minute| {
                start
                    .checked_add_signed(Duration::minutes(minute))
                    .and_then(|time| Local.from_local_datetime(&time).earliest())
            })
        })
        .map(|time| time.with_timezone(&Utc))
        .unwrap_or(now)
}

fn day_span(period: &str) -> Option<i64> {
    match period {
        "7d" => Some(7),
        "30d" => Some(30),
        "60d" => Some(60),
        _ => None,
    }
}

fn days(db: &Connection, cutoff: Option<NaiveDate>) -> rusqlite::Result<Vec<(String, Value)>> {
    let sql = if cutoff.is_some() {
        "SELECT dateKey, data FROM usageDaily WHERE dateKey >= ? ORDER BY dateKey ASC"
    } else {
        "SELECT dateKey, data FROM usageDaily ORDER BY dateKey ASC"
    };
    let mut statement = db.prepare(sql)?;
    let values: Vec<String> = cutoff
        .map(|cutoff| cutoff.to_string())
        .into_iter()
        .collect();
    statement
        .query_map(params_from_iter(&values), |row| {
            let raw: String = row.get(1)?;
            Ok((row.get(0)?, parse_json(Some(&raw))))
        })?
        .collect()
}

fn last_minutes(db: &Connection, now: DateTime<Utc>) -> rusqlite::Result<Vec<Value>> {
    let frame = now.timestamp_millis().div_euclid(60_000) - 9;
    let Some(start) = DateTime::from_timestamp_millis(frame * 60_000) else {
        return Err(rusqlite::Error::InvalidQuery);
    };
    let mut buckets = vec![empty_counter(MINUTE); 10];
    let mut statement = db.prepare(
        "SELECT timestamp, promptTokens, completionTokens, cost
         FROM usageHistory WHERE timestamp >= ?1 AND timestamp <= ?2",
    )?;
    let query = statement.query_map(params![iso(start), iso(now)], |row| {
        let time: String = row.get(0)?;
        Ok((
            time,
            json!({
                "requests":1,
                "promptTokens":scalar(row.get_ref(1)?),
                "completionTokens":scalar(row.get_ref(2)?),
                "cost":scalar(row.get_ref(3)?),
            }),
        ))
    })?;
    for row in query {
        let (time, vals) = row?;
        if let Some(time) = parse_timestamp(&time) {
            let minute = time.timestamp_millis().div_euclid(60_000);
            if let Ok(index) = usize::try_from(minute - frame)
                && let Some(bucket) = buckets.get_mut(index)
            {
                bump(bucket, &vals, MINUTE);
            }
        }
    }
    Ok(buckets)
}

pub(super) fn statistics(
    db: &Connection,
    period: &str,
    now: DateTime<Utc>,
) -> rusqlite::Result<Value> {
    let label = Labels::from_db(db);
    let mut stats = Stats::new();
    if matches!(period, "today" | "24h") {
        let cutoff = if period == "today" {
            midnight(now)
        } else {
            now - Duration::hours(24)
        };
        for item in records(db, "WHERE timestamp >= ?1", &[iso(cutoff)])? {
            stats.history(&label, &item);
        }
    } else {
        let count = day_span(period);
        for (day, value) in days(db, count.map(|count| rolling_days(count, now)))? {
            if value.is_null() {
                return Err(rusqlite::Error::InvalidQuery);
            }
            stats.daily(&label, &day, &value);
        }
        let cutoff = (now - Duration::days(count.unwrap_or(2).min(2))).max(DateTime::UNIX_EPOCH);
        for item in records(db, "WHERE timestamp >= ?1", &[iso(cutoff)])? {
            stats.precision(&label, &item);
        }
    }
    Ok(stats.response(last_minutes(db, now)?, recent(db)?))
}

pub(super) fn chart(db: &Connection, period: &str, now: DateTime<Utc>) -> rusqlite::Result<Value> {
    if matches!(period, "today" | "24h") {
        let today = period == "today";
        let start = if today {
            midnight(now)
        } else {
            now - Duration::hours(24)
        };
        let end = if today {
            start + Duration::hours(24)
        } else {
            now
        };
        let mut buckets: Vec<_> = (0..24)
            .map(|hour| {
                let label = (start + Duration::hours(hour))
                    .with_timezone(&Local)
                    .format("%H:%M")
                    .to_string();
                json!({"label":label,"tokens":0,"cost":0,"requests":0})
            })
            .collect();
        let mut statement = db.prepare(
            "SELECT timestamp, promptTokens, completionTokens, cost FROM usageHistory WHERE timestamp >= ?1",
        )?;
        let rows = statement.query_map([iso(start)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                scalar(row.get_ref(1)?),
                scalar(row.get_ref(2)?),
                scalar(row.get_ref(3)?),
            ))
        })?;
        for row in rows {
            let (time, prompt, completion, cost) = row?;
            let Some(time) = parse_timestamp(&time) else {
                continue;
            };
            if time < start || (if today { time >= end } else { time > end }) {
                continue;
            }
            let index = time.signed_duration_since(start).num_hours().min(23) as usize;
            bump(
                &mut buckets[index],
                &json!({"tokens":js_add(&one(&prompt), &one(&completion)),"cost":cost,"requests":1}),
                &["tokens", "cost", "requests"],
            );
        }
        return Ok(json!(buckets));
    }

    let count = day_span(period);
    let requested = count.map(|count| rolling_days(count, now));
    let rows = days(db, requested)?;
    let today = now.with_timezone(&Local).date_naive();
    let start = if let Some(requested) = requested {
        requested
    } else if let Some((first, _)) = rows.first() {
        NaiveDate::parse_from_str(first, "%Y-%m-%d").map_err(|_| rusqlite::Error::InvalidQuery)?
    } else {
        return Ok(json!([]));
    };
    let buckets = count.unwrap_or_else(|| {
        today
            .signed_duration_since(start)
            .num_days()
            .saturating_add(1)
            .max(1)
    });
    let daily: HashMap<_, _> = rows.into_iter().collect();
    let data: Vec<_> = (0..buckets)
        .filter_map(|offset| {
            let current = start.checked_add_signed(Duration::days(offset))?;
            let key = current.to_string();
            let day = daily.get(&key).unwrap_or(&Value::Null);
            Some(json!({
                "label":current.format("%b %-d").to_string(),
                "tokens":js_add(&one(&day["promptTokens"]), &one(&day["completionTokens"])),
                "cost":one(&day["cost"]),
                "requests":one(&day["requests"]),
            }))
        })
        .collect();
    Ok(json!(data))
}
