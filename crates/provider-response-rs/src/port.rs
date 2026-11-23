//! The Rust port of `src/lib/providerConnectionResponse.js`.
//!
//! Replace each `unimplemented!` with the port of the source function. Return `Err` where
//! the source throws or raises: the caller gets an error there too. After changing a
//! signature, run `rust-anything refresh` to regenerate the bindings. `rust-anything diff` checks
//! behavior against what the source recorded.

#![forbid(unsafe_code)]

use std::sync::OnceLock;

use serde_json::{Map, Value};

static AWS_PROVIDERS: OnceLock<Result<Vec<Value>, String>> = OnceLock::new();

pub fn provider_index() -> Result<&'static [Value], String> {
    static INDEX: OnceLock<Result<Vec<Value>, String>> = OnceLock::new();
    INDEX
        .get_or_init(|| {
            serde_json::from_str(include_str!("../provider-index.json"))
                .map_err(|error| error.to_string())
        })
        .as_deref()
        .map_err(Clone::clone)
}

pub fn uses_aws_credential_form(provider: Value) -> Result<Value, String> {
    let ids = AWS_PROVIDERS
        .get_or_init(|| {
            Ok(provider_index()?
                .iter()
                .filter(|row| row["credentialForm"] == "aws")
                .map(|row| row["id"].clone())
                .collect())
        })
        .as_ref()
        .map_err(Clone::clone)?;
    Ok(Value::Bool(
        property_key(&provider).is_some_and(|key| ids.iter().any(|id| id == key)),
    ))
}

pub fn to_provider_connection_response(connection: Value) -> Result<Value, String> {
    if connection.is_null() || crate::js::is_undefined(&connection) {
        let kind = if connection.is_null() {
            "null"
        } else {
            "undefined"
        };
        return Err(format!(
            "Cannot destructure property 'apiKey' of 'connection' as it is {kind}."
        ));
    }
    let mut safe = spread(connection);
    for key in ["apiKey", "accessToken", "refreshToken", "idToken"] {
        safe.shift_remove(key);
    }
    if uses_aws_credential_form(safe.get("provider").cloned().unwrap_or_default())? == true
        && let Some(data) = safe
            .get_mut("providerSpecificData")
            .filter(|data| truthy(data))
    {
        let mut details = spread(data.take());
        details.shift_remove("sessionToken");
        *data = Value::Object(details);
    }
    Ok(Value::Object(safe))
}

fn property_key(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s),
        Value::Array(items) if items.len() == 1 => property_key(&items[0]),
        _ => None,
    }
}

fn truthy(value: &Value) -> bool {
    if let Some(n) = crate::js::as_number(value) {
        return n != 0.0 && !n.is_nan();
    }
    if let Some(s) = crate::js::string_units(value) {
        return !s.is_empty();
    }
    !value.is_null() && value != &Value::Bool(false) && !crate::js::is_undefined(value)
}

fn spread(value: Value) -> Map<String, Value> {
    if let Some(units) = crate::js::string_units(&value) {
        return units
            .into_iter()
            .enumerate()
            .map(|(index, unit)| (index.to_string(), crate::js::utf16_string(&[unit])))
            .collect();
    }
    if crate::js::as_number(&value).is_some() || crate::js::is_undefined(&value) {
        return Map::new();
    }
    match value {
        Value::Object(map) => map,
        Value::Array(items) => items
            .into_iter()
            .enumerate()
            .map(|(index, item)| (index.to_string(), item))
            .collect(),
        _ => Map::new(),
    }
}
