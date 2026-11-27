use std::collections::HashSet;

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::{model_resolution, provider_index};

pub struct AllowSets {
    combos: HashSet<String>,
    models: HashSet<(String, String)>,
    providers: HashSet<String>,
}

fn load_allow_sets(db: &rusqlite::Connection, key: &str) -> rusqlite::Result<Option<AllowSets>> {
    use rusqlite::OptionalExtension;
    let row = db
        .query_row(
            "SELECT accessRestricted, accessAllow FROM apiKeys WHERE key = ?1",
            [key],
            |row| {
                Ok((
                    crate::storage::is_active(row.get_ref(0)?),
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()?;
    let Some((true, raw)) = row else {
        return Ok(None);
    };
    let settings =
        key_access_rs::port::key_access_from_columns(json!(true), json!(raw)).map_err(|error| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(error)))
        })?;
    let mut all_combos = db.prepare("SELECT name FROM combos")?;
    let all_combos = all_combos
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|name| name.map(|name| name.to_lowercase()))
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    let mut allowed_combos = HashSet::new();
    let mut allowed_models = HashSet::new();
    let mut allowed_providers = HashSet::new();
    for name in settings["allow"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let lowered = name.to_lowercase();
        if all_combos.contains(&lowered) {
            allowed_combos.insert(lowered);
            continue;
        }
        if let Some((provider, model)) = model_resolution::get_model_info(db, name)?
            && !provider.is_empty()
            && !model.is_empty()
        {
            allowed_models.insert((provider.to_lowercase(), model.to_lowercase()));
        }
        if !name.contains('/') {
            let resolved = provider_index::entries()
                .iter()
                .find(|entry| entry["alias"] == name || entry["id"] == name)
                .and_then(|entry| entry["id"].as_str())
                .unwrap_or(name);
            allowed_providers.insert(resolved.to_lowercase());
        }
    }
    Ok(Some(AllowSets {
        combos: allowed_combos,
        models: allowed_models,
        providers: allowed_providers,
    }))
}

pub fn filter_for_key(
    db: &Connection,
    key: &str,
    models: Vec<Value>,
) -> rusqlite::Result<Vec<Value>> {
    let Some(allow) = load_allow_sets(db, key)? else {
        return Ok(models);
    };
    let mut result = Vec::new();
    for model in models {
        let Some(id) = model["id"].as_str() else {
            continue;
        };
        if model["owned_by"] == "combo" {
            if allow.combos.contains(&id.to_lowercase()) {
                result.push(model);
            }
            continue;
        }
        let Some((provider, resolved)) = model_resolution::get_model_info(db, id)? else {
            continue;
        };
        if provider.is_empty() {
            continue;
        }
        let provider = provider.to_lowercase();
        let allowed = if matches!(model["kind"].as_str(), Some("webSearch" | "webFetch")) {
            allow.providers.contains(&provider)
        } else {
            allow.models.contains(&(provider, resolved.to_lowercase()))
        };
        if allowed {
            result.push(model);
        }
    }
    Ok(result)
}

pub fn permits_chat(db: &Connection, key: &str, requested: &str) -> rusqlite::Result<bool> {
    let requested = model_resolution::strip_gateway_prefix(requested);
    let Some(allow) = load_allow_sets(db, key)? else {
        return Ok(true);
    };
    if requested.is_empty() {
        return Ok(false);
    }
    if model_resolution::get_combo_models(db, requested)?.is_some() {
        return Ok(allow.combos.contains(&requested.to_lowercase()));
    }
    match model_resolution::get_model_info(db, requested)? {
        Some((provider, model)) if !provider.is_empty() => Ok(allow
            .models
            .contains(&(provider.to_lowercase(), model.to_lowercase()))),
        _ => Ok(allow.combos.contains(&requested.to_lowercase())),
    }
}

pub fn permits_resolved(
    db: &Connection,
    key: &str,
    provider: &str,
    model: &str,
) -> rusqlite::Result<bool> {
    Ok(match load_allow_sets(db, key)? {
        None => true,
        Some(allow) => allow
            .models
            .contains(&(provider.to_lowercase(), model.to_lowercase())),
    })
}

pub fn permits_provider(
    db: &Connection,
    key: &str,
    requested: &str,
    provider: &str,
    is_combo: bool,
) -> rusqlite::Result<bool> {
    Ok(match load_allow_sets(db, key)? {
        None => true,
        Some(allow) if is_combo => allow.combos.contains(&requested.to_lowercase()),
        Some(allow) => !provider.is_empty() && allow.providers.contains(&provider.to_lowercase()),
    })
}
