use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Local, SecondsFormat, Utc};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, params_from_iter};
use serde_json::{Value, json};

use crate::provider_connections::truthy;

const LEGACY_FILES: &[&str] = &[
    "db.json",
    "usage.json",
    "disabledModels.json",
    "request-details.json",
];
const KEEP_BACKUPS: usize = 3;

pub fn initialize(root: &Path, file: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let db_dir = root.join("db");
    let backups = db_dir.join("backups");
    fs::create_dir_all(&backups)?;
    let mut connection = Connection::open(file)?;
    let schema: Value = serde_json::from_str(include_str!("database-schema.json"))?;
    let pragmas = schema["pragmas"].as_str().ok_or("missing SQLite pragmas")?;
    connection.execute_batch(pragmas)?;
    let tables = schema["tables"]
        .as_object()
        .ok_or("missing SQLite tables")?;
    let backup_version = schema["backupSchemaVersion"]
        .as_i64()
        .ok_or("missing backup schema version")?;
    let app_version = app_version()?;

    let has_meta: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='_meta')",
        [],
        |row| row.get(0),
    )?;
    let old_backup = if has_meta {
        meta_version(&connection, "backupSchemaVersion")?
    } else {
        0
    };
    let was_fresh = !has_meta || meta_empty(&connection)?;
    if !was_fresh && old_backup < backup_version {
        let snapshot = backup_dir(
            &backups,
            &format!("schema-{old_backup}-to-{backup_version}"),
            &app_version,
        );
        if let Err(error) =
            snapshot.and_then(|path| snapshot_schema(&mut connection, &path.join("data.sqlite")))
        {
            eprintln!("Could not save a pre-migration database backup: {error}");
        }
    }

    let mut tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_table(&tx, "_meta", &tables["_meta"])?;
    let fresh = meta_empty(&tx)?;
    for (name, table) in tables {
        ensure_table(&tx, name, table)?;
    }
    let version = schema["schemaVersion"]
        .as_i64()
        .ok_or("missing database schema version")?;
    if meta_version(&tx, "schemaVersion")? < version {
        set_meta(&tx, "schemaVersion", version.to_string())?;
    }
    if meta_version(&tx, "backupSchemaVersion")? <= backup_version {
        set_meta(&tx, "backupSchemaVersion", backup_version.to_string())?;
    }

    let marker = db_dir.join(".migrated-from-json");
    let mut migrated_at = None;
    let mut aborted = false;
    if fresh && !marker.exists() {
        let legacy = LEGACY_FILES
            .iter()
            .map(|name| {
                fs::read(root.join(name))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            })
            .collect::<Vec<_>>();
        if legacy
            .iter()
            .any(|value| value.as_ref().is_some_and(truthy))
        {
            let snapshot = backup_dir(&backups, "migrate-from-json", &app_version)?;
            for name in LEGACY_FILES {
                let source = root.join(name);
                if source.is_file() {
                    fs::copy(source, snapshot.join(name))?;
                }
            }
            let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
            let mut migration = tx.savepoint()?;
            match import_main(&migration, legacy[0].as_ref().unwrap_or(&Value::Null), &now) {
                Err(rusqlite::Error::ToSqlConversionFailure(error))
                    if error.is::<SizeMismatch>() =>
                {
                    migration.rollback()?;
                    aborted = true;
                }
                result => {
                    result?;
                    import_usage(&migration, legacy[1].as_ref().unwrap_or(&Value::Null), &now)?;
                    import_disabled(&migration, legacy[2].as_ref().unwrap_or(&Value::Null))?;
                    import_details(&migration, legacy[3].as_ref().unwrap_or(&Value::Null), &now)?;
                    set_meta(&migration, "migratedAt", &now)?;
                    migration.commit()?;
                    migrated_at = Some(now);
                }
            }
        }
    }
    if !aborted {
        set_meta(&tx, "appVersion", app_version)?;
    }
    tx.commit()?;
    if let Some(timestamp) = migrated_at {
        let _ = fs::write(marker, timestamp);
    }
    prune_backups(&backups);
    Ok(())
}

fn app_version() -> Result<String, serde_json::Error> {
    let manifest: Value = serde_json::from_str(include_str!("../../../package.json"))?;
    Ok(manifest["version"].as_str().unwrap_or("0.0.0").to_owned())
}

fn meta_empty(db: &Connection) -> rusqlite::Result<bool> {
    db.query_row("SELECT NOT EXISTS(SELECT 1 FROM _meta)", [], |row| {
        row.get(0)
    })
}

