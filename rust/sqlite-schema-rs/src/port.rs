//! The Rust port of `src/lib/db/schema.js`.
//!
//! Replace each `unimplemented!` with the port of the source function. Return `Err` where
//! the source throws or raises: the caller gets an error there too. After changing a
//! signature, run `rust-anything refresh` to regenerate the bindings. `rust-anything diff` checks
//! behavior against what the source recorded.

#![forbid(unsafe_code)]

use serde_json::Value;

use crate::js::{as_number, is_undefined, string_units, utf16_string};

pub fn build_create_table_sql(name: Value, def: Value) -> Result<Value, String> {
    if def.is_null() || is_undefined(&def) {
        let value = if def.is_null() { "null" } else { "undefined" };
        return Err(format!(
            "Cannot read properties of {value} (reading 'columns')"
        ));
    }
    let columns = &def["columns"];
    if columns.is_null() || is_undefined(columns) {
        return Err("Cannot convert undefined or null to object".into());
    }

    let mut definitions = Vec::new();
    if let Some(units) = string_units(columns) {
        for (index, unit) in units.into_iter().enumerate() {
            let mut definition: Vec<u16> = format!("{index} ").encode_utf16().collect();
            definition.push(unit);
            definitions.push(definition);
        }
    } else if let Some(items) = columns.as_array() {
        for (index, value) in items.iter().enumerate() {
            definitions.push(column_definition(&index.to_string(), value)?);
        }
    } else if let Some(object) = columns.as_object()
        && as_number(columns).is_none()
    {
        let mut entries: Vec<_> = object.iter().collect();
        entries.sort_by_key(|(key, _)| array_index(key));
        for (key, value) in entries {
            definitions.push(column_definition(key, value)?);
        }
    }

    let mut sql: Vec<u16> = "CREATE TABLE IF NOT EXISTS ".encode_utf16().collect();
    sql.extend(js_string(&name)?);
    sql.extend(" (".encode_utf16());
    let primary_key = &def["primaryKey"];
    if truthy(primary_key) {
        definitions.push(js_string(primary_key)?);
    }
    for (index, definition) in definitions.into_iter().enumerate() {
        if index != 0 {
            sql.extend(", ".encode_utf16());
        }
        sql.extend(definition);
    }
    sql.push(u16::from(b')'));
    Ok(utf16_string(&sql))
}

fn array_index(key: &str) -> u32 {
    if let Ok(index) = key.parse::<u32>()
        && index.to_string() == key
    {
        return index;
    }
    u32::MAX
}

fn column_definition(key: &str, value: &Value) -> Result<Vec<u16>, String> {
    let mut units: Vec<u16> = key.encode_utf16().collect();
    units.push(u16::from(b' '));
    units.extend(js_string(value)?);
    Ok(units)
}

fn truthy(value: &Value) -> bool {
    if is_undefined(value) {
        return false;
    }
    if let Some(number) = as_number(value) {
        return number != 0.0 && !number.is_nan();
    }
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::String(value) => !value.is_empty(),
        _ => true,
    }
}

fn js_string(value: &Value) -> Result<Vec<u16>, String> {
    if let Some(units) = string_units(value) {
        return Ok(units);
    }
    if let Some(number) = as_number(value) {
        return Ok(ryu_js::Buffer::new()
            .format(number)
            .encode_utf16()
            .collect());
    }
    if is_undefined(value) {
        return Ok("undefined".encode_utf16().collect());
    }
    if let Some(items) = value.as_array() {
        let mut units = Vec::new();
        for (index, item) in items.iter().enumerate() {
            if index != 0 {
                units.push(u16::from(b','));
            }
            if !item.is_null() && !is_undefined(item) {
                units.extend(js_string(item)?);
            }
        }
        return Ok(units);
    }
    let text = match value {
        Value::Null => "null",
        Value::Bool(true) => "true",
        Value::Bool(false) => "false",
        Value::Object(object) if object.contains_key("toString") => {
            return Err("Cannot convert object to primitive value".into());
        }
        _ => "[object Object]",
    };
    Ok(text.encode_utf16().collect())
}
