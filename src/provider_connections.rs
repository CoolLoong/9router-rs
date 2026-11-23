use rusqlite::{Connection, OptionalExtension, Row, params, types::Value as SqlValue};
use serde_json::{Map, Value, json};

use crate::storage::is_active;

const COLS: &str =
    "id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt";
const CORE: &[&str] = &[
    "id",
    "provider",
    "authType",
    "name",
    "email",
    "priority",
    "isActive",
    "createdAt",
    "updatedAt",
];

pub fn read_all(db: &Connection, provider: Option<&str>) -> rusqlite::Result<Vec<Value>> {
    let mut connections: Vec<_> = if let Some(provider) = provider {
        let mut stmt = db.prepare(&format!(
            "SELECT {COLS} FROM providerConnections WHERE provider = ?1"
        ))?;
        stmt.query_map([provider], read_row)?
            .collect::<rusqlite::Result<_>>()?
    } else {
        let mut stmt = db.prepare(&format!("SELECT {COLS} FROM providerConnections"))?;
        stmt.query_map([], read_row)?
            .collect::<rusqlite::Result<_>>()?
    };
    connections.sort_by(|a, b| compare_priority(a, b, 999.0));
    Ok(connections)
}

pub fn by_id(db: &Connection, id: &str) -> rusqlite::Result<Option<Value>> {
    db.query_row(
        &format!("SELECT {COLS} FROM providerConnections WHERE id = ?1"),
        [id],
        read_row,
    )
    .optional()
}

pub fn api_key_by_name(
    db: &Connection,
    provider: &str,
    name: &Value,
) -> rusqlite::Result<Option<Value>> {
    db.query_row(
        &format!("SELECT {COLS} FROM providerConnections WHERE provider = ?1 AND authType = 'apikey' AND name = ?2"),
        params![provider, scalar(name)?], read_row,
    ).optional()
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<Value> {
    let data: String = row.get(7)?;
    let mut result = object(serde_json::from_str(&data).unwrap_or_default());
    for (index, field) in CORE.iter().enumerate() {
        let value = if *field == "isActive" {
            json!(is_active(row.get_ref(6)?))
        } else {
            let column = if index >= 7 { index + 1 } else { index };
            sql_json(row.get_ref(column)?)
        };
        result.insert((*field).to_owned(), value);
    }
    Ok(Value::Object(result))
}

pub fn write(db: &Connection, conn: &Value) -> rusqlite::Result<()> {
    let mut data = object(conn.clone());
    for key in CORE {
        data.shift_remove(*key);
    }
    db.execute(
        "INSERT INTO providerConnections(id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET provider=excluded.provider, authType=excluded.authType,
           name=excluded.name, email=excluded.email, priority=excluded.priority, isActive=excluded.isActive,
           data=excluded.data, updatedAt=excluded.updatedAt",
        params![
            conn["id"].as_str(), conn["provider"].as_str(), conn["authType"].as_str(),
            scalar(&conn["name"])?, scalar(&conn["email"])?, scalar(&conn["priority"])?,
            conn["isActive"] != false, Value::Object(data).to_string(),
            conn["createdAt"].as_str(), conn["updatedAt"].as_str(),
        ],
    )?;
    Ok(())
}

pub fn reorder(db: &Connection, provider: &str) -> rusqlite::Result<()> {
    let mut list = read_all(db, Some(provider))?;
    list.sort_by(|a, b| {
        compare_priority(a, b, 0.0)
            .then_with(|| timestamp(&b["updatedAt"]).cmp(&timestamp(&a["updatedAt"])))
    });
    for (index, conn) in list.iter().enumerate() {
        let priority = (index + 1) as i64;
        if conn["priority"] != priority {
            db.execute(
                "UPDATE providerConnections SET priority = ?1 WHERE id = ?2",
                params![priority, conn["id"].as_str()],
            )?;
        }
    }
    Ok(())
}

pub fn patch(mut existing: Value, mut patch: Map<String, Value>, now: &str) -> Value {
    if patch
        .get("testStatus")
        .is_some_and(|status| status == "active")
    {
        for name in ["lastError", "lastErrorAt"] {
            patch.entry(name).or_insert(Value::Null);
        }
        patch.insert("errorCode".into(), Value::Null);
        patch.insert("rateLimitedUntil".into(), Value::Null);
        patch.insert("backoffLevel".into(), json!(0));
        if let Some(fields) = existing.as_object() {
            for key in fields.keys().filter(|key| key.starts_with("modelLock_")) {
                patch.insert(key.clone(), Value::Null);
            }
        }
    }
    if let Some(fields) = existing.as_object_mut() {
        fields.extend(patch);
        fields.insert("updatedAt".into(), json!(now));
    }
    existing
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(fields) => fields,
        Value::Array(items) => items
            .into_iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v))
            .collect(),
        _ => Map::new(),
    }
}

pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn sql_json(value: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null | ValueRef::Blob(_) => Value::Null,
        ValueRef::Integer(n) => json!(n),
        ValueRef::Real(n) => json!(n),
        ValueRef::Text(s) => json!(String::from_utf8_lossy(s)),
    }
}

fn scalar(value: &Value) -> rusqlite::Result<SqlValue> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::String(s) => Ok(SqlValue::Text(s.clone())),
        Value::Number(n) => Ok(n
            .as_i64()
            .map(SqlValue::Integer)
            .unwrap_or_else(|| SqlValue::Real(n.as_f64().unwrap_or_default()))),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn compare_priority(a: &Value, b: &Value, default: f64) -> std::cmp::Ordering {
    let number = |v: &Value| {
        if !truthy(v) {
            return default;
        }
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(f64::NAN)
    };
    number(&a["priority"])
        .partial_cmp(&number(&b["priority"]))
        .unwrap_or(std::cmp::Ordering::Equal)
}

fn timestamp(value: &Value) -> i64 {
    value
        .as_str()
        .and_then(|date| chrono::DateTime::parse_from_rfc3339(date).ok())
        .map_or(0, |date| date.timestamp_millis())
}
