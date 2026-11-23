use std::collections::HashSet;
use std::sync::LazyLock;

use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use regex::Regex;
use serde_json::{Value, json};

use crate::chat_client::Client;
use crate::chat_input::{PreparedChat, js_space, stream_requested, truthy};
use crate::chat_output::js_text;
use crate::thinking_intent::model_suffix;

static HAIKU: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)haiku[-._]?([0-9]+)|(?:^|[^0-9.])([0-9]+)(?:[-._][0-9]+)?[-._]?haiku").unwrap()
});

pub fn matches(provider: &str, headers: &HeaderMap) -> bool {
    (matches!(provider, "anthropic" | "claude") || provider.starts_with("anthropic-compatible-"))
        && Client::detect(headers, &Value::Null) == Client::Claude
}

pub fn prepare(mut body: Value, model: &str, accept: &str) -> Option<PreparedChat> {
    if body["userAgent"] == "antigravity" {
        return None;
    }
    let stream = stream_requested(&body, accept);
    let (model, _) = model_suffix(model);
    let unsupported = model.to_ascii_lowercase().contains("haiku")
        && HAIKU
            .captures(model)
            .and_then(|captures| captures.get(1).or_else(|| captures.get(2)))
            .and_then(|major| major.as_str().parse::<u64>().ok())
            .is_none_or(|major| major < 5);
    if unsupported {
        if body["thinking"]["type"] == "adaptive" {
            body["thinking"] = json!({"type":"enabled","budget_tokens":10000});
        }
        if let Some(fields) = body.get_mut("output_config").and_then(Value::as_object_mut)
            && fields.get("effort").is_some_and(|value| !value.is_null())
        {
            fields.shift_remove("effort");
            if fields.is_empty() {
                body.as_object_mut()?.shift_remove("output_config");
            }
        }
    }
    let thinking = body["thinking"]["type"] == "enabled";
    let input = body.get_mut("messages")?.as_array_mut()?;
    let prefill = input
        .last()
        .is_some_and(|message| message["role"] == "assistant");
    let mut messages: Vec<Value> = Vec::new();
    for mut message in std::mem::take(input) {
        message.as_object_mut()?;
        if message["content"].is_object() {
            let mut block = std::mem::take(&mut message["content"]);
            block.as_object_mut()?.shift_remove("cache_control");
            message["content"] = json!([block]);
        }
        if message["role"] == "system" {
            let text = message_text(&message["content"]);
            if text.trim_matches(js_space).is_empty() {
                continue;
            }
            let block = json!({"type":"text","text":text});
            if let Some(previous) = messages
                .last_mut()
                .filter(|message| message["role"] == "user")
            {
                match &mut previous["content"] {
                    Value::Array(content) => content.push(block),
                    Value::String(text) => {
                        previous["content"] = json!([{"type":"text","text":text},block])
                    }
                    _ => previous["content"] = json!([block]),
                }
            } else {
                messages.push(json!({"role":"user","content":[block]}));
            }
        } else {
            messages.push(message);
        }
    }
    let mut removed = HashSet::new();
    for message in messages
        .iter_mut()
        .filter(|message| message["role"] == "assistant")
    {
        if let Some(content) = message["content"].as_array_mut() {
            content.retain(|block| {
                if is_thinking(block) {
                    return valid_signature(&block["signature"]);
                }
                if block["type"] == "server_tool_use" {
                    let id = block["id"].as_str().unwrap_or_default();
                    let valid = id.strip_prefix("srvtoolu_").is_some_and(|name| {
                        !name.is_empty()
                            && name
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    });
                    if !valid && !block["id"].is_null() {
                        removed.insert(js_text(&block["id"]));
                    }
                    return valid;
                }
                true
            });
            if thinking
                && content.iter().any(|block| block["type"] == "tool_use")
                && !content.iter().any(is_thinking)
            {
                content.insert(
                    0,
                    json!({
                        "type":"thinking","thinking":".",
                        "signature":include_str!("claude-thinking-signature.txt"),
                    }),
                );
            }
        }
    }
    messages.retain_mut(|message| match &mut message["content"] {
        Value::String(text) => !text.trim_matches(js_space).is_empty(),
        Value::Array(content) => {
            content.retain(|block| {
                if matches!(
                    block["type"].as_str(),
                    Some("tool_result" | "web_search_tool_result")
                ) && removed.contains(&js_text(&block["tool_use_id"]))
                {
                    return false;
                }
                let text_field = match block["type"].as_str() {
                    Some("text") => "text",
                    Some("thinking") => "thinking",
                    _ => return true,
                };
                !block[text_field].is_null()
                    && !js_text(&block[text_field])
                        .trim_matches(js_space)
                        .is_empty()
            });
            !content.is_empty()
        }
        _ => true,
    });
    if !prefill
        && messages
            .last()
            .is_some_and(|message| message["role"] == "assistant")
    {
        messages.push(json!({"role":"user","content":[{"type":"text","text":"Continue."}]}));
    }
    *input = messages;
    body["model"] = json!(model);
    Some(PreparedChat { body, stream })
}

