use serde_json::{Map, Value, json};

use crate::chat_format::ChatFormat;
use crate::chat_input::{
    PreparedChat, js_json, js_space, preprocess_tool_history, stream_requested, truthy,
};
use crate::chat_protocol_input;

pub fn prepare(client: ChatFormat, body: Value, model: &str, accept: &str) -> Option<PreparedChat> {
    if !body.is_object() || has_thinking(&body, model) || has_media(&body) {
        return None;
    }
    let stream = stream_requested(&body, accept);
    let mut prepared = match client {
        ChatFormat::Responses => native(body, model, false)?,
        ChatFormat::Messages => {
            let chat = chat_protocol_input::translate(client, body, accept)?;
            from_chat(chat, model)?
        }
        ChatFormat::Chat => {
            if !body["input"].is_array()
                && (body.get("system").is_some()
                    || truthy(&body["anthropic_version"])
                    || body["contents"].is_array()
                    || body["userAgent"] == "antigravity")
            {
                return None;
            }
            if truthy(&body["input"]) {
                if !body["input"].is_array() && !body["input"].is_string() {
                    return None;
                }
                let forced = body["input"].is_array() || truthy(&body["messages"]);
                native(body, model, forced)?
            } else {
                let mut body = body;
                preprocess_messages(&mut body)?;
                from_chat(body, model)?
            }
        }
    };
    if prepared["response_format"]["type"] == "json_schema"
        && truthy(&prepared["response_format"]["json_schema"]["schema"])
    {
        return None;
    }
    let lower = model.to_ascii_lowercase();
    if let Some(messages) = prepared.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            let object = message.as_object_mut()?;
            object.shift_remove("encrypted_content");
            object.shift_remove("reasoning_encrypted_content");
            if message["role"] == "assistant"
                && (lower.contains("deepseek")
                    || (lower.starts_with("kimi-")
                        && message["tool_calls"]
                            .as_array()
                            .is_some_and(|v| !v.is_empty())))
                && message["reasoning_content"]
                    .as_str()
                    .is_none_or(str::is_empty)
            {
                message["reasoning_content"] = json!(" ");
            }
        }
    }
    let outbound_stream = prepared["stream"] == true;
    let object = prepared.as_object_mut()?;
    object.shift_remove("_toolNameMap");
    object.shift_remove("_customToolNames");
    if !stream && !outbound_stream {
        object.shift_remove("stream_options");
    }
    if lower.contains("claude") {
        object.shift_remove("temperature");
    }
    if lower
        .rsplit('/')
        .next()
        .unwrap_or(&lower)
        .starts_with("deepseek-")
        && let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut)
    {
        let mut seen = std::collections::HashSet::new();
        tools.retain(|tool| {
            tool["name"]
                .as_str()
                .or_else(|| tool["function"]["name"].as_str())
                .is_none_or(|name| name.is_empty() || seen.insert(name.to_owned()))
        });
    }
    Some(PreparedChat {
        body: prepared,
        stream,
    })
}

fn native(mut body: Value, model: &str, forced: bool) -> Option<Value> {
    preprocess_messages(&mut body)?;
    let object = body.as_object_mut()?;
    object.insert("model".into(), json!(model));
    if forced {
        object.insert("stream".into(), json!(true));
        if !object.contains_key("max_output_tokens")
            && let Some(limit) = object
                .get("max_completion_tokens")
                .or_else(|| object.get("max_tokens"))
        {
            object.insert("max_output_tokens".into(), limit.clone());
        }
        object.shift_remove("max_tokens");
        object.shift_remove("max_completion_tokens");
    }
    Some(body)
}

fn preprocess_messages(body: &mut Value) -> Option<()> {
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        preprocess_tool_history(messages)?;
    }
    Some(())
}

fn has_thinking(body: &Value, model: &str) -> bool {
    model
        .trim_end_matches(js_space)
        .strip_suffix(')')
        .and_then(|base| base.rsplit_once('('))
        .is_some_and(|(_, level)| !level.is_empty() && !level.contains(')'))
        || [
            "thinking",
            "reasoning_effort",
            "reasoning",
            "thinkingConfig",
            "enable_thinking",
            "thinking_budget",
            "output_config",
        ]
        .iter()
        .any(|key| body.get(*key).is_some())
        || body["generationConfig"].get("thinkingConfig").is_some()
        || body["request"]["generationConfig"]
            .get("thinkingConfig")
            .is_some()
        || body["params"].get("thinking").is_some()
        || body["params"].get("reasoning_effort").is_some()
}