fn meta_version(db: &Connection, key: &str) -> rusqlite::Result<i64> {
    let value = db
        .query_row("SELECT value FROM _meta WHERE key=?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()?;
    Ok(value
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0))
}

fn set_meta(db: &Connection, key: &str, value: impl AsRef<str>) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO _meta (key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        (key, value.as_ref()),
    )?;
    Ok(())
}

fn ensure_table(
    db: &Connection,
    name: &str,
    table: &Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let ddl = sqlite_schema_rs::port::build_create_table_sql(json!(name), table.clone())?;
    db.execute(ddl.as_str().ok_or("invalid SQLite table declaration")?, [])?;
    let mut statement = db.prepare(&format!("PRAGMA table_info({})", identifier(name)))?;
    let found = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    drop(statement);
    let columns = table["columns"]
        .as_object()
        .ok_or("missing SQLite columns")?;
    for (column, definition) in columns {
        if !found.contains(column) {
            let definition = definition
                .as_str()
                .ok_or("invalid SQLite column definition")?;
            let definition = definition
                .replace("PRIMARY KEY AUTOINCREMENT", "")
                .replace("PRIMARY KEY", "")
                .replace("UNIQUE", "");
            db.execute(
                &format!(
                    "ALTER TABLE {} ADD COLUMN {} {}",
                    identifier(name),
                    identifier(column),
                    definition.trim(),
                ),
                [],
            )?;
        }
    }
    for index in table["indexes"].as_array().into_iter().flatten() {
        db.execute(index.as_str().ok_or("invalid SQLite index")?, [])?;
    }
    Ok(())
}

fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn backup_dir(parent: &Path, label: &str, version: &str) -> std::io::Result<PathBuf> {
    let now = Local::now().format("%Y%m%d-%H%M%S");
    let path = parent.join(format!("{label}-{version}-{now}"));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn snapshot_schema(db: &mut Connection, path: &Path) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing backup directory"))?;
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    let run = |db: &mut Connection| -> rusqlite::Result<()> {
        db.execute(
            "ATTACH DATABASE ?1 AS bak",
            [temporary.path().to_string_lossy().as_ref()],
        )?;
        let result = copy_tables(db);
        let detach = db.execute("DETACH DATABASE bak", []);
        result.and(detach.map(|_| ()))
    };
    run(db).map_err(std::io::Error::other)?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn copy_tables(db: &mut Connection) -> rusqlite::Result<()> {
    let tx = db.transaction()?;
    let mut statement = tx.prepare(
        "SELECT name,sql FROM main.sqlite_schema WHERE type='table'
         AND name NOT LIKE 'sqlite_%' AND name <> 'requestDetails' AND sql IS NOT NULL",
    )?;
    let tables = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for (name, sql) in tables {
        let declaration = sql
            .get("CREATE TABLE".len()..)
            .ok_or(rusqlite::Error::InvalidQuery)?;
        tx.execute(
            &format!("CREATE TABLE bak.{}", declaration.trim_start()),
            [],
        )?;
        let name = identifier(&name);
        tx.execute(
            &format!("INSERT INTO bak.{name} SELECT * FROM main.{name}"),
            [],
        )?;
    }
    tx.commit()
}

fn prune_backups(path: &Path) {
    let mut entries = fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata
                .is_dir()
                .then_some((metadata.modified().ok()?, entry.path()))
        })
        .collect::<Vec<_>>();
    entries.sort_unstable_by(|left, right| right.cmp(left));
    for (_, path) in entries.into_iter().skip(KEEP_BACKUPS) {
        let _ = fs::remove_dir_all(path);
    }
}

fn rows<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value[key].as_array().map(Vec::as_slice).unwrap_or_default()
}

fn fallback(value: &Value, default: Value) -> Value {
    if truthy(value) {
        value.clone()
    } else {
        default
    }
}

fn active(value: &Value) -> Value {
    json!(i64::from(*value != false))
}

fn remainder(row: &Value, columns: &[&str]) -> Value {
    let mut remainder = row.as_object().cloned().unwrap_or_default();
    remainder.retain(|key, _| !columns.contains(&key.as_str()));
    Value::Object(remainder)
}

