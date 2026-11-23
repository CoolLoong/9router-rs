//! The Rust port of open-sse/providers/capabilities.js.

#![forbid(unsafe_code)]

use crate::js::{as_number, is_undefined, number, string_units, undefined};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::borrow::Cow;
use std::sync::LazyLock;

#[path = "api.rs"]
mod api;
pub use api::{combo_capabilities, model_capabilities, service_kind_capabilities};

static META: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("provider-capabilities.json"))
        .expect("provider capabilities metadata is generated from upstream")
});
static NOT_VISION: LazyLock<Regex> = LazyLock::new(|| vision_regex("notVision"));
static VISION_NAME: LazyLock<Regex> = LazyLock::new(|| vision_regex("visionName"));

fn vision_regex(key: &str) -> Regex {
    Regex::new(
        META[key]
            .as_str()
            .expect("vision metadata field is a string"),
    )
    .expect("upstream vision regex uses supported syntax")
}

fn nullish(value: &Value) -> bool {
    value.is_null() || is_undefined(value)
}

fn string_value(value: &Value) -> Option<Cow<'_, str>> {
    if let Some(text) = value.as_str() {
        Some(Cow::Borrowed(text))
    } else {
        Some(Cow::Owned(String::from_utf16_lossy(&string_units(value)?)))
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        _ if is_undefined(value) => false,
        _ => as_number(value).is_none_or(|n| n != 0.0 && !n.is_nan()),
    }
}

fn js_text(value: &Value) -> String {
    if let Some(text) = string_value(value) {
        return text.into_owned();
    }
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                if nullish(item) {
                    String::new()
                } else {
                    js_text(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) if is_undefined(value) => "undefined".to_string(),
        Value::Object(_) => "[object Object]".to_string(),
        other => other.to_string(),
    }
}

fn js_number(value: &Value) -> f64 {
    if let Some(n) = as_number(value) {
        return n;
    }
    match value {
        Value::Null | Value::Bool(false) => 0.0,
        Value::Bool(true) => 1.0,
        Value::String(_) | Value::Array(_) => {
            let value = js_text(value);
            let trimmed = value
                .trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}');
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse().unwrap_or(f64::NAN)
            }
        }
        _ => f64::NAN,
    }
}

fn nullish_property(value: &Value, prop: &str) -> String {
    let kind = if value.is_null() { "null" } else { "undefined" };
    format!("TypeError: Cannot read properties of {kind} (reading '{prop}')")
}

