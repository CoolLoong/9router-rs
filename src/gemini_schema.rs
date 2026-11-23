use std::collections::HashSet;

use serde_json::{Map, Value, json};

use crate::chat_input::truthy;
use crate::chat_output::js_text;

const UNSUPPORTED: &[&str] = &[
    "minLength",
    "maxLength",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minItems",
    "maxItems",
    "format",
    "multipleOf",
    "uniqueItems",
    "contains",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contentSchema",
    "prefixItems",
    "additionalItems",
    "default",
    "examples",
    "$schema",
    "$defs",
    "definitions",
    "const",
    "$ref",
    "$comment",
    "deprecated",
    "readOnly",
    "writeOnly",
    "additionalProperties",
    "propertyNames",
    "patternProperties",
    "enumDescriptions",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
    "dependencies",
    "dependentSchemas",
    "dependentRequired",
    "title",
    "optional",
    "~optional",
    "if",
    "then",
    "else",
    "contentMediaType",
    "contentEncoding",
    "cornerRadius",
    "fillColor",
    "fontFamily",
    "fontSize",
    "fontWeight",
    "gap",
    "padding",
    "strokeColor",
    "strokeThickness",
    "textColor",
    "errorMessage",
    "errorMessages",
    "markdownDescription",
    "minProperties",
    "maxProperties",
];

pub fn clean(mut schema: Value) -> Value {
    for transform in [
        const_to_enum,
        stringify_enum,
        merge_all_of,
        convert_prefix_items,
        flatten_variants,
        flatten_types,
        infer_object,
        ensure_items,
        remove_keywords,
        cleanup_required,
        add_placeholders,
    ] {
        walk(&mut schema, transform, false);
    }
    walk(&mut schema, enforce_type, true);
    schema
}

fn walk(node: &mut Value, transform: fn(&mut Map<String, Value>), leaves_first: bool) {
    match node {
        Value::Array(values) => {
            for value in values {
                walk(value, transform, leaves_first);
            }
        }
        Value::Object(fields) => {
            if !leaves_first {
                transform(fields);
            }
            for value in fields.values_mut() {
                walk(value, transform, leaves_first);
            }
            if leaves_first {
                transform(fields);
            }
        }
        _ => (),
    }
}

fn const_to_enum(fields: &mut Map<String, Value>) {
    if !fields.get("enum").is_some_and(truthy)
        && let Some(constant) = fields.shift_remove("const")
    {
        fields.insert("enum".into(), json!([constant]));
    }
}

fn stringify_enum(fields: &mut Map<String, Value>) {
    if let Some(values) = fields.get_mut("enum").and_then(Value::as_array_mut) {
        for value in values {
            *value = json!(js_text(value));
        }
        if !fields.get("type").is_some_and(truthy) {
            fields.insert("type".into(), json!("string"));
        }
    }
}

fn merge_all_of(fields: &mut Map<String, Value>) {
    let Some(Value::Array(items)) = fields.shift_remove("allOf") else {
        return;
    };
    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        if let Some(obj) = item["properties"].as_object() {
            properties.extend(obj.clone());
        }
        if let Some(keys) = item["required"].as_array() {
            required.extend(
                keys.iter()
                    .filter(|key| seen.insert(key.to_string()))
                    .cloned(),
            );
        }
    }
    if !properties.is_empty() {
        if let Some(current) = fields.get_mut("properties").and_then(Value::as_object_mut) {
            current.extend(properties);
        } else {
            fields.insert("properties".into(), Value::Object(properties));
        }
    }
    if !required.is_empty() {
        if let Some(current) = fields.get_mut("required").and_then(Value::as_array_mut) {
            current.extend(required);
        } else {
            fields.insert("required".into(), Value::Array(required));
        }
    }
}

fn convert_prefix_items(fields: &mut Map<String, Value>) {
    let Some(Value::Array(items)) = fields.shift_remove("prefixItems") else {
        return;
    };
    if fields.get("items").is_some_and(truthy) {
        return;
    }
    let variants = items
        .into_iter()
        .filter(|item| truthy(item) && item["type"] != "null")
        .collect::<Vec<_>>();
    let items = match variants.as_slice() {
        [] => return,
        [item] => item.clone(),
        _ => json!({ "anyOf": variants }),
    };
    fields.insert("items".into(), items);
}

