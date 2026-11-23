use std::collections::HashSet;

use provider_capabilities_rs::port::model_capabilities;
use serde_json::{Map, Value, json};

use crate::chat_format::ChatFormat;
use crate::chat_input::{
    PreparedChat, js_json, js_space, preprocess_tool_history, stream_requested, truthy,
};
use crate::chat_protocol_input;
use crate::claude_thinking;
use crate::openai_thinking;
use crate::thinking_intent::{model_suffix, thinking_intent};

const MAX_TOKENS: i64 = 64_000;
const MIN_TOOL_TOKENS: i64 = 32_000;
const THINKING_SIGNATURE: &str = include_str!("claude-thinking-signature.txt");
const SYSTEM_PROMPT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

pub fn prepare(client: ChatFormat, body: Value, model: &str, accept: &str) -> Option<PreparedChat> {
    prepare_for_provider(client, body, model, accept, false)
}

pub fn prepare_for_provider(
    client: ChatFormat,
    body: Value,
    model: &str,
    accept: &str,
    official: bool,
) -> Option<PreparedChat> {
    prepare_with_policy(client, body, model, accept, official, None)
}

#[derive(Clone, Copy)]
pub(super) struct RegistryMessages<'a> {
    pub provider: &'a str,
    pub thinking_format: Option<&'a str>,
    pub tool_types: Option<&'a [Value]>,
    pub drop_output_config: bool,
    pub require_tool_type: bool,
}

pub(super) fn prepare_registered(
    client: ChatFormat,
    body: Value,
    model: &str,
    accept: &str,
    registry: RegistryMessages<'_>,
) -> Option<PreparedChat> {
    prepare_with_policy(client, body, model, accept, false, Some(registry))
}