fn has_media(body: &Value) -> bool {
    fn needs_modality(value: &Value) -> bool {
        matches!(
            value["type"].as_str(),
            Some("input_audio" | "audio_url" | "document" | "file" | "input_file" | "audio")
        ) || ["images", "attachments", "experimental_attachments"]
            .iter()
            .any(|key| value[*key].is_array())
            || match &value["content"] {
                Value::Array(parts) => parts.iter().any(needs_modality),
                value if value.is_object() => needs_modality(value),
                _ => false,
            }
    }
    ["messages", "input"].iter().any(|key| {
        body[*key]
            .as_array()
            .is_some_and(|items| items.iter().any(needs_modality))
    })
}

pub(super) fn from_chat(mut body: Value, model: &str) -> Option<Value> {
    let mut input = Vec::new();
    let mut result = Map::from_iter([
        ("model".into(), json!(model)),
        ("input".into(), json!([])),
        ("stream".into(), json!(true)),
        ("store".into(), json!(false)),
    ]);

    if let Some(messages) = body.as_object_mut()?.shift_remove("messages") {
        for message in messages.as_array()? {
            message.as_object()?;
            match message["role"].as_str() {
                Some("system" | "developer") => {
                    if !result.contains_key("instructions") {
                        result.insert(
                            "instructions".into(),
                            json!(instructions(&message["content"])),
                        );
                    }
                }
                Some("user" | "assistant") => {
                    let assistant = message["role"] == "assistant";
                    if assistant && let Some(reasoning) = reasoning_item(message) {
                        input.push(reasoning);
                    }
                    let role = if assistant { "assistant" } else { "user" };
                    let content = input_contents(&message["content"], assistant)?;
                    if !content.is_empty() {
                        input.push(json!({ "type": "message", "role": role, "content": content }));
                    }
                    if assistant && truthy(&message["tool_calls"]) {
                        for call in message["tool_calls"].as_array()? {
                            let Some(name) = call["function"]["name"]
                                .as_str()
                                .map(|name| name.trim_matches(js_space))
                                .filter(|name| !name.is_empty())
                            else {
                                continue;
                            };
                            let id = truncate_utf16(call["id"].as_str()?, 64);
                            if id.is_empty() {
                                return None;
                            }
                            let name = truncate_utf16(name, 128);
                            let arguments = tool_arguments(&call["function"]["arguments"]);
                            input.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": arguments,
                            }));
                        }
                    }
                }
                Some("tool") => {
                    let id = truncate_utf16(message["tool_call_id"].as_str()?, 64);
                    if id.is_empty() {
                        return None;
                    }
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": id,
                        "output": tool_output(&message["content"]),
                    }));
                }
                _ => (),
            }
        }
    }
    if !result.contains_key("instructions") {
        result.insert("instructions".into(), json!(""));
    }
    result.insert("input".into(), Value::Array(input));

    if let Some(tools) = body.as_object_mut()?.shift_remove("tools")
        && let Value::Array(tools) = tools
    {
        let mut declarations = Vec::new();
        for tool in tools {
            if tool.is_null() {
                return None;
            }
            if tool["type"] != "function" {
                if truthy(&tool) {
                    declarations.push(tool);
                }
                continue;
            }
            let function = &tool["function"];
            let Some(name) = function["name"]
                .as_str()
                .map(|name| name.trim_matches(js_space))
                .filter(|name| !name.is_empty())
            else {
                continue;
            };
            let name = truncate_utf16(name, 128);
            let description = if truthy(&function["description"]) {
                function["description"].as_str()?
            } else {
                ""
            };
            let mut parameters = if truthy(&function["parameters"]) {
                function["parameters"].clone()
            } else {
                json!({"type": "object", "properties": {}})
            };
            if parameters["type"] == "object" && !truthy(&parameters["properties"]) {
                parameters["properties"] = json!({});
            }
            let mut declaration = json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
            });
            if let Some(strict) = function.get("strict") {
                declaration["strict"] = strict.clone();
            }
            declarations.push(declaration);
        }
        result.insert("tools".into(), Value::Array(declarations));
    }

    if let Some(value) = body.get("temperature") {
        result.insert("temperature".into(), value.clone());
    }
    if let Some(limit) = body
        .get("max_output_tokens")
        .or_else(|| body.get("max_completion_tokens"))
        .or_else(|| body.get("max_tokens"))
    {
        result.insert("max_output_tokens".into(), limit.clone());
    }
    for key in ["top_p", "service_tier", "prompt_cache_key"] {
        if let Some(value) = body.as_object_mut()?.shift_remove(key) {
            result.insert(key.into(), value);
        }
    }
    Some(Value::Object(result))
}

