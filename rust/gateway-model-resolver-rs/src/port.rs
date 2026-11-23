//! The Rust port of `open-sse/services/model.js`.
//!
//! Replace each `unimplemented!` with the port of the source function. Return `Err` where
//! the source throws or raises: the caller gets an error there too. After changing a
//! signature, run `rust-anything refresh` to regenerate the bindings. `rust-anything diff` checks
//! behavior against what the source recorded.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::{Value, json};

use crate::js::{as_number, is_undefined, string_units, utf16_string};

static PROVIDER_ALIASES: OnceLock<Result<HashMap<String, String>, serde_json::Error>> =
    OnceLock::new();
const SLASH: u16 = b'/' as u16;

fn provider_aliases() -> Result<&'static HashMap<String, String>, String> {
    PROVIDER_ALIASES
        .get_or_init(|| serde_json::from_str(include_str!("../provider-aliases.json")))
        .as_ref()
        .map_err(ToString::to_string)
}

pub fn provider_id(alias: &str) -> Result<&str, String> {
    Ok(known_provider_id(alias)?.unwrap_or(alias))
}

pub fn known_provider_id(alias: &str) -> Result<Option<&'static str>, String> {
    Ok(provider_aliases()?.get(alias).map(String::as_str))
}

pub fn resolve_provider_alias(alias_or_id: Value) -> Result<Value, String> {
    let Some(key) = property_key(&alias_or_id)? else {
        return Ok(alias_or_id);
    };
    Ok(known_provider_id(&key)?.map_or(alias_or_id, |provider| json!(provider)))
}

pub fn parse_model(model_str: Value) -> Result<Value, String> {
    if !truthy(&model_str) {
        return Ok(json!({
            "provider": null, "model": null, "isAlias": false, "providerAlias": null
        }));
    }

    let split = if let Some(units) = string_units(&model_str) {
        split_model_string(&units)
    } else if let Some(items) = model_str.as_array() {
        items
            .iter()
            .position(|item| item.as_str() == Some("/"))
            .map(|slash| (json!(items[..slash]), json!(items[slash + 1..])))
    } else {
        return Err("modelStr.includes is not a function".into());
    };

    if let Some((alias, model)) = split {
        let provider = resolve_provider_alias(alias.clone())?;
        Ok(
            json!({ "provider": provider, "model": model, "isAlias": false, "providerAlias": alias }),
        )
    } else {
        Ok(json!({ "provider": null, "model": model_str, "isAlias": true, "providerAlias": null }))
    }
}

pub fn resolve_model_alias_from_map(alias: Value, aliases: Value) -> Result<Value, String> {
    if !truthy(&aliases) {
        return Ok(Value::Null);
    }
    let Some(key) = property_key(&alias)? else {
        return Ok(Value::Null);
    };
    let resolved = aliases
        .as_object()
        .and_then(|map| map.get(&key))
        .or_else(|| {
            let index: usize = key.parse().ok()?;
            if index.to_string() != key {
                return None;
            }
            aliases.as_array()?.get(index)
        });
    let Some(resolved) = resolved.filter(|item| truthy(item)) else {
        return Ok(Value::Null);
    };
    resolve_model_alias_target(resolved)
}

pub fn resolve_model_alias_target(resolved: &Value) -> Result<Value, String> {
    if let Some(units) = string_units(resolved) {
        if let Some((alias, model)) = split_model_string(&units) {
            let provider = resolve_provider_alias(alias)?;
            return Ok(json!({ "provider": provider, "model": model }));
        }
        return Ok(Value::Null);
    }

    let provider = &resolved["provider"];
    let model = &resolved["model"];
    if truthy(provider) && truthy(model) {
        return Ok(
            json!({ "provider": resolve_provider_alias(provider.clone())?, "model": model }),
        );
    }
    Ok(Value::Null)
}

fn split_model_string(units: &[u16]) -> Option<(Value, Value)> {
    let slash = units.iter().position(|&unit| unit == SLASH)?;
    Some((
        utf16_string(&units[..slash]),
        utf16_string(&units[slash + 1..]),
    ))
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        _ if is_undefined(value) => false,
        _ => as_number(value).is_none_or(|n| n != 0.0 && !n.is_nan()),
    }
}

fn property_key(value: &Value) -> Result<Option<String>, String> {
    if let Some(units) = string_units(value) {
        return Ok(String::from_utf16(&units).ok());
    }
    if let Some(number) = as_number(value) {
        return Ok(Some(ryu_js::Buffer::new().format(number).to_owned()));
    }
    if let Some(items) = value.as_array() {
        let mut parts = Vec::new();
        for item in items {
            if item.is_null() || is_undefined(item) {
                parts.push(String::new());
            } else if let Some(part) = property_key(item)? {
                parts.push(part);
            } else {
                return Ok(None);
            }
        }
        return Ok(Some(parts.join(",")));
    }
    let text = match value {
        _ if is_undefined(value) => "undefined",
        Value::Null => "null",
        Value::Bool(true) => "true",
        Value::Bool(false) => "false",
        _ if value.get("toString").is_some() => {
            return Err("Cannot convert object to primitive value".into());
        }
        _ => "[object Object]",
    };
    Ok(Some(text.into()))
}