fn bind_row(db: &Connection, sql: &str, values: &[Value]) -> rusqlite::Result<()> {
    let bindings = values
        .iter()
        .map(|value| match value {
            Value::Null => Ok(SqlValue::Null),
            Value::Bool(value) => Ok(SqlValue::Integer(i64::from(*value))),
            Value::Number(value) => value
                .as_i64()
                .map(SqlValue::Integer)
                .or_else(|| value.as_f64().map(SqlValue::Real))
                .ok_or(rusqlite::Error::InvalidQuery),
            Value::String(value) => Ok(SqlValue::Text(value.clone())),
            _ => Err(rusqlite::Error::InvalidQuery),
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    db.execute(sql, params_from_iter(bindings))?;
    Ok(())
}

#[derive(Debug)]
struct SizeMismatch {
    table: String,
    actual: usize,
    expected: usize,
}

impl std::fmt::Display for SizeMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} migration has {} rows, expected {}",
            self.table, self.actual, self.expected
        )
    }
}

impl std::error::Error for SizeMismatch {}

fn assert_size(db: &Connection, table: &str, expected: usize) -> rusqlite::Result<()> {
    let inserted: usize = db.query_row(
        &format!("SELECT COUNT(*) FROM {}", identifier(table)),
        [],
        |row| row.get(0),
    )?;
    if inserted == expected {
        Ok(())
    } else {
        Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
            SizeMismatch {
                table: table.into(),
                actual: inserted,
                expected,
            },
        )))
    }
}

fn set_kv(db: &Connection, scope: &str, key: &str, value: &Value) -> rusqlite::Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO kv (scope,key,value) VALUES(?1,?2,?3)",
        params![scope, key, value.to_string()],
    )?;
    Ok(())
}

