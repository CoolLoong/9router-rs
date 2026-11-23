//! The Rust port of open-sse/providers/models/.

#![forbid(unsafe_code)]

use crate::js::{as_number, is_undefined, string_units, undefined, utf16_string};
use serde_json::{Map, Value, json};
use std::borrow::Cow;

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        _ if is_undefined(value) => false,
        _ => as_number(value).is_none_or(|n| n != 0.0 && !n.is_nan()),
    }
}

fn nullish_property(value: &Value, prop: &str) -> String {
    let nullish = if value.is_null() { "null" } else { "undefined" };
    format!("TypeError: Cannot read properties of {nullish} (reading '{prop}')")
}

fn string_value(value: &Value) -> Option<Cow<'_, str>> {
    if let Some(text) = value.as_str() {
        Some(Cow::Borrowed(text))
    } else {
        Some(Cow::Owned(String::from_utf16_lossy(&string_units(value)?)))
    }
}

fn object_fields(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(fields) if as_number(value).is_none() => fields.clone(),
        Value::Array(items) => items
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, item)| (index.to_string(), item))
            .collect(),
        _ => Map::new(),
    }
}

fn append_js_string(value: &Value, out: &mut Vec<u16>) -> Result<(), String> {
    if let Some(units) = string_units(value) {
        out.extend(units);
    } else if let Some(number) = as_number(value) {
        let text = match number {
            n if n.is_nan() => "NaN".to_string(),
            f64::INFINITY => "Infinity".to_string(),
            f64::NEG_INFINITY => "-Infinity".to_string(),
            0.0 => "0".to_string(),
            n if n.abs() >= 1e21 => format!("{n:e}").replace('e', "e+"),
            n if n.abs() < 1e-6 => format!("{n:e}"),
            n => n.to_string(),
        };
        out.extend(text.encode_utf16());
    } else {
        let text = match value {
            Value::Null => "null",
            Value::Bool(true) => "true",
            Value::Bool(false) => "false",
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    if index != 0 {
                        out.push(b',' as u16);
                    }
                    if !item.is_null() && !is_undefined(item) {
                        append_js_string(item, out)?;
                    }
                }
                return Ok(());
            }
            _ if is_undefined(value) => "undefined",
            _ if value.get("toString").is_some() => {
                return Err("TypeError: Cannot convert object to primitive value".into());
            }
            _ => "[object Object]",
        };
        out.extend(text.encode_utf16());
    }
    Ok(())
}

/// Port of withCodexReviewModels.
pub fn with_codex_review_models(models: Value) -> Result<Value, String> {
    let items = models.as_array().ok_or_else(|| {
        if models.is_null() || is_undefined(&models) {
            nullish_property(&models, "flatMap")
        } else {
            "TypeError: models.flatMap is not a function".to_string()
        }
    })?;
    let mut result = Vec::with_capacity(items.len() * 2);
    for model in items {
        if model.is_null() || is_undefined(model) {
            return Err(nullish_property(model, "kind"));
        }
        result.push(model.clone());
        if model_kind_value(model).as_str() != Some("llm") {
            continue;
        }
        let id = model.get("id").cloned().unwrap_or_else(undefined);
        let mut id_units = string_units(&id).ok_or_else(|| {
            if id.is_null() || is_undefined(&id) {
                nullish_property(&id, "endsWith")
            } else {
                "TypeError: model.id.endsWith is not a function".to_string()
            }
        })?;
        let suffix: Vec<u16> = "-review".encode_utf16().collect();
        if id_units.ends_with(&suffix) {
            continue;
        }
        id_units.extend(suffix);
        let mut name_units = Vec::new();
        append_js_string(
            &model.get("name").cloned().unwrap_or_else(undefined),
            &mut name_units,
        )?;
        name_units.extend(" Review".encode_utf16());
        let mut review = object_fields(model);
        review.insert("id".into(), utf16_string(&id_units));
        review.insert("name".into(), utf16_string(&name_units));
        review.insert(
            "upstreamModelId".into(),
            model
                .get("upstreamModelId")
                .filter(|value| truthy(value))
                .cloned()
                .unwrap_or(id),
        );
        review.insert("quotaFamily".into(), json!("review"));
        result.push(Value::Object(review));
    }
    Ok(Value::Array(result))
}

fn js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

