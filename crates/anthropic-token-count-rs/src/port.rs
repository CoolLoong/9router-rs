//! The Rust port of `src/app/api/v1/messages/count_tokens/`.
//!
//! Replace each `unimplemented!` with the port of the source function. Return `Err` where
//! the source throws or raises: the caller gets an error there too. After changing a
//! signature, run `rust-anything refresh` to regenerate the bindings. `rust-anything diff` checks
//! behavior against what the source recorded.

#![forbid(unsafe_code)]

/// Port of `estimateAnthropicInputTokens` (`src/app/api/v1/messages/count_tokens/route.js:61`).
pub fn estimate_anthropic_input_tokens(
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    if body.is_null() {
        return Err("TypeError: Cannot read properties of null (reading 'messages')".to_owned());
    }
    let fields = count(&body["system"]) + count(&body["tools"]);
    let messages = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| {
            let content = &item["content"];
            if let Some(blocks) = content.as_array() {
                blocks.iter().map(block_chars).sum()
            } else {
                count(content)
            }
        })
        .sum::<usize>();
    Ok(serde_json::json!((fields + messages).div_ceil(4)))
}

fn block_chars(value: &serde_json::Value) -> usize {
    match value["type"].as_str() {
        Some("text") => count(&value["text"]),
        Some("tool_use") => count(&value["name"]) + count(&value["input"]),
        Some("tool_result") => count(&value["content"]),
        Some("thinking") => count(&value["thinking"]),
        _ => count(value),
    }
}

fn count(value: &serde_json::Value) -> usize {
    use serde_json::Value;
    match value {
        Value::Null => 0,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::String(value) => value.encode_utf16().count(),
        Value::Array(items) => items.iter().map(count).sum(),
        Value::Object(_) if crate::js::is_undefined(value) => 0,
        Value::Number(_) | Value::Object(_) if crate::js::as_number(value).is_some() => {
            crate::js::as_number(value).map_or(0, |value| ryu_js::Buffer::new().format(value).len())
        }
        Value::Object(fields) => crate::js::string_units(value).map_or_else(
            || {
                fields
                    .iter()
                    .map(|(key, value)| key.encode_utf16().count() + count(value))
                    .sum()
            },
            |units| units.len(),
        ),
        Value::Number(_) => 0,
    }
}