fn prepare_with_policy(
    client: ChatFormat,
    mut body: Value,
    input_model: &str,
    accept: &str,
    official: bool,
    registry: Option<RegistryMessages<'_>>,
) -> Option<PreparedChat> {
    if !body.is_object() || unsupported(&body, client == ChatFormat::Messages, true) {
        return None;
    }
    if body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .is_none_or(|message| !matches!(message["role"].as_str(), Some("user" | "tool")))
        && let Some(fields) = body.as_object_mut()
    {
        fields.shift_remove("thinking");
    }
    let (model, override_intent) = model_suffix(input_model);
    let intent = override_intent.or_else(|| thinking_intent(&body));
    let display = claude_thinking::capture_display(&body);
    let stream = stream_requested(&body, accept);
    let explicitly_prefilled = last_source_assistant(&body);
    let provider = match registry {
        Some(profile) => profile.provider,
        None if official => "anthropic",
        None => "anthropic-compatible-native",
    };
    let model_for_caps = if client == ChatFormat::Messages && registry.is_none() {
        format!("{provider}/{model}")
    } else {
        model.to_owned()
    };
    let caps = model_capabilities(Some(provider), &model_for_caps, None);
    let executor_thinking = registry
        .and_then(|profile| {
            profile
                .thinking_format
                .or_else(|| caps["thinkingFormat"].as_str())
        })
        .filter(|format| {
            matches!(
                *format,
                "deepseek" | "zai" | "minimax" | "openai" | "qwen" | "kimi"
            )
        });
    let controls = executor_thinking.and_then(|format| {
        openai_thinking::capture(&mut body, provider, input_model, Some(format), false).1
    });
    let mut prepared = match client {
        ChatFormat::Messages => {
            preflight_messages(&mut body)?;
            body
        }
        ChatFormat::Chat => {
            if truthy(&body["input"])
                || body["contents"].is_array()
                || body.get("system").is_some()
                || truthy(&body["anthropic_version"])
                || body["userAgent"] == "antigravity"
            {
                return None;
            }
            preflight_messages(&mut body)?;
            from_openai(&body, model, stream)?
        }
        ChatFormat::Responses => {
            claude_thinking::strip(&mut body);
            let chat = chat_protocol_input::translate(client, body, accept)?;
            from_openai(&chat, model, stream)?
        }
    };

    if let Some(controls) = controls {
        controls.write(&mut prepared);
    } else if executor_thinking.is_none() {
        claude_thinking::apply(&mut prepared, provider, model, &caps, intent, display);
    }
    if let Some(profile) = registry {
        if profile.drop_output_config {
            prepared.as_object_mut()?.shift_remove("output_config");
        }
        prepare_native_with_policy(
            &mut prepared,
            &caps,
            explicitly_prefilled,
            if provider == "deepseek" {
                ThinkingBlocks::DeepSeek
            } else {
                ThinkingBlocks::Preserve
            },
            profile.tool_types,
            true,
        )?;
        if profile.require_tool_type
            && let Some(tools) = prepared.get_mut("tools").and_then(Value::as_array_mut)
        {
            for tool in tools.iter_mut().filter(|tool| !truthy(&tool["type"])) {
                tool["type"] = json!("custom");
            }
        }
    } else {
        prepare_native(&mut prepared, &caps, explicitly_prefilled, !official)?;
    }
    prepared["model"] = json!(model);

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
    let map = prepared.as_object_mut()?;
    map.shift_remove("_toolNameMap");
    map.shift_remove("_customToolNames");
    if official || registry.is_some() {
        map.shift_remove("context_management");
    }
    if !stream && !outbound_stream {
        map.shift_remove("stream_options");
    }
    if lower.contains("claude") {
        map.shift_remove("temperature");
    }
    if model
        .trim_matches(js_space)
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_lowercase()
        .starts_with("deepseek-")
        && let Some(tools) = map.get_mut("tools").and_then(Value::as_array_mut)
    {
        let mut seen = HashSet::new();
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

pub(super) fn unsupported(body: &Value, native_messages: bool, inline_images: bool) -> bool {
    fn unsupported_block(item: &Value, native_messages: bool, inline_images: bool) -> bool {
        let media = match item["type"].as_str() {
            Some("image") => !inline_images || !native_image(&item["source"]),
            Some("image_url") => {
                !inline_images
                    || native_messages
                    || data_image(item["image_url"]["url"].as_str()).is_none()
            }
            Some("input_image") => {
                !inline_images
                    || native_messages
                    || data_image(item["image_url"].as_str()).is_none()
            }
            _ => false,
        };
        media
            || matches!(
                item["type"].as_str(),
                Some(
                    "input_audio"
                        | "audio_url"
                        | "document"
                        | "file"
                        | "input_file"
                        | "audio"
                        | "container_upload"
                        | "reasoning"
                        | "server_tool_use"
                        | "web_search_tool_result"
                )
            )
            || (!native_messages && is_thinking(item))
            || ["images", "attachments", "experimental_attachments"]
                .iter()
                .any(|key| item[*key].is_array())
            || match &item["content"] {
                Value::Array(parts) => parts
                    .iter()
                    .any(|part| unsupported_block(part, native_messages, inline_images)),
                block if block.is_object() => {
                    unsupported_block(block, native_messages, inline_images)
                }
                _ => false,
            }
    }
    ["messages", "input", "system"].iter().any(|key| {
        body[*key].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| unsupported_block(item, native_messages, inline_images))
        })
    })
}

pub(super) fn last_source_assistant(body: &Value) -> bool {
    if let Some(messages) = body["messages"].as_array() {
        return messages
            .last()
            .is_some_and(|message| message["role"] == "assistant");
    }
    body["contents"]
        .as_array()
        .or_else(|| body["input"].as_array())
        .and_then(|items| items.last())
        .is_some_and(|item| matches!(item["role"].as_str(), Some("assistant" | "model")))
}

pub(super) fn preflight_messages(body: &mut Value) -> Option<()> {
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        preprocess_tool_history(messages)?;
    }
    Some(())
}