fn instructions(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| {
            part["text"]
                .as_str()
                .or_else(|| part["content"].as_str())
                .filter(|text| !text.is_empty())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn reasoning_item(message: &Value) -> Option<Value> {
    let encrypted = [
        &message["encrypted_content"],
        &message["reasoning_encrypted_content"],
        &message["reasoning"]["encrypted_content"],
    ]
    .into_iter()
    .find_map(|value| value.as_str().filter(|text| !text.is_empty()));
    let summary = [&message["reasoning_content"], &message["reasoning"]]
        .into_iter()
        .find_map(|value| {
            value
                .as_str()
                .filter(|text| !text.trim_matches(js_space).is_empty())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| {
            message["reasoning_details"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|detail| {
                    detail["text"]
                        .as_str()
                        .or_else(|| detail["content"].as_str())
                        .filter(|text| !text.is_empty())
                })
                .collect::<Vec<_>>()
                .join("\n")
        });
    if encrypted.is_none() && summary.is_empty() {
        return None;
    }
    let mut item = json!({ "type": "reasoning" });
    if !summary.is_empty() {
        item["summary"] = json!([{ "type": "summary_text", "text": summary }]);
    }
    if let Some(encrypted) = encrypted {
        item["encrypted_content"] = json!(encrypted);
    }
    Some(item)
}

fn input_contents(content: &Value, assistant: bool) -> Option<Vec<Value>> {
    let kind = if assistant {
        "output_text"
    } else {
        "input_text"
    };
    if let Some(text) = content.as_str() {
        return Some(vec![json!({ "type": kind, "text": text })]);
    }
    let Some(parts) = content.as_array() else {
        return Some(Vec::new());
    };
    parts
        .iter()
        .map(|part| {
            if part.is_null() {
                return None;
            }
            if part["type"] == "text" {
                let mut result = json!({"type": kind});
                if let Some(text) = part.get("text") {
                    result["text"] = text.clone();
                }
                return Some(result);
            }
            if part["type"] == "image_url" {
                let image = &part["image_url"];
                let url = image["url"].as_str()?;
                let detail = if truthy(&image["detail"]) {
                    image["detail"].clone()
                } else {
                    json!("auto")
                };
                return Some(json!({"type": "input_image", "image_url": url, "detail": detail}));
            }
            let value = [&part["text"], &part["content"]]
                .into_iter()
                .find(|value| truthy(value))
                .unwrap_or(part);
            let text = value
                .as_str()
                .map(str::to_owned)
                .or_else(|| js_json(value, false))?;
            Some(json!({ "type": kind, "text": text }))
        })
        .collect()
}

pub(super) fn truncate_utf16(text: &str, limit: usize) -> &str {
    let mut used = 0;
    for (index, ch) in text.char_indices() {
        used += ch.len_utf16();
        if used > limit {
            return &text[..index];
        }
    }
    text
}

pub(super) fn tool_arguments(value: &Value) -> String {
    if value.is_null() {
        return "{}".into();
    }
    if let Some(text) = value.as_str() {
        return if serde_json::from_str::<Value>(text).is_ok() {
            text.to_owned()
        } else {
            "{}".into()
        };
    }
    js_json(value, false).unwrap_or_else(|| value.to_string())
}

pub(super) fn tool_output(value: &Value) -> String {
    match value {
        Value::String(text) => text.to_owned(),
        Value::Null => String::new(),
        Value::Array(items) => {
            let mut text = String::new();
            for item in items {
                let piece = item
                    .get("text")
                    .filter(|value| !value.is_null())
                    .map_or_else(
                        || js_json(item, false).unwrap_or_else(|| item.to_string()),
                        crate::chat_output::js_text,
                    );
                text.push_str(&piece);
            }
            text
        }
        value => js_json(value, false).unwrap_or_else(|| value.to_string()),
    }
}
