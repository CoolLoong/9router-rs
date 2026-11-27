use gateway_model_resolver_rs::port::{known_provider_id, resolve_model_alias_target};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

pub const ANTHROPIC_GATEWAY_PREFIX: &str = "anthropic-gateway/";

pub fn strip_gateway_prefix(model: &str) -> &str {
    model
        .strip_prefix(ANTHROPIC_GATEWAY_PREFIX)
        .or_else(|| {
            model
                .get(..ANTHROPIC_GATEWAY_PREFIX.len())
                .filter(|head| head.eq_ignore_ascii_case(ANTHROPIC_GATEWAY_PREFIX))
                .and_then(|_| model.get(ANTHROPIC_GATEWAY_PREFIX.len()..))
        })
        .filter(|rest| !rest.is_empty())
        .unwrap_or(model)
}

pub fn get_model_info(db: &Connection, model: &str) -> rusqlite::Result<Option<(String, String)>> {
    let model = strip_gateway_prefix(model);
    if model.is_empty() {
        return Ok(None);
    }
    if let Some((prefix, model)) = model.split_once('/') {
        let provider = match prefix {
            "xmtp" | "xiaomi-tokenplan" => "xiaomi-tokenplan".to_owned(),
            _ => match known_provider_id(prefix).map_err(conversion_error)? {
                Some(provider) => provider.to_owned(),
                None => node_for_prefix(db, prefix)?.unwrap_or_else(|| prefix.to_owned()),
            },
        };
        return Ok(Some((provider, model.to_owned())));
    }
    if combo_exists(db, model)? {
        return Ok(None);
    }

    let alias: Option<String> = db
        .query_row(
            "SELECT value FROM kv WHERE scope = 'modelAliases' AND key = ?1",
            [model],
            |row| row.get(0),
        )
        .optional()?;
    let alias = alias.and_then(|value| serde_json::from_str(&value).ok());
    let target = match alias {
        Some(value) => resolve_model_alias_target(&value).map_err(conversion_error)?,
        None => Value::Null,
    };
    let (provider, model) = if target.is_null() {
        if model == "grok-build" {
            ("grok-cli", model)
        } else {
            (infer_provider(model), model)
        }
    } else {
        let (Some(provider), Some(model)) = (target["provider"].as_str(), target["model"].as_str())
        else {
            return Err(conversion_error("Invalid model alias target"));
        };
        (provider, model)
    };

    let provider = if provider == "anthropic"
        && !has_active_connection(db, "anthropic")?
        && has_active_connection(db, "claude")?
    {
        "claude"
    } else {
        provider
    };
    Ok(Some((provider.to_owned(), model.to_owned())))
}

pub fn combo_exists(db: &Connection, name: &str) -> rusqlite::Result<bool> {
    let name = strip_gateway_prefix(name);
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM combos WHERE name = ?1)",
        [name],
        |row| row.get(0),
    )
}

pub fn get_combo_models(db: &Connection, model: &str) -> rusqlite::Result<Option<Value>> {
    let model = strip_gateway_prefix(model);
    if model.contains('/') {
        return Ok(None);
    }
    let models: Option<String> = db
        .query_row(
            "SELECT models FROM combos WHERE name = ?1",
            [model],
            |row| row.get(0),
        )
        .optional()?;
    Ok(models
        .and_then(|models| serde_json::from_str(&models).ok())
        .filter(has_positive_length))
}

fn has_positive_length(value: &Value) -> bool {
    match value {
        Value::Array(items) => !items.is_empty(),
        Value::String(text) => !text.is_empty(),
        Value::Object(fields) => fields.get("length").is_some_and(|length| match length {
            Value::Number(number) => number.as_f64().is_some_and(|number| number > 0.0),
            Value::String(text) => text.trim().parse::<f64>().is_ok_and(|number| number > 0.0),
            Value::Bool(flag) => *flag,
            _ => false,
        }),
        _ => false,
    }
}

fn node_for_prefix(db: &Connection, prefix: &str) -> rusqlite::Result<Option<String>> {
    let mut statement = db.prepare("SELECT id, data FROM providerNodes WHERE type = ?1")?;
    for kind in [
        "openai-compatible",
        "anthropic-compatible",
        "custom-embedding",
    ] {
        let mut rows = statement.query([kind])?;
        while let Some(row) = rows.next()? {
            let data: String = row.get(1)?;
            if serde_json::from_str::<Value>(&data)
                .ok()
                .is_some_and(|data| data["prefix"].as_str() == Some(prefix))
            {
                return row.get(0).map(Some);
            }
        }
    }
    Ok(None)
}

pub(super) fn has_active_connection(db: &Connection, provider: &str) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM providerConnections WHERE provider = ?1 AND isActive = 1)",
        [provider],
        |row| row.get(0),
    )
}

fn infer_provider(model: &str) -> &'static str {
    let model = model.to_lowercase();
    if model == "codex-auto-review"
        || ["gpt-5.", "gpt-6.", "gpt-6-", "gpt-daybreak-", "gpt-reserve"]
            .iter()
            .any(|prefix| model.starts_with(prefix))
    {
        "codex"
    } else if model.starts_with("claude-") {
        "anthropic"
    } else if model
        .split_once('.')
        .and_then(|(region, tail)| tail.split_once('.').map(|(vendor, _)| (region, vendor)))
        .is_some_and(|(region, vendor)| {
            matches!(region, "us" | "eu" | "ap" | "global")
                && matches!(vendor, "anthropic" | "meta" | "amazon" | "mistral" | "xai")
        })
    {
        "bedrock"
    } else if model.starts_with("gemini-") {
        "gemini"
    } else if model.starts_with("deepseek-") {
        "openrouter"
    } else {
        "openai"
    }
}

fn conversion_error(error: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(error.into())))
}