pub(super) fn from_openai(body: &Value, model: &str, stream: bool) -> Option<Value> {
    let caps = model_capabilities(None, model, None);
    let ceiling = caps["maxOutput"]
        .as_i64()
        .filter(|limit| *limit != 0)
        .unwrap_or(MAX_TOKENS);
    let limit = if truthy(&body["max_tokens"]) {
        body["max_tokens"].clone()
    } else {
        json!(MAX_TOKENS)
    };
    let floor = body["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty());
    let tokens = numeric(&limit)?;
    let limit = if (floor && tokens < MIN_TOOL_TOKENS as f64) && MIN_TOOL_TOKENS > ceiling
        || tokens > ceiling as f64
    {
        json!(ceiling)
    } else if floor && tokens < MIN_TOOL_TOKENS as f64 {
        json!(MIN_TOOL_TOKENS)
    } else {
        limit
    };
    let mut result = json!({ "model": model, "max_tokens": limit, "stream": stream });
    if let Some(temperature) = body.get("temperature") {
        result["temperature"] = temperature.clone();
    }

    let mut system = Vec::new();
    let mut messages = Vec::new();
    let mut current_role = None;
    let mut current_parts = Vec::new();
    if let Some(items) = body["messages"].as_array() {
        for item in items {
            item.as_object()?;
            if item["role"] == "system" {
                system.push(openai_text(&item["content"])?);
                continue;
            }
            let role = if matches!(item["role"].as_str(), Some("user" | "tool")) {
                "user"
            } else {
                "assistant"
            };
            let mut blocks = openai_blocks(item)?;
            if blocks.iter().any(|block| block["type"] == "tool_result") {
                let (results, others): (Vec<_>, Vec<_>) = blocks
                    .into_iter()
                    .partition(|block| block["type"] == "tool_result");
                flush(&mut messages, current_role, &mut current_parts);
                messages.push(json!({ "role": "user", "content": results }));
                if !others.is_empty() {
                    current_role = Some(role);
                    current_parts.extend(others);
                }
                continue;
            }
            if current_role != Some(role) {
                flush(&mut messages, current_role, &mut current_parts);
                current_role = Some(role);
            }
            let has_tool_use = blocks.iter().any(|block| block["type"] == "tool_use");
            current_parts.append(&mut blocks);
            if has_tool_use {
                flush(&mut messages, current_role, &mut current_parts);
            }
        }
    }
    flush(&mut messages, current_role, &mut current_parts);
    result["messages"] = Value::Array(messages);

    let response_format = &body["response_format"];
    let schema = &response_format["json_schema"]["schema"];
    if response_format["type"] == "json_schema" && truthy(schema) {
        let schema = js_json(schema, true)?;
        system.push(format!("You must respond with valid JSON that strictly follows this JSON schema:\n```json\n{schema}\n```\nRespond ONLY with the JSON object, no other text."));
    } else if response_format["type"] == "json_object" {
        system.push(
            "You must respond with valid JSON. Respond ONLY with a JSON object, no other text."
                .into(),
        );
    }
    let mut blocks = vec![text_block(&json!(SYSTEM_PROMPT))];
    if !system.is_empty() {
        blocks.push(text_block(&json!(system.join("\n"))));
    }
    result["system"] = Value::Array(blocks);

    if let Some(tools) = body["tools"].as_array() {
        let mut output = Vec::new();
        for tool in tools {
            tool.as_object()?;
            if truthy(&tool["type"]) && tool["type"] != "function" {
                output.push(tool.clone());
                continue;
            }
            let data = tool
                .get("function")
                .filter(|value| !value.is_null())
                .unwrap_or(tool);
            let name = data["name"].as_str()?;
            let description = if truthy(&data["description"]) {
                data["description"].clone()
            } else {
                json!("")
            };
            let input_schema = [&data["parameters"], &data["input_schema"]]
                .into_iter()
                .find(|value| truthy(value))
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {}, "required": [] }));
            output.push(
                json!({ "name": name, "description": description, "input_schema": input_schema }),
            );
        }
        result["tools"] = Value::Array(output);
    }
    let choice = &body["tool_choice"];
    if truthy(choice) {
        result["tool_choice"] = if choice == "required" {
            json!({ "type": "any" })
        } else if truthy(&choice["function"]["name"]) {
            json!({ "type": "tool", "name": choice["function"]["name"] })
        } else if matches!(
            choice["type"].as_str(),
            Some("auto" | "any" | "tool" | "none")
        ) {
            choice.clone()
        } else {
            json!({ "type": "auto" })
        };
    }
    Some(result)
}