fn glob_matches(pattern: &str, model: &str) -> bool {
    let mut parts = pattern.split('*');
    let start = parts.next().unwrap_or_default();
    let Some(rest) = model.strip_prefix(start) else {
        return false;
    };
    let Some(end) = parts.next_back() else {
        return rest.is_empty();
    };
    if model.contains(['\r', '\n', '\u{2028}', '\u{2029}']) {
        return false;
    }
    let Some(mut rest) = rest.strip_suffix(end) else {
        return false;
    };
    for part in parts {
        let Some(index) = rest.find(part) else {
            return false;
        };
        rest = &rest[index + part.len()..];
    }
    true
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn merged(base: Option<&Value>) -> Map<String, Value> {
    let mut all = object(&META["defaults"]);
    if let Some(fields) = base.and_then(Value::as_object) {
        all.extend(fields.clone());
    }
    all
}

fn with_catalog(
    result: &mut Map<String, Value>,
    provider: &str,
    model: &str,
    base: &str,
    catalog: &Value,
) {
    let lower_model = base.to_lowercase();
    let base_id = lower_model.split(':').next().unwrap_or_default();
    if let Some(modalities) = catalog["models"].get(format!("{provider}:{base_id}")) {
        for key in ["vision", "pdf", "audioInput", "videoInput"] {
            if modalities[key] == true {
                result.insert(key.into(), Value::Bool(true));
            }
        }
    }
    let by_provider = &catalog["providers"][provider];
    let limits = by_provider
        .get(model)
        .filter(|entry| truthy(entry))
        .or_else(|| by_provider.get(base_id));
    if let Some(limits) = limits {
        for key in ["contextWindow", "maxOutput"] {
            if let Some(value) = limits.get(key).filter(|value| js_number(value) > 0.0) {
                result.insert(key.into(), value.clone());
            }
        }
    }
}

fn promotion_vision(model: &str) -> bool {
    let lower = model.to_lowercase();
    !NOT_VISION.is_match(&lower) && VISION_NAME.is_match(&lower)
}

pub fn capabilities_from_service_kind(kind: Value) -> Result<Value, String> {
    Ok(service_kind_capabilities(&js_text(&kind)).unwrap_or(Value::Null))
}

pub fn get_capabilities_for_model(
    provider: Value,
    model: Value,
    catalog: Option<Value>,
) -> Result<Value, String> {
    if !truthy(&model) {
        return Ok(META["defaults"].clone());
    }
    let model = if let Some(text) = string_value(&model) {
        text
    } else if let Some(items) = model.as_array() {
        if items.iter().any(|item| item == "/") {
            return Err("TypeError: model.split is not a function".into());
        }
        Cow::Owned(js_text(&model))
    } else {
        return Err("TypeError: model.includes is not a function".into());
    };
    let provider = truthy(&provider).then(|| js_text(&provider));
    Ok(model_capabilities(
        provider.as_deref(),
        &model,
        catalog.as_ref(),
    ))
}

fn aggregate_parts(all: &[Value]) -> Value {
    let mut result = Map::new();
    for key in [
        "vision",
        "pdf",
        "audioInput",
        "videoInput",
        "imageOutput",
        "audioOutput",
        "search",
    ] {
        result.insert(key.into(), json!(all.iter().any(|caps| truthy(&caps[key]))));
    }
    result.insert(
        "tools".into(),
        json!(all.iter().all(|caps| truthy(&caps["tools"]))),
    );
    for key in [
        "reasoning",
        "thinkingFormat",
        "thinkingCanDisable",
        "thinkingRange",
    ] {
        result.insert(
            key.into(),
            all[0].get(key).cloned().unwrap_or_else(undefined),
        );
    }
    for (key, narrow) in [("contextWindow", true), ("maxOutput", false)] {
        let mut values = all
            .iter()
            .map(|caps| caps.get(key).map_or(f64::NAN, js_number));
        let initial = if narrow {
            f64::INFINITY
        } else {
            f64::NEG_INFINITY
        };
        let limit = values
            .try_fold(initial, |current, value| {
                (!value.is_nan()).then(|| {
                    if narrow {
                        current.min(value)
                    } else {
                        current.max(value)
                    }
                })
            })
            .unwrap_or(f64::NAN);
        result.insert(key.into(), number(limit));
    }
    Value::Object(result)
}

fn combo_inner(
    models: &[Value],
    lookup: Option<&Map<String, Value>>,
    catalog: Option<&Value>,
    resolve: &impl Fn(&str) -> Result<Option<Value>, String>,
    depth: f64,
) -> Result<Option<Value>, String> {
    if models.is_empty() || depth > 6.0 {
        return Ok(None);
    }
    let mut all = Vec::with_capacity(models.len());
    for model in models {
        let id = if let Some(text) = string_value(model) {
            text
        } else if model.is_array() {
            Cow::Owned(js_text(model))
        } else if nullish(model) {
            return Err(nullish_property(model, "includes"));
        } else {
            return Err("TypeError: fullId.includes is not a function".into());
        };
        if !id.contains('/')
            && let Some(nested) = lookup
                .and_then(|lookup| lookup.get(id.as_ref()))
                .filter(|nested| truthy(nested))
        {
            let children = if let Some(children) = nested.as_array() {
                combo_inner(children, lookup, catalog, resolve, depth + 1.0)?
            } else if has_length(nested) && depth < 6.0 {
                return Err("TypeError: comboModels.map is not a function".into());
            } else {
                None
            };
            all.push(match children {
                Some(value) => value,
                None => resolve(&id)?.unwrap_or_else(|| model_capabilities(None, &id, catalog)),
            });
            continue;
        }
        let (provider, model) = id
            .split_once('/')
            .map_or((None, id.as_ref()), |(provider, model)| {
                (Some(provider), model)
            });
        let mut capability = model_capabilities(provider, model, catalog);
        if let Some(override_caps) = resolve(&id)?
            && let Some(fields) = capability.as_object_mut()
        {
            fields.extend(object(&override_caps));
        }
        all.push(capability);
    }
    Ok(Some(aggregate_parts(&all)))
}

fn has_length(value: &Value) -> bool {
    match value {
        Value::Array(items) => !items.is_empty(),
        Value::String(text) => !text.is_empty(),
        _ => value.get("length").is_some_and(truthy),
    }
}

pub fn aggregate_combo_capabilities(
    models: Value,
    lookup: Option<Value>,
    resolve_caps: Option<Value>,
    depth: Option<f64>,
) -> Result<Value, String> {
    let depth = depth.unwrap_or_default();
    if !has_length(&models) || depth > 6.0 {
        return Ok(Value::Null);
    }
    let models = models
        .as_array()
        .ok_or("TypeError: comboModels.map is not a function")?;
    let callback = |id: &str| match resolve_caps.as_ref() {
        Some(value) if let Some(overrides) = value.get("fixtureOverrides") => {
            Ok(overrides.get(id).filter(|value| !nullish(value)).cloned())
        }
        Some(value) if !nullish(value) => Err("TypeError: resolveCaps is not a function".into()),
        _ => Ok(None),
    };
    combo_inner(
        models,
        lookup.as_ref().and_then(Value::as_object),
        resolve_caps.as_ref().and_then(|value| value.get("catalog")),
        &callback,
        depth,
    )
    .map(|result| result.unwrap_or(Value::Null))
}

pub fn is_gemini4_model(model: Value) -> Result<Value, String> {
    let model = if truthy(&model) {
        js_text(&model).to_ascii_lowercase()
    } else {
        String::new()
    };
    Ok(json!(model.match_indices("gemini-4").any(
        |(start, needle)| {
            (start == 0 || model.as_bytes()[start - 1] == b'/')
                && model[start + needle.len()..]
                    .chars()
                    .next()
                    .is_none_or(|next| matches!(next, '-' | '.'))
        }
    )))
}
