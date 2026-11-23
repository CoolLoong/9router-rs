//! The Rust port of `src/shared/utils/keyAccess.js`.
//!
//! Replace each `unimplemented!` with the port of the source function. Return `Err` where
//! the source throws or raises: the caller gets an error there too. After changing a
//! signature, run `rust-anything refresh` to regenerate the bindings. `rust-anything diff` checks
//! behavior against what the source recorded.

#![forbid(unsafe_code)]

use crate::js::{as_number, is_undefined, string_units, utf16_string};
use js_json_codec::{parse_array_strings, stringify_string_array};
use serde_json::{Value, json};
use std::collections::HashSet;

const MAX_ENTRIES: usize = 200;
const MAX_ENTRY_LENGTH: usize = 256;

/// Port of `normalizeAllowList` (`src/shared/utils/keyAccess.js:13`).
pub fn normalize_allow_list(list: Value) -> Result<Value, String> {
    Ok(allow_value(normalize(array_strings(&list))))
}

/// Port of `keyAccessFromColumns` (`src/shared/utils/keyAccess.js:36`).
pub fn key_access_from_columns(restricted_col: Value, allow_col: Value) -> Result<Value, String> {
    let restricted = restricted_col == Value::Bool(true) || as_number(&restricted_col) == Some(1.0);
    let allow = if restricted {
        let entries = string_units(&allow_col)
            .and_then(|source| parse_array_strings(&source))
            .unwrap_or_default();
        allow_value(normalize(entries))
    } else {
        json!([])
    };
    Ok(json!({ "restricted": restricted, "allow": allow }))
}

/// Port of `keyAccessToColumns` (`src/shared/utils/keyAccess.js:47`).
pub fn key_access_to_columns(access: Value) -> Result<Value, String> {
    let restricted = access.get("restricted").is_some_and(truthy);
    let allow = match access.get("allow").filter(|_| restricted) {
        Some(value) => stringify_string_array(normalize(array_strings(value)))
            .map_err(|err| err.to_string())?,
        None => "[]".to_owned(),
    };
    Ok(json!({ "accessRestricted": u8::from(restricted), "accessAllow": allow }))
}

/// Port of `validateKeyAccessInput` (`src/shared/utils/keyAccess.js:60`).
pub fn validate_key_access_input(input: Value) -> Result<Value, String> {
    let output = match validate_fields(&input) {
        Ok(value) => json!({ "ok": true, "value": value }),
        Err(error) => json!({ "ok": false, "error": error }),
    };
    Ok(output)
}

fn validate_fields(input: &Value) -> Result<Value, String> {
    let fields = input
        .as_object()
        .filter(|_| {
            !is_undefined(input) && string_units(input).is_none() && as_number(input).is_none()
        })
        .ok_or("access must be an object { restricted, allow }")?;
    let mut extra: Vec<_> = fields
        .keys()
        .map(String::as_str)
        .filter(|key| !matches!(*key, "restricted" | "allow"))
        .collect();
    extra.sort_by_key(|key| {
        key.parse::<u32>()
            .ok()
            .filter(|index| index.to_string() == *key)
            .unwrap_or(u32::MAX)
    });
    if !extra.is_empty() {
        return Err(format!("access: unexpected field(s) {}", extra.join(", ")));
    }
    let restricted = fields
        .get("restricted")
        .and_then(Value::as_bool)
        .ok_or("access.restricted must be a boolean")?;
    let allow = fields.get("allow").filter(|value| !is_undefined(value));
    let entries = match allow {
        None => Vec::new(),
        Some(list) => list
            .as_array()
            .and_then(|items| items.iter().map(string_units).collect())
            .ok_or("access.allow must be an array of strings")?,
    };
    if entries.len() > MAX_ENTRIES {
        return Err(format!("access.allow has more than {MAX_ENTRIES} entries"));
    }
    if entries
        .iter()
        .any(|entry| trim(entry).len() > MAX_ENTRY_LENGTH)
    {
        return Err(format!(
            "access.allow entries must be at most {MAX_ENTRY_LENGTH} characters"
        ));
    }
    Ok(json!({ "restricted": restricted, "allow": allow_value(normalize(entries)) }))
}

fn allow_value(entries: Vec<Vec<u16>>) -> Value {
    Value::Array(entries.iter().map(|entry| utf16_string(entry)).collect())
}

fn array_strings(value: &Value) -> impl Iterator<Item = Vec<u16>> + '_ {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(string_units)
}

fn truthy(value: &Value) -> bool {
    if let Some(number) = as_number(value) {
        return number != 0.0 && !number.is_nan();
    }
    if let Some(units) = string_units(value) {
        return !units.is_empty();
    }
    !(is_undefined(value) || value.is_null() || value == &Value::Bool(false))
}

fn trim(entry: &[u16]) -> &[u16] {
    let whitespace = |unit: &u16| {
        matches!(
            unit,
            0x0009..=0x000D | 0x0020 | 0x00A0 | 0x1680 | 0x2000..=0x200A
                | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
        )
    };
    let Some(start) = entry.iter().position(|unit| !whitespace(unit)) else {
        return &[];
    };
    let end = entry
        .iter()
        .rposition(|unit| !whitespace(unit))
        .map_or(start, |index| index + 1);
    &entry[start..end]
}

fn normalize(entries: impl IntoIterator<Item = Vec<u16>>) -> Vec<Vec<u16>> {
    let mut seen = HashSet::new();
    entries
        .into_iter()
        .filter_map(|entry| {
            let trimmed = trim(&entry);
            (!trimmed.is_empty() && seen.insert(lowercase(trimmed))).then(|| trimmed.to_vec())
        })
        .collect()
}

fn lowercase(units: &[u16]) -> Vec<u16> {
    if let Ok(text) = String::from_utf16(units) {
        return text.to_lowercase().encode_utf16().collect();
    }
    let mut folded = Vec::new();
    let mut run = String::new();
    for scalar in char::decode_utf16(units.iter().copied()) {
        match scalar {
            Ok(ch) => run.push(ch),
            Err(unpaired) => {
                folded.extend(run.to_lowercase().encode_utf16());
                run.clear();
                folded.push(unpaired.unpaired_surrogate());
            }
        }
    }
    folded.extend(run.to_lowercase().encode_utf16());
    folded
}