fn flush(messages: &mut Vec<Value>, role: Option<&str>, parts: &mut Vec<Value>) {
    if let Some(role) = role.filter(|_| !parts.is_empty()) {
        messages.push(json!({ "role": role, "content": std::mem::take(parts) }));
    }
}

fn openai_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let mut lines = Vec::new();
    if let Some(parts) = content.as_array() {
        for part in parts.iter().filter(|part| part["type"] == "text") {
            let text = if part["text"].is_null() {
                ""
            } else {
                part["text"].as_str()?
            };
            lines.push(text);
        }
    }
    Some(lines.join("\n"))
}

fn openai_blocks(message: &Value) -> Option<Vec<Value>> {
    let mut blocks = Vec::new();
    let content = &message["content"];
    match message["role"].as_str() {
        Some("tool") => {
            let mut converted = message.clone();
            if let Some(parts) = content.as_array() {
                let content: Vec<_> = parts
                    .iter()
                    .filter_map(|part| {
                        if part["type"] == "text" && truthy(&part["text"]) {
                            Some(text_block(&part["text"]))
                        } else if part["type"] == "image_url" {
                            data_image(part["image_url"]["url"].as_str())
                        } else {
                            None
                        }
                    })
                    .collect();
                if !content.is_empty() {
                    converted["content"] = Value::Array(content);
                }
            }
            blocks.push(tool_result(&converted, "tool_call_id"));
        }
        Some("assistant" | "user") => {
            let assistant = message["role"] == "assistant";
            if let Some(parts) = content.as_array() {
                for part in parts {
                    if part["type"] == "text" && truthy(&part["text"]) {
                        blocks.push(text_block(&part["text"]));
                    } else if !assistant && part["type"] == "image_url" {
                        blocks.push(data_image(part["image_url"]["url"].as_str())?);
                    } else if !assistant && part["type"] == "image" {
                        blocks.push(json!({"type":"image","source":part["source"]}));
                    } else if assistant && part["type"] == "tool_use" {
                        let mut block = Map::from_iter([("type".into(), json!("tool_use"))]);
                        for key in ["id", "name", "input"] {
                            copy(&mut block, key, part, key);
                        }
                        blocks.push(Value::Object(block));
                    } else if !assistant && part["type"] == "tool_result" {
                        blocks.push(tool_result(part, "tool_use_id"));
                    }
                }
            } else if let Some(text) = content.as_str().filter(|text| !text.is_empty()) {
                blocks.push(text_block(&json!(text)));
            }
            if assistant && let Some(calls) = message["tool_calls"].as_array() {
                for call in calls.iter().filter(|call| call["type"] == "function") {
                    let function = &call["function"];
                    let name = function["name"].as_str()?;
                    let mut block = Map::from_iter([("type".into(), json!("tool_use"))]);
                    copy(&mut block, "id", call, "id");
                    block.insert("name".into(), json!(name));
                    if let Some(arguments) = function.get("arguments") {
                        let value = arguments
                            .as_str()
                            .and_then(|text| serde_json::from_str::<Value>(text).ok())
                            .unwrap_or_else(|| arguments.clone());
                        block.insert("input".into(), value);
                    }
                    blocks.push(Value::Object(block));
                }
            }
        }
        _ => (),
    }
    Some(blocks)
}