fn strip_thinking(model: &str) -> &str {
    let trimmed = model.trim_matches(js_whitespace);
    let Some(head) = trimmed.strip_suffix(')') else {
        return trimmed;
    };
    let Some((id, thinking)) = head.rsplit_once('(') else {
        return trimmed;
    };
    if thinking.is_empty() || thinking.contains(')') {
        trimmed
    } else {
        id.trim_matches(js_whitespace)
    }
}

fn leaf_model(id: &str) -> &str {
    let base = strip_thinking(id);
    base.rsplit_once('/').map_or(base, |(_, leaf)| leaf)
}

fn muse_suffix(lowercase_id: &str) -> Option<&str> {
    let tail = lowercase_id.strip_prefix("muse")?;
    tail.strip_prefix(['-', '_'])
        .unwrap_or(tail)
        .strip_prefix("spark")
}

/// Port of isMuseSparkModel.
pub fn is_muse_spark_model(model_id: Value) -> Result<Value, String> {
    Ok(json!(string_value(&model_id).is_some_and(|id| {
        let id = leaf_model(&id).to_ascii_lowercase();
        muse_suffix(&id).is_some_and(|tail| {
            tail.chars()
                .next()
                .is_none_or(|first| "-_:.".contains(first) || js_whitespace(first))
        })
    })))
}

/// Port of isDeepSeekModel.
pub fn is_deep_seek_model(model_id: Value) -> Result<Value, String> {
    Ok(json!(string_value(&model_id).is_some_and(|id| {
        leaf_model(&id)
            .to_ascii_lowercase()
            .starts_with("deepseek-")
    })))
}

/// Port of isOpencodeFreeTierModel.
pub fn is_opencode_free_tier_model(model_id: Value) -> Result<Value, String> {
    Ok(json!(string_value(&model_id).is_some_and(|id| {
        leaf_model(&id).to_ascii_lowercase().ends_with("-free")
    })))
}

/// Port of opencodeFamilyFormats.
pub fn opencode_family_formats(model_id: Value) -> Result<Value, String> {
    let Some(input) = string_value(&model_id) else {
        return Ok(Value::Null);
    };
    let model = strip_thinking(&input);
    let lower = model.to_ascii_lowercase();
    Ok(
        if lower.starts_with("grok") || lower.starts_with("gpt") || muse_suffix(&lower).is_some() {
            json!({"match": {"$regexp": "/^(grok|gpt|muse[-_]?spark)/i"},
               "supportedFormats": ["openai-responses"], "targetFormat": "openai-responses"})
        } else if model.starts_with("deepseek-v4-pro") || model.starts_with("deepseek-v4-flash") {
            json!({"match": {"$regexp": "/^deepseek-v4-(pro|flash)/"},
               "supportedFormats": ["openai", "claude", "openai-responses"]})
        } else if model.starts_with("minimax") || model.starts_with("qwen") {
            json!({"match": {"$regexp": "/^(minimax|qwen)/"}, "supportedFormats": ["openai", "claude"]})
        } else if lower.starts_with("claude-") {
            json!({"match": {"$regexp": "/^claude-/i"}, "supportedFormats": ["claude"]})
        } else {
            Value::Null
        },
    )
}

fn decimal_version(text: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    text.split_once('.').map_or_else(
        || digits(text),
        |(major, minor)| digits(major) && digits(minor),
    )
}

fn ascii_digit(c: u16) -> bool {
    (b'0' as u16..=b'9' as u16).contains(&c)
}

fn title_case(prefix: &str, suffix: &[u16]) -> Value {
    let mut title: Vec<u16> = prefix.encode_utf16().collect();
    let parts = suffix.split(|c| {
        *c == b'-' as u16
            || *c == b'_' as u16
            || char::from_u32((*c).into()).is_some_and(js_whitespace)
    });
    let mut separator = false;
    for part in parts.filter(|part| !part.is_empty()) {
        if separator {
            title.push(b' ' as u16);
        }
        separator = true;
        let first = part[0];
        if let Some(letter) = char::from_u32(first.into()) {
            for code in letter.to_uppercase() {
                title.extend_from_slice(code.encode_utf16(&mut [0; 2]));
            }
        } else {
            title.push(first);
        }
        title.extend_from_slice(&part[1..]);
    }
    utf16_string(&title)
}

