use serde_json::{Value, json};

use crate::kimchi_models;
use crate::storage::Storage;

pub(crate) fn supports(account: &Value) -> bool {
    matches!(account["authType"].as_str(), Some("oauth" | "apikey")) && token(account).is_some()
}

fn token(account: &Value) -> Option<&str> {
    [&account["apiKey"], &account["accessToken"]]
        .into_iter()
        .find_map(|value| value.as_str().filter(|value| !value.is_empty()))
        .filter(|value| {
            value.len() <= 16 * 1024 && value.bytes().all(|byte| byte.is_ascii_graphic())
        })
}

pub(crate) fn prepare(storage: &Storage, account: &Value, model: &str, body: &mut Value) {
    let Some(fields) = body.as_object_mut() else {
        return;
    };
    let system = fields
        .shift_remove("system")
        .map(system_text)
        .unwrap_or_default();
    for field in [
        "anthropic_version",
        "anthropic_beta",
        "client_metadata",
        "mcp_servers",
        "stop_sequences",
        "thinking",
        "top_k",
    ] {
        fields.shift_remove(field);
    }
    if model
        .split(['-', '_', '/'])
        .any(|part| part.eq_ignore_ascii_case("claude") || part.eq_ignore_ascii_case("anthropic"))
        || token(account)
            .is_some_and(|token| kimchi_models::cached_anthropic(storage, account, token, model))
    {
        fields.shift_remove("reasoning_effort");
        fields.shift_remove("reasoning");
    }
    if let Some(messages) = fields.get_mut("messages").and_then(Value::as_array_mut) {
        if !system.is_empty() {
            merge_system(messages, system);
        }
        for message in messages {
            let Some(message) = message.as_object_mut() else {
                continue;
            };
            message.shift_remove("cache_control");
            if message.get("role").is_some_and(|role| role == "assistant")
                && message
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .is_some_and(|reasoning| reasoning.encode_utf16().take(9).count() > 8)
            {
                message.shift_remove("reasoning_content");
            }
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                for part in parts.iter_mut().filter_map(Value::as_object_mut) {
                    part.shift_remove("cache_control");
                    part.shift_remove("signature");
                }
            }
        }
    }
    if let Some(tools) = fields.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
            tool.shift_remove("cache_control");
        }
    }
}

fn system_text(system: Value) -> String {
    match system {
        Value::String(text) => text,
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.as_str().or_else(|| part["text"].as_str()))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn merge_system(messages: &mut Vec<Value>, system: String) {
    if let Some(existing) = messages
        .iter_mut()
        .find(|message| message["role"] == "system")
    {
        match existing.get_mut("content") {
            Some(Value::String(text)) => {
                *text = format!("{system}\n\n{text}");
            }
            Some(Value::Array(parts)) => parts.insert(0, json!({"type":"text","text":system})),
            _ => (),
        }
    } else {
        messages.insert(0, json!({"role":"system","content":system}));
    }
}

pub(crate) fn redact_error(account: &Value, message: &str) -> String {
    ["apiKey", "accessToken", "refreshToken", "idToken"]
        .into_iter()
        .flat_map(|field| [&account[field], &account["providerSpecificData"][field]])
        .filter_map(Value::as_str)
        .filter(|secret| !secret.is_empty())
        .fold(message.to_owned(), |text, secret| {
            text.replace(secret, "[redacted]")
        })
}