fn data_image(url: Option<&str>) -> Option<Value> {
    let (mime, data) = url?.strip_prefix("data:")?.split_once(";base64,")?;
    if !mime.starts_with("image/") || mime.len() <= 6 || data.is_empty() {
        return None;
    }
    Some(json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}))
}

fn native_image(source: &Value) -> bool {
    source["type"] == "base64"
        && source["media_type"]
            .as_str()
            .is_some_and(|mime| mime.starts_with("image/") && mime.len() > 6)
        && source["data"].as_str().is_some_and(|data| !data.is_empty())
}

fn tool_result(source: &Value, id: &str) -> Value {
    let mut block = Map::from_iter([("type".into(), json!("tool_result"))]);
    copy(&mut block, "tool_use_id", source, id);
    copy(&mut block, "content", source, "content");
    block.insert(
        "is_error".into(),
        json!(source["is_error"] == true || source["status"] == "error"),
    );
    Value::Object(block)
}

pub(super) fn prepare_native(
    body: &mut Value,
    caps: &Value,
    explicitly_prefilled: bool,
    compatible: bool,
) -> Option<()> {
    let thinking = if compatible {
        ThinkingBlocks::Signed
    } else {
        ThinkingBlocks::Preserve
    };
    prepare_native_with_policy(body, caps, explicitly_prefilled, thinking, None, false)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ThinkingBlocks {
    Signed,
    DeepSeek,
    Preserve,
}

fn prepare_native_with_policy(
    body: &mut Value,
    caps: &Value,
    explicitly_prefilled: bool,
    thinking_blocks: ThinkingBlocks,
    tool_types: Option<&[Value]>,
    hoist_images: bool,
) -> Option<()> {
    let forced = matches!(body["tool_choice"]["type"].as_str(), Some("any" | "tool"));
    if caps["forcedToolChoice"] == false && forced {
        let mut choice = Map::from_iter([("type".into(), json!("auto"))]);
        copy(
            &mut choice,
            "disable_parallel_tool_use",
            &body["tool_choice"],
            "disable_parallel_tool_use",
        );
        body["tool_choice"] = Value::Object(choice);
    }
    if truthy(&body["max_tokens"]) {
        let ceiling = caps["maxOutput"]
            .as_i64()
            .filter(|limit| *limit != 0)
            .unwrap_or(MAX_TOKENS);
        if numeric(&body["max_tokens"])? > ceiling as f64 {
            body["max_tokens"] = json!(ceiling);
        }
    }

    if let Some(system) = body.get_mut("system").and_then(Value::as_array_mut) {
        let last = system.len().checked_sub(1);
        for (index, block) in system.iter_mut().enumerate() {
            block.as_object_mut()?.shift_remove("cache_control");
            if Some(index) == last {
                block["cache_control"] = hour_cache();
            }
        }
    }
    let thinking_enabled = body["thinking"]["type"] == "enabled"
        || thinking_blocks == ThinkingBlocks::DeepSeek && body["thinking"]["type"] == "adaptive";
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        let final_assistant = messages.last().is_some_and(|m| m["role"] == "assistant");
        let len = messages.len();
        let mut filtered = Vec::new();
        for (index, mut message) in std::mem::take(messages).into_iter().enumerate() {
            if message["content"].is_object() {
                message["content"] = json!([std::mem::take(&mut message["content"])]);
            }
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                for part in parts {
                    part.as_object_mut()?.shift_remove("cache_control");
                }
            }
            if (index + 1 == len && final_assistant) || has_content(&message["content"]) {
                filtered.push(message);
            }
        }
        let mut filtered = fix_ordering(filtered)?;
        ensure_user(&mut filtered, final_assistant);
        let thinking_enabled = thinking_enabled
            && filtered
                .last()
                .is_some_and(|message| message["role"] == "user");
        let mut cache = true;
        for message in filtered
            .iter_mut()
            .rev()
            .filter(|message| message["role"] == "assistant")
        {
            let Some(parts) = message
                .get_mut("content")
                .and_then(Value::as_array_mut)
                .filter(|parts| !parts.is_empty())
            else {
                continue;
            };
            if cache {
                cache = false;
                if let Some(block) = parts.iter_mut().rev().find(|block| !is_thinking(block)) {
                    block["cache_control"] = short_cache();
                }
            }
            if thinking_blocks != ThinkingBlocks::Preserve {
                let mut has_thinking = false;
                let mut has_tool = false;
                parts.retain_mut(|block| {
                    if !is_thinking(block) {
                        has_tool |= block["type"] == "tool_use";
                        return true;
                    }
                    if !block["thinking"]
                        .as_str()
                        .is_some_and(|text| !text.trim_matches(js_space).is_empty())
                    {
                        return false;
                    }
                    if thinking_blocks == ThinkingBlocks::Signed {
                        block["signature"] = json!(THINKING_SIGNATURE);
                    }
                    has_thinking = true;
                    true
                });
                if thinking_enabled && has_tool && !has_thinking {
                    let mut placeholder = json!({"type":"thinking","thinking":"."});
                    if thinking_blocks == ThinkingBlocks::Signed {
                        placeholder["signature"] = json!(THINKING_SIGNATURE);
                    }
                    parts.insert(0, placeholder);
                }
            }
        }
        *messages = filtered;
    }

    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        let mut filtered = Vec::new();
        for mut tool in std::mem::take(tools) {
            if truthy(&tool["type"])
                && tool["type"] != "function"
                && tool_types.is_none_or(|types| !types.contains(&tool["type"]))
            {
                continue;
            }
            if truthy(&tool["function"]) {
                let function = &tool["function"];
                let mut normalized = Map::new();
                copy(&mut normalized, "name", function, "name");
                copy(&mut normalized, "description", function, "description");
                copy(&mut normalized, "input_schema", function, "parameters");
                tool = Value::Object(normalized);
            } else if tool_types.is_none() {
                tool.as_object_mut()?.shift_remove("type");
            }
            tool.as_object_mut()?.shift_remove("cache_control");
            filtered.push(tool);
        }
        if let Some(last) = filtered
            .iter_mut()
            .rev()
            .find(|tool| tool["defer_loading"] != true)
        {
            last["cache_control"] = hour_cache();
        }
        *tools = filtered;
        if tools.is_empty() {
            body.as_object_mut()?.shift_remove("tools");
            body.as_object_mut()?.shift_remove("tool_choice");
        }
    }
    if hoist_images {
        hoist_tool_result_images(body);
    }
    if marks(body) < 4
        && let Some(last) = body
            .get_mut("messages")
            .and_then(Value::as_array_mut)
            .and_then(|messages| messages.last_mut())
        && last["role"] == "user"
        && let Some(parts) = last.get_mut("content").and_then(Value::as_array_mut)
        && parts.iter().any(|block| block["type"] == "tool_result")
        && !parts.iter().any(|block| truthy(&block["cache_control"]))
        && let Some(block) = parts.last_mut()
    {
        block["cache_control"] = short_cache();
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        ensure_user(messages, explicitly_prefilled);
    }
    Some(())
}