fn derive_name(id: Value) -> Value {
    let Some(units) = string_units(&id) else {
        return id;
    };
    let lower = String::from_utf16_lossy(&units).to_ascii_lowercase();
    if let Some(version) = lower.strip_prefix("kimi-k") {
        let n = version.strip_suffix("-thinking").unwrap_or(version);
        if decimal_version(n) {
            return json!(format!(
                "Kimi K{n}{}",
                if n == version { "" } else { " Thinking" }
            ));
        }
    }
    if let Some(version) = lower.strip_prefix("glm-") {
        let n = version.strip_suffix('v').unwrap_or(version);
        if decimal_version(n) {
            return json!(format!(
                "GLM {n}{}",
                if n == version { "" } else { "V (Vision)" }
            ));
        }
    }
    if let Some(n) = lower
        .strip_prefix("minimax-m")
        .filter(|n| decimal_version(n))
    {
        return json!(format!("MiniMax M{n}"));
    }
    for (model, display) in [
        ("gpt-", "GPT "),
        ("gemini-", "Gemini "),
        ("grok-", "Grok "),
        ("deepseek-", "DeepSeek "),
        ("qwen", "Qwen "),
    ] {
        if !lower.starts_with(model) {
            continue;
        }
        let tail = &units[model.len()..];
        if tail.is_empty()
            || tail
                .iter()
                .any(|u| matches!(u, 0x0a | 0x0d | 0x2028 | 0x2029))
        {
            break;
        }
        if model == "qwen" && !ascii_digit(tail[0]) && tail[0] != b'.' as u16 {
            break;
        }
        return title_case(display, tail);
    }
    id
}

/// Port of deriveModelName.
pub fn derive_model_name(id: Value) -> Result<Value, String> {
    Ok(derive_name(id))
}

/// Port of normalizeModelId.
pub fn normalize_model_id(model_id: Value) -> Result<Value, String> {
    let Some(units) = string_units(&model_id) else {
        return Ok(model_id);
    };
    let mut out = Vec::with_capacity(units.len());
    let mut tail = units.as_slice();
    while let Some((&first, rest)) = tail.split_first() {
        out.push(first);
        if ascii_digit(first)
            && let [delimiter, next, remainder @ ..] = rest
            && *delimiter == b'-' as u16
            && ascii_digit(*next)
        {
            out.extend([b'.' as u16, *next]);
            tail = remainder;
        } else {
            tail = rest;
        }
    }
    Ok(utf16_string(&out))
}

/// Port of normalizeModel.
pub fn normalize_model(raw: Value) -> Result<Value, String> {
    if raw.is_null() || is_undefined(&raw) {
        return Err(nullish_property(&raw, "name"));
    }
    if string_units(&raw).is_some() {
        return Ok(json!({"id": raw, "name": derive_name(raw.clone())}));
    }
    if raw.get("name").is_some_and(|value| !is_undefined(value)) {
        return Ok(raw);
    }
    let mut fields = object_fields(&raw);
    let name = derive_name(raw.get("id").cloned().unwrap_or_else(undefined));
    fields.insert("name".into(), name);
    Ok(Value::Object(fields))
}

fn property_or(model: &Value, property: &str, default: Value) -> Value {
    model
        .get(property)
        .filter(|value| truthy(value))
        .cloned()
        .unwrap_or(default)
}

fn model_kind_value(model: &Value) -> Value {
    property_or(model, "kind", property_or(model, "type", json!("llm")))
}

/// Port of modelKind.
pub fn model_kind(model: Value) -> Result<Value, String> {
    Ok(model_kind_value(&model))
}

/// Port of modelQuotaFamily.
pub fn model_quota_family(model: Value) -> Result<Value, String> {
    Ok(property_or(&model, "quotaFamily", json!("normal")))
}

/// Port of modelStrip.
pub fn model_strip(model: Value) -> Result<Value, String> {
    Ok(property_or(&model, "strip", json!([])))
}

/// Port of modelTargetFormat.
pub fn model_target_format(model: Value) -> Result<Value, String> {
    Ok(property_or(&model, "targetFormat", Value::Null))
}

/// Port of modelSupportedFormats.
pub fn model_supported_formats(model: Value) -> Result<Value, String> {
    Ok(property_or(&model, "supportedFormats", Value::Null))
}