fn message_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .map(|block| {
            if block.is_string() {
                js_text(block)
            } else if truthy(&block["text"]) {
                js_text(&block["text"])
            } else {
                String::new()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_thinking(block: &Value) -> bool {
    matches!(
        block["type"].as_str(),
        Some("thinking" | "redacted_thinking")
    )
}

fn valid_signature(value: &Value) -> bool {
    let signature = value.as_str().unwrap_or_default().trim_matches(js_space);
    let signature = signature
        .split_once('#')
        .map_or(signature, |(_, suffix)| suffix.trim_matches(js_space));
    if signature.len() > 32 * 1024 * 1024 {
        return false;
    }
    let decode = |value: &str| {
        STANDARD
            .decode(value)
            .or_else(|_| STANDARD_NO_PAD.decode(value))
            .ok()
    };
    match signature.as_bytes().first() {
        Some(b'E') => decode(signature).is_some_and(|bytes| bytes.first() == Some(&0x12)),
        Some(b'R') => decode(signature)
            .filter(|bytes| bytes.first() == Some(&b'E'))
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|outer| decode(&outer))
            .is_some_and(|bytes| bytes.first() == Some(&0x12)),
        _ => false,
    }
}

pub fn anchor(body: &mut Value) {
    let mut head = 0;
    for name in ["system", "tools"] {
        if let Some(blocks) = body.get_mut(name).and_then(Value::as_array_mut) {
            let last = blocks
                .iter()
                .rposition(|value| name == "system" || value["defer_loading"] != true);
            for (index, value) in blocks.iter_mut().enumerate() {
                if let Some(fields) = value.as_object_mut() {
                    if Some(index) == last {
                        fields.insert(
                            "cache_control".into(),
                            json!({"type":"ephemeral","ttl":"1h"}),
                        );
                        head += 1;
                    } else {
                        fields.shift_remove("cache_control");
                    }
                }
            }
        }
    }
    let Some(messages) = body["messages"].as_array_mut() else {
        return;
    };
    let marked = messages
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter(|block| truthy(&block["cache_control"]))
        .count();
    if marked + head >= 4 {
        let mut remove = marked + head - 4;
        for message in messages {
            for block in message["content"].as_array_mut().into_iter().flatten() {
                if remove > 0 && truthy(&block["cache_control"]) {
                    block
                        .as_object_mut()
                        .expect("cache marker object")
                        .shift_remove("cache_control");
                    remove -= 1;
                }
            }
        }
        return;
    }
    let mut assistant = false;
    for message in messages.iter_mut().rev() {
        for block in message["content"].as_array_mut().into_iter().flatten() {
            if let Some(fields) = block.as_object_mut() {
                fields.shift_remove("cache_control");
            }
        }
        if !assistant && message["role"] == "assistant" {
            assistant = mark_last(message);
        }
    }
    if !assistant {
        for message in messages.iter_mut().rev() {
            if mark_last(message) {
                break;
            }
        }
    }
    if let Some(last) = messages.last_mut().filter(|last| last["role"] == "user")
        && last["content"].as_array().is_some_and(|blocks| {
            blocks.iter().any(|block| block["type"] == "tool_result")
                && !blocks.iter().any(|block| truthy(&block["cache_control"]))
        })
    {
        mark_last(last);
    }
}

fn mark_last(message: &mut Value) -> bool {
    let Some(block) = message["content"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .rev()
        .find(|block| block.is_object() && !is_thinking(block))
    else {
        return false;
    };
    block["cache_control"] = json!({"type":"ephemeral"});
    true
}