fn hoist_tool_result_images(body: &mut Value) {
    for message in body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "user")
    {
        let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut hoisted = Vec::new();
        for result in parts
            .iter_mut()
            .filter(|part| part["type"] == "tool_result")
        {
            let id = result["tool_use_id"]
                .as_str()
                .unwrap_or("undefined")
                .to_owned();
            let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            if !content.iter().any(|item| item["type"] == "image") {
                continue;
            }
            let (images, rest): (Vec<_>, Vec<_>) = std::mem::take(content)
                .into_iter()
                .partition(|item| item["type"] == "image");
            *content = if rest.is_empty() {
                vec![json!({"type":"text","text":"(image attached below)"})]
            } else {
                rest
            };
            hoisted.push(json!({"type":"text","text":format!("[Image from tool result {id}]")}));
            hoisted.extend(images);
        }
        parts.extend(hoisted);
    }
}

fn is_thinking(block: &Value) -> bool {
    matches!(
        block["type"].as_str(),
        Some("thinking" | "redacted_thinking")
    )
}

fn fix_ordering(messages: Vec<Value>) -> Option<Vec<Value>> {
    if messages.len() <= 1 {
        return Some(messages);
    }

    let mut merged: Vec<Value> = Vec::new();
    for mut message in messages {
        let role = message.get("role").cloned();
        let content = message.as_object_mut()?.shift_remove("content");
        let mut parts = match content {
            Some(Value::Array(parts)) => parts,
            Some(content) => vec![text_block(&content)],
            None => vec![json!({ "type": "text" })],
        };
        if role.as_ref().is_some_and(|role| role == "assistant") {
            let mut found_tool = false;
            parts.retain(|part| {
                if part["type"] == "tool_use" {
                    found_tool = true;
                    true
                } else {
                    !found_tool
                }
            });
        }

        if let Some(last) = merged
            .last_mut()
            .filter(|last| last.get("role") == role.as_ref())
        {
            let existing = last["content"].as_array_mut()?;
            existing.append(&mut parts);
            let (mut tool_results, ordinary): (Vec<_>, Vec<_>) = std::mem::take(existing)
                .into_iter()
                .partition(|block| block["type"] == "tool_result");
            tool_results.extend(ordinary);
            *existing = tool_results;
        } else {
            let mut merged_part = Map::new();
            if let Some(role) = role {
                merged_part.insert("role".into(), role);
            }
            merged_part.insert("content".into(), Value::Array(parts));
            merged.push(Value::Object(merged_part));
        }
    }
    Some(merged)
}