fn import_main(db: &Connection, data: &Value, now: &str) -> rusqlite::Result<()> {
    if !data.is_object() && !data.is_array() {
        return Ok(());
    }
    if truthy(&data["settings"]) {
        db.execute(
            "INSERT INTO settings (id,data) VALUES(1,?1)
                    ON CONFLICT(id) DO UPDATE SET data=excluded.data",
            [data["settings"].to_string()],
        )?;
    }

    for row in rows(data, "providerConnections") {
        let fields = [
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
        bind_row(
            db,
            "INSERT OR REPLACE INTO providerConnections
            (id,provider,authType,name,email,priority,isActive,data,createdAt,updatedAt)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            &[
                row["id"].clone(),
                row["provider"].clone(),
                fallback(&row["authType"], json!("oauth")),
                fallback(&row["name"], Value::Null),
                fallback(&row["email"], Value::Null),
                fallback(&row["priority"], Value::Null),
                active(&row["isActive"]),
                json!(remainder(row, &fields).to_string()),
                fallback(&row["createdAt"], json!(now)),
                fallback(&row["updatedAt"], json!(now)),
            ],
        )?;
    }
    assert_size(
        db,
        "providerConnections",
        rows(data, "providerConnections").len(),
    )?;

    for row in rows(data, "providerNodes") {
        bind_row(
            db,
            "INSERT OR REPLACE INTO providerNodes
            (id,type,name,data,createdAt,updatedAt) VALUES (?1,?2,?3,?4,?5,?6)",
            &[
                row["id"].clone(),
                fallback(&row["type"], Value::Null),
                fallback(&row["name"], Value::Null),
                json!(
                    remainder(row, &["id", "type", "name", "createdAt", "updatedAt"]).to_string()
                ),
                fallback(&row["createdAt"], json!(now)),
                fallback(&row["updatedAt"], json!(now)),
            ],
        )?;
    }
    assert_size(db, "providerNodes", rows(data, "providerNodes").len())?;

    for row in rows(data, "proxyPools") {
        bind_row(
            db,
            "INSERT OR REPLACE INTO proxyPools
            (id,isActive,testStatus,data,createdAt,updatedAt) VALUES (?1,?2,?3,?4,?5,?6)",
            &[
                row["id"].clone(),
                active(&row["isActive"]),
                fallback(&row["testStatus"], json!("unknown")),
                json!(
                    remainder(
                        row,
                        &["id", "isActive", "testStatus", "createdAt", "updatedAt"]
                    )
                    .to_string()
                ),
                fallback(&row["createdAt"], json!(now)),
                fallback(&row["updatedAt"], json!(now)),
            ],
        )?;
    }
    assert_size(db, "proxyPools", rows(data, "proxyPools").len())?;

    for row in rows(data, "apiKeys") {
        bind_row(
            db,
            "INSERT OR REPLACE INTO apiKeys
            (id,key,name,machineId,isActive,createdAt) VALUES (?1,?2,?3,?4,?5,?6)",
            &[
                row["id"].clone(),
                row["key"].clone(),
                fallback(&row["name"], Value::Null),
                fallback(&row["machineId"], Value::Null),
                active(&row["isActive"]),
                fallback(&row["createdAt"], json!(now)),
            ],
        )?;
    }
    assert_size(db, "apiKeys", rows(data, "apiKeys").len())?;

    for row in rows(data, "combos") {
        bind_row(
            db,
            "INSERT OR REPLACE INTO combos
            (id,name,kind,models,createdAt,updatedAt) VALUES (?1,?2,?3,?4,?5,?6)",
            &[
                row["id"].clone(),
                row["name"].clone(),
                fallback(&row["kind"], Value::Null),
                json!(fallback(&row["models"], json!([])).to_string()),
                fallback(&row["createdAt"], json!(now)),
                fallback(&row["updatedAt"], json!(now)),
            ],
        )?;
    }
    assert_size(db, "combos", rows(data, "combos").len())?;

    for (scope, default) in [
        ("modelAliases", None),
        ("mitmAlias", Some(json!({}))),
        ("pricing", Some(json!({}))),
    ] {
        if let Some(entries) = data[scope].as_object() {
            for (key, value) in entries {
                let value = if let Some(default) = &default {
                    fallback(value, default.clone())
                } else {
                    value.clone()
                };
                set_kv(db, scope, key, &value)?;
            }
        }
    }
    for model in rows(data, "customModels") {
        let provider = model
            .get("providerAlias")
            .map(crate::chat_output::js_text)
            .unwrap_or_else(|| "undefined".into());
        let id = model
            .get("id")
            .map(crate::chat_output::js_text)
            .unwrap_or_else(|| "undefined".into());
        let kind = crate::chat_output::js_text(&fallback(&model["type"], json!("llm")));
        set_kv(
            db,
            "customModels",
            &format!("{provider}|{id}|{kind}"),
            model,
        )?;
    }
    Ok(())
}

fn import_usage(db: &Connection, data: &Value, now: &str) -> rusqlite::Result<()> {
    for entry in rows(data, "history") {
        let tokens = fallback(&entry["tokens"], json!({}));
        let values = [
            fallback(&entry["timestamp"], json!(now)),
            fallback(&entry["provider"], Value::Null),
            fallback(&entry["model"], Value::Null),
            fallback(&entry["connectionId"], Value::Null),
            fallback(&entry["apiKey"], Value::Null),
            fallback(&entry["endpoint"], Value::Null),
            fallback(
                &tokens["prompt_tokens"],
                fallback(&tokens["input_tokens"], json!(0)),
            ),
            fallback(
                &tokens["completion_tokens"],
                fallback(&tokens["output_tokens"], json!(0)),
            ),
            fallback(&entry["cost"], json!(0)),
            fallback(&entry["status"], json!("ok")),
            json!(tokens.to_string()),
            json!("{}"),
        ];
        bind_row(db, "INSERT INTO usageHistory
             (timestamp,provider,model,connectionId,apiKey,endpoint,promptTokens,completionTokens,cost,status,tokens,meta)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)", &values)?;
    }
    if let Some(days) = data["dailySummary"].as_object() {
        for (date, day) in days {
            db.execute(
                "INSERT OR REPLACE INTO usageDaily (dateKey,data) VALUES (?1,?2)",
                (date, day.to_string()),
            )?;
        }
    }
    if data["totalRequestsLifetime"].is_number() {
        set_meta(
            db,
            "totalRequestsLifetime",
            data["totalRequestsLifetime"].to_string(),
        )?;
    }
    Ok(())
}

fn import_disabled(db: &Connection, data: &Value) -> rusqlite::Result<()> {
    if let Some(disabled) = data["disabled"].as_object() {
        for (provider, models) in disabled {
            set_kv(db, "disabledModels", provider, &fallback(models, json!([])))?;
        }
    }
    Ok(())
}

fn import_details(db: &Connection, data: &Value, now: &str) -> rusqlite::Result<()> {
    for row in rows(data, "records") {
        bind_row(
            db,
            "INSERT OR REPLACE INTO requestDetails
            (id,timestamp,provider,model,connectionId,status,data) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            &[
                row["id"].clone(),
                fallback(&row["timestamp"], json!(now)),
                fallback(&row["provider"], Value::Null),
                fallback(&row["model"], Value::Null),
                fallback(&row["connectionId"], Value::Null),
                fallback(&row["status"], Value::Null),
                json!(row.to_string()),
            ],
        )?;
    }
    Ok(())
}