fn flatten_variants(fields: &mut Map<String, Value>) {
    for kind in ["anyOf", "oneOf"] {
        let Some(Value::Array(items)) = fields.get(kind) else {
            continue;
        };
        let selected = items
            .iter()
            .rev()
            .filter(|item| truthy(item) && item["type"] != "null")
            .max_by_key(|item| {
                if item["type"] == "object" || truthy(&item["properties"]) {
                    3
                } else if item["type"] == "array" || truthy(&item["items"]) {
                    2
                } else if truthy(&item["type"]) {
                    1
                } else {
                    0
                }
            })
            .and_then(Value::as_object)
            .cloned();
        if let Some(selected) = selected {
            fields.shift_remove(kind);
            fields.extend(selected);
        }
    }
}

fn flatten_types(fields: &mut Map<String, Value>) {
    let Some(Value::Array(types)) = fields.get("type") else {
        return;
    };
    let mut types = types.iter().filter(|value| **value != "null");
    let first = types.next();
    let selected = if let Some(first) = first {
        let types = std::iter::once(first).chain(types).collect::<Vec<_>>();
        if types.len() == 1 {
            first.clone()
        } else {
            let properties = fields.get("properties").is_some_and(truthy);
            let items = fields
                .get("items")
                .is_some_and(|items| truthy(&items["type"]));
            ["object", "array", "string", "number", "integer", "boolean"]
                .into_iter()
                .filter(|name| match *name {
                    "object" => properties,
                    "array" => items,
                    _ => true,
                })
                .find(|name| types.iter().any(|value| **value == *name))
                .map_or_else(|| first.clone(), |name| json!(name))
        }
    } else {
        json!("string")
    };
    fields.insert("type".into(), selected);
}

fn infer_object(fields: &mut Map<String, Value>) {
    if fields.get("properties").is_some_and(truthy) && !fields.get("type").is_some_and(truthy) {
        fields.insert("type".into(), json!("object"));
    }
}

fn ensure_items(fields: &mut Map<String, Value>) {
    if fields.get("type").and_then(Value::as_str) != Some("array") {
        return;
    }
    if let Some(items) = fields.get_mut("items").and_then(Value::as_object_mut) {
        if !items.get("type").is_some_and(truthy) {
            let kind = if items.get("properties").is_some_and(truthy) {
                "object"
            } else if items.get("items").is_some_and(truthy) {
                "array"
            } else {
                "string"
            };
            items.insert("type".into(), json!(kind));
        }
    } else {
        fields.insert("items".into(), json!({ "type": "string" }));
    }
}

fn remove_keywords(fields: &mut Map<String, Value>) {
    fields.retain(|key, _| !key.starts_with("x-") && !UNSUPPORTED.contains(&key.as_str()));
}

fn cleanup_required(fields: &mut Map<String, Value>) {
    if let (Some(Value::Array(required)), Some(properties)) =
        (fields.get("required"), fields.get("properties"))
        && truthy(properties)
    {
        let required = required
            .iter()
            .filter(|key| properties.get(js_text(key)).is_some())
            .cloned()
            .collect::<Vec<_>>();
        if required.is_empty() {
            fields.shift_remove("required");
        } else {
            fields.insert("required".into(), Value::Array(required));
        }
    }
}

fn add_placeholders(fields: &mut Map<String, Value>) {
    if fields.is_empty() {
        fields.insert("type".into(), json!("object"));
    }
    if fields.get("type").and_then(Value::as_str) == Some("object")
        && fields
            .get("properties")
            .and_then(Value::as_object)
            .is_none_or(Map::is_empty)
    {
        fields.insert("properties".into(), json!({
            "reason": { "type": "string", "description": "Brief explanation of why you are calling this tool" }
        }));
        fields.insert("required".into(), json!(["reason"]));
    }
}

fn enforce_type(fields: &mut Map<String, Value>) {
    match fields.get("type").and_then(Value::as_str) {
        Some("array") => ensure_items(fields),
        _ => {
            fields.shift_remove("items");
        }
    }
    if fields.get("type").and_then(Value::as_str) != Some("object") {
        fields.shift_remove("properties");
        fields.shift_remove("required");
    }
}