fn has_content(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| !text.trim_matches(js_space).is_empty())
        || value.as_array().is_some_and(|blocks| {
            blocks.iter().any(|block| {
                matches!(
                    block["type"].as_str(),
                    Some("tool_use" | "tool_result" | "image")
                ) || (block["type"] == "text"
                    && block["text"]
                        .as_str()
                        .is_some_and(|text| !text.trim_matches(js_space).is_empty()))
            })
        })
}

fn ensure_user(messages: &mut Vec<Value>, explicitly_prefilled: bool) {
    if !explicitly_prefilled
        && messages
            .last()
            .is_some_and(|message| message["role"] == "assistant")
    {
        messages.push(json!({ "role": "user", "content": [text_block(&json!("Continue."))] }));
    }
}

fn marks(body: &Value) -> usize {
    let head = ["system", "tools"]
        .iter()
        .map(|key| {
            body[*key]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|part| truthy(&part["cache_control"]))
                .count()
        })
        .sum::<usize>();
    head + body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|message| {
            message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|part| truthy(&part["cache_control"]))
                .count()
        })
        .sum::<usize>()
}

fn copy(output: &mut Map<String, Value>, field: &str, input: &Value, key: &str) {
    if let Some(value) = input.get(key) {
        output.insert(field.into(), value.clone());
    }
}

fn text_block(text: &Value) -> Value {
    json!({ "type": "text", "text": text })
}

fn short_cache() -> Value {
    json!({ "type": "ephemeral" })
}

fn hour_cache() -> Value {
    json!({ "type": "ephemeral", "ttl": "1h" })
}

fn numeric(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| {
        if let Some(flag) = value.as_bool() {
            return Some(if flag { 1.0 } else { 0.0 });
        }
        let text = value.as_str()?.trim_matches(js_space);
        if text.is_empty() {
            Some(0.0)
        } else {
            text.parse().ok()
        }
    })
}
