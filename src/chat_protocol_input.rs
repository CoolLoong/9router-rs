use std::collections::HashSet;

use serde_json::{Map, Value, json};

use crate::chat_format::ChatFormat;
use crate::chat_input::{js_json, js_space, preprocess_tool_history, stream_requested, truthy};

const THINKING_FIELDS: &[&str] = &[
    "thinking",
    "reasoning_effort",
    "reasoning",
    "enable_thinking",
    "thinking_budget",
    "output_config",
    "thinkingConfig",
];

pub fn translate(format: ChatFormat, body: Value, accept: &str) -> Option<Value> {
    match format {
        ChatFormat::Chat => Some(body),
        ChatFormat::Messages => anthropic_messages(body, accept, false),
        ChatFormat::Responses if unsupported_thinking(&body) => None,
        ChatFormat::Responses => openai_responses(body, false, false),
    }
}

pub(crate) fn chatgpt_web(
    format: ChatFormat,
    body: Value,
    accept: &str,
    preview: bool,
) -> Option<Value> {
    match format {
        ChatFormat::Chat => Some(body),
        ChatFormat::Messages => convert_anthropic_messages(body, accept, false, false, true),
        ChatFormat::Responses if !preview && unsupported_thinking(&body) => None,
        ChatFormat::Responses => openai_responses(body, preview, true),
    }
}

pub(super) fn unsupported_thinking(body: &Value) -> bool {
    THINKING_FIELDS.iter().any(|key| body.get(*key).is_some())
        || body["generationConfig"].get("thinkingConfig").is_some()
        || body["request"]["generationConfig"]
            .get("thinkingConfig")
            .is_some()
}

pub(super) fn for_kiro(format: ChatFormat, mut body: Value, accept: &str) -> Option<Value> {
    let mut thinking = Map::new();
    if format != ChatFormat::Chat {
        let fields = body.as_object_mut()?;
        for &name in THINKING_FIELDS {
            if let Some(value) = fields.shift_remove(name) {
                thinking.insert(name.to_owned(), value);
            }
        }
    }
    if format == ChatFormat::Messages {
        for message in body
            .get_mut("messages")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if message["role"] == "assistant"
                && let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut)
            {
                parts.retain(|part| {
                    !matches!(
                        part["type"].as_str(),
                        Some("thinking" | "redacted_thinking")
                    )
                });
            }
        }
    }
    let mut translated = translate(format, body, accept)?;
    translated.as_object_mut()?.extend(thinking);
    for message in translated
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if message["role"] == "developer" {
            message["role"] = json!("system");
        }
        if message["role"] == "assistant"
            && let Some(fields) = message.as_object_mut()
        {
            for name in [
                "reasoning_content",
                "reasoning_encrypted_content",
                "encrypted_content",
            ] {
                fields.shift_remove(name);
            }
        }
    }
    Some(translated)
}

pub(crate) fn anthropic_messages(
    body: Value,
    accept: &str,
    preserve_cache_control: bool,
) -> Option<Value> {
    convert_anthropic_messages(body, accept, preserve_cache_control, false, false)
}

pub(super) fn anthropic_messages_for_responses(body: Value, accept: &str) -> Option<Value> {
    convert_anthropic_messages(body, accept, false, true, false)
}

fn convert_anthropic_messages(
    mut body: Value,
    accept: &str,
    preserve_cache_control: bool,
    responses: bool,
    inline_files: bool,
) -> Option<Value> {
    if !responses && (body.get("response_format").is_some() || unsupported_thinking(&body)) {
        return None;
    }
    let stream = stream_requested(&body, accept);
    let mut result = Map::new();
    copy_field(&mut result, "model", &body, "model");
    result.insert("messages".into(), Value::Array(Vec::new()));
    result.insert("stream".into(), Value::Bool(stream));

    if truthy(&body["max_tokens"]) {
        let value = &body["max_tokens"];
        let tokens = value
            .as_f64()
            .or_else(|| value.as_str()?.trim().parse().ok())?;
        let mut adjusted = if body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
            && tokens < 32_000.0
        {
            json!(32_000)
        } else {
            value.clone()
        };
        let mut tokens = crate::thinking_intent::numeric(&adjusted).unwrap_or(tokens);
        let budget = &body["thinking"]["budget_tokens"];
        if responses
            && truthy(budget)
            && let Some(limit) = crate::thinking_intent::numeric(budget)
            && tokens <= limit
        {
            adjusted = if let Some(text) = budget.as_str() {
                json!(format!("{text}1024"))
            } else {
                let adjusted = limit + 1024.0;
                if adjusted.fract() == 0.0
                    && adjusted >= i64::MIN as f64
                    && adjusted < i64::MAX as f64
                {
                    json!(adjusted as i64)
                } else {
                    json!(adjusted)
                }
            };
            tokens = crate::thinking_intent::numeric(&adjusted).unwrap_or(tokens);
        }
        if tokens > 64_000.0 {
            adjusted = json!(64_000);
        }
        result.insert("max_tokens".into(), adjusted);
    }
    copy_field(&mut result, "temperature", &body, "temperature");

    let mut output = Vec::new();
    let system = &body["system"];
    if truthy(system) {
        let content = match system {
            Value::Array(blocks)
                if preserve_cache_control
                    && blocks.iter().any(|block| truthy(&block["cache_control"])) =>
            {
                let mut parts: Vec<Value> = Vec::new();
                for block in blocks {
                    block.as_object()?;
                    let text = block["text"].as_str().map_or("", strip_billing_header);
                    let cache = &block["cache_control"];
                    if !text.is_empty() {
                        let mut part = json!({"type":"text","text":text});
                        if truthy(cache) {
                            part["cache_control"] = cache.clone();
                        }
                        parts.push(part);
                    } else if truthy(cache)
                        && let Some(last) = parts.last_mut()
                    {
                        last["cache_control"] = cache.clone();
                    }
                }
                if parts.iter().any(|part| truthy(&part["cache_control"])) {
                    json!(parts)
                } else {
                    json!(
                        parts
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                }
            }
            Value::Array(blocks) => {
                let mut parts = Vec::new();
                for block in blocks {
                    block.as_object()?;
                    let part = block["text"].as_str().map_or("", strip_billing_header);
                    if !part.is_empty() {
                        parts.push(part);
                    }
                }
                json!(parts.join("\n"))
            }
            Value::String(text) => json!(strip_billing_header(text)),
            _ => Value::Null,
        };
        if truthy(&content) {
            output.push(json!({ "role": "system", "content": content }));
        }
    }

    let mut messages = std::mem::take(body.get_mut("messages")?.as_array_mut()?);
    preprocess_tool_history(&mut messages)?;
    for message in messages {
        convert_claude_message(
            message,
            &mut output,
            preserve_cache_control,
            responses,
            inline_files,
        )?;
    }
    repair_claude_results(&mut output);
    result.insert("messages".into(), Value::Array(output));

    if let Some(tools) = body["tools"].as_array() {
        result.insert(
            "tools".into(),
            Value::Array(tools.iter().map(claude_tool).collect::<Option<Vec<_>>>()?),
        );
    }
    let choice = &body["tool_choice"];
    if truthy(choice) {
        result.insert(
            "tool_choice".into(),
            match choice {
                Value::String(_) => choice.clone(),
                _ if choice["type"] == "any" => json!("required"),
                _ if choice["type"] == "tool" => {
                    let mut spec = Map::new();
                    copy_field(&mut spec, "name", choice, "name");
                    json!({ "type": "function", "function": spec })
                }
                _ => json!("auto"),
            },
        );
    }
    Some(Value::Object(result))
}

fn strip_billing_header(text: &str) -> &str {
    const HEADER: &str = "x-anthropic-billing-header:";
    if text
        .get(..HEADER.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(HEADER))
    {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        text
    }
}

fn convert_claude_message(
    mut message: Value,
    output: &mut Vec<Value>,
    preserve_cache_control: bool,
    responses: bool,
    inline_files: bool,
) -> Option<()> {
    if message["content"].is_object() {
        message["content"] = json!([std::mem::take(&mut message["content"])]);
    }
    let content = &message["content"];
    let inline_files = inline_files && message["role"] == "user";
    if !responses
        && content.as_array().is_some_and(|parts| {
            parts.iter().any(|part| match part["type"].as_str() {
                Some("thinking" | "redacted_thinking") => false,
                Some("document") if inline_files => false,
                _ => unsupported_claude_block(part),
            })
        })
    {
        return None;
    }
    if message["role"] == "system" {
        let text = if let Some(parts) = content.as_array() {
            let text_parts = parts.iter().filter(|part| part["type"] == "text");
            let mut lines = Vec::new();
            for part in text_parts {
                if truthy(&part["text"]) {
                    lines.push(part["text"].as_str()?);
                }
            }
            lines.join("\n")
        } else {
            content.as_str().unwrap_or_default().to_owned()
        };
        if !text.trim_matches(js_space).is_empty() {
            output.push(json!({ "role": "user", "content": format!("<instructions>\n{text}\n</instructions>") }));
        }
        return Some(());
    }
    let role = if matches!(message["role"].as_str(), Some("user" | "tool")) {
        "user"
    } else {
        "assistant"
    };
    if content.is_string() {
        output.push(json!({ "role": role, "content": content }));
        return Some(());
    }
    let Some(blocks) = content.as_array() else {
        return Some(());
    };
    let mut parts = Vec::new();
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                let mut part = json!({ "type": "text", "text": block["text"].as_str()? });
                if preserve_cache_control && truthy(&block["cache_control"]) {
                    part["cache_control"] = block["cache_control"].clone();
                }
                parts.push(part);
            }
            Some("image") => {
                if let Some(image) = claude_image(block) {
                    parts.push(image);
                } else if !responses {
                    return None;
                }
            }
            Some("document") if inline_files => parts.push(claude_document(block)?),
            Some("tool_use") => {
                let empty = json!({});
                let input = &block["input"];
                let arguments = js_json(if truthy(input) { input } else { &empty }, false)?;
                let mut function = Map::new();
                copy_field(&mut function, "name", block, "name");
                function.insert("arguments".into(), json!(arguments));
                calls.push(tool_call(block, "id", function));
            }
            Some("tool_result") => {
                let content = &block["content"];
                let images: Vec<_> = content
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(claude_image)
                    .collect();
                results.push(tool_result(
                    block,
                    "tool_use_id",
                    Some(claude_result_text(content, !images.is_empty())?),
                ));
                if !images.is_empty() {
                    let id = block
                        .get("tool_use_id")
                        .map(crate::chat_output::js_text)
                        .unwrap_or_else(|| "undefined".into());
                    parts.push(json!({
                        "type":"text", "text":format!("[Image from tool result {id}]"),
                    }));
                    parts.extend(images);
                }
            }
            _ => (),
        }
    }
    if !results.is_empty() {
        output.extend(results);
        if !parts.is_empty() {
            output.push(json!({ "role": "user", "content": collapse_claude_text(parts) }));
        }
    } else if !calls.is_empty() {
        let mut result = json!({ "role": "assistant" });
        if !parts.is_empty() {
            result["content"] = collapse_claude_text(parts);
        }
        result["tool_calls"] = Value::Array(calls);
        output.push(result);
    } else if !parts.is_empty() {
        output.push(json!({ "role": role, "content": collapse_claude_text(parts) }));
    } else if blocks.is_empty() {
        output.push(json!({ "role": role, "content": "" }));
    }
    Some(())
}

fn claude_image(block: &Value) -> Option<Value> {
    let source = &block["source"];
    if block["type"] != "image" || source["type"] != "base64" {
        return None;
    }
    let mime = source["media_type"].as_str()?;
    let bytes = source["data"].as_str()?;
    Some(json!({
        "type": "image_url",
        "image_url": { "url": format!("data:{mime};base64,{bytes}") },
    }))
}

fn claude_document(block: &Value) -> Option<Value> {
    let source = &block["source"];
    if source["type"] != "base64" || source["media_type"] != "application/pdf" {
        return None;
    }
    let data = source["data"].as_str()?;
    if !base64_payload(data) {
        return None;
    }
    let mut filename = block["title"]
        .as_str()
        .map(|title| title.trim_matches(js_space))
        .filter(|title| !title.is_empty())
        .unwrap_or("document.pdf")
        .to_owned();
    if !filename
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("pdf"))
    {
        filename.push_str(".pdf");
    }
    Some(json!({
        "type":"file",
        "file":{"file_data":data,"filename":filename,"mime_type":"application/pdf"},
    }))
}

fn unsupported_claude_block(block: &Value) -> bool {
    matches!(
        block["type"].as_str(),
        Some(
            "document"
                | "audio"
                | "file"
                | "image_url"
                | "input_audio"
                | "audio_url"
                | "thinking"
                | "redacted_thinking"
                | "server_tool_use"
                | "web_search_tool_result"
        )
    ) || (block["type"] == "tool_result"
        && block["content"]
            .as_array()
            .is_some_and(|parts| parts.iter().any(unsupported_claude_block)))
}

fn collapse_claude_text(mut parts: Vec<Value>) -> Value {
    if parts.len() == 1 && parts[0]["type"] == "text" && !truthy(&parts[0]["cache_control"]) {
        std::mem::take(&mut parts[0]["text"])
    } else {
        Value::Array(parts)
    }
}

fn claude_result_text(value: &Value, has_images: bool) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    if let Some(parts) = value.as_array() {
        let mut text = Vec::new();
        for part in parts.iter().filter(|part| part["type"] == "text") {
            if part["text"].is_null() {
                text.push("");
            } else {
                text.push(part["text"].as_str()?);
            }
        }
        let joined = text.join("\n");
        return if joined.is_empty() && !has_images {
            js_json(value, false)
        } else {
            Some(joined)
        };
    }
    if truthy(value) {
        js_json(value, false)
    } else {
        Some(String::new())
    }
}

fn repair_claude_results(messages: &mut Vec<Value>) {
    let mut index = 0;
    while index < messages.len() {
        let mut position = index + 1;
        if messages[index]["role"] == "assistant"
            && let Some(calls) = messages[index]["tool_calls"].as_array()
            && !calls.is_empty()
        {
            let mut responded = HashSet::new();
            while let Some(message) = messages.get(position)
                && message["role"] == "tool"
                && truthy(&message["tool_call_id"])
            {
                responded.insert(message["tool_call_id"].as_str());
                position += 1;
            }
            let missing: Vec<_> = calls
                .iter()
                .filter(|call| !responded.contains(&call["id"].as_str()))
                .map(|call| tool_result(call, "id", Some("[No response received]".into())))
                .collect();
            let count = missing.len();
            if count != 0 {
                messages.splice(position..position, missing);
                index = position + count;
                continue;
            }
        }
        index += 1;
    }
}

fn claude_tool(tool: &Value) -> Option<Value> {
    tool.as_object()?;
    let mut function = Map::new();
    copy_field(&mut function, "name", tool, "name");
    function.insert("description".into(), json!(description(tool)?));
    function.insert(
        "parameters".into(),
        if truthy(&tool["input_schema"]) {
            tool["input_schema"].clone()
        } else {
            empty_parameters()
        },
    );
    Some(json!({ "type": "function", "function": function }))
}

pub(crate) fn responses_preview(body: Value) -> Option<Value> {
    openai_responses(body, true, false)
}

fn openai_responses(mut body: Value, preview: bool, inline_files: bool) -> Option<Value> {
    if !preview
        && (truthy(&body["previous_response_id"])
            || truthy(&body["conversation"])
            || body["store"] == true)
        || truthy(&body["messages"])
    {
        return None;
    }
    let input = body.as_object_mut()?.shift_remove("input")?;
    if !truthy(&input) {
        return None;
    }

    let mut messages = Vec::new();
    if truthy(&body["instructions"]) {
        messages.push(json!({ "role": "system", "content": body["instructions"] }));
    }
    let items = match input {
        Value::String(text) => vec![user_input_text(&text)],
        Value::Array(items) if items.is_empty() => vec![user_input_text("...")],
        Value::Array(items) => items,
        _ => return None,
    };
    let mut pending_assistant = None;
    let mut reasoning = Map::new();
    let mut custom_names = Vec::new();
    let mut additional_tools = Vec::new();
    for item in items {
        item.as_object()?;
        let item_type = if truthy(&item["type"]) {
            item["type"].as_str()?
        } else if truthy(&item["role"]) {
            "message"
        } else {
            ""
        };
        match item_type {
            "message" => {
                flush_pending(&mut pending_assistant, &mut messages);
                let mut message = responses_message(item, inline_files)?;
                if message["role"] == "assistant" {
                    message
                        .as_object_mut()?
                        .extend(std::mem::take(&mut reasoning));
                } else {
                    reasoning.clear();
                }
                messages.push(message);
            }
            "function_call" | "custom_tool_call" => {
                let assistant = pending_assistant.get_or_insert_with(|| {
                    let mut fields = Map::from_iter([
                        ("role".into(), json!("assistant")),
                        ("content".into(), Value::Null),
                        ("tool_calls".into(), json!([])),
                    ]);
                    fields.extend(std::mem::take(&mut reasoning));
                    Value::Object(fields)
                });
                let Some(name) = item["name"]
                    .as_str()
                    .filter(|name| !name.trim_matches(js_space).is_empty())
                else {
                    continue;
                };
                let arguments = if item_type == "custom_tool_call" {
                    remember_tool(&mut custom_names, name);
                    let input = &item["input"];
                    let raw = if let Some(text) = input.as_str() {
                        text.to_owned()
                    } else if input.is_null() {
                        "\"\"".to_owned()
                    } else {
                        js_json(input, false)?
                    };
                    js_json(&json!({"input":raw}), false)?
                } else if let Some(text) = item["arguments"].as_str() {
                    text.to_owned()
                } else if item["arguments"].is_null() {
                    "{}".into()
                } else {
                    js_json(&item["arguments"], false)?
                };
                let function = Map::from_iter([
                    ("name".into(), json!(name)),
                    ("arguments".into(), json!(arguments)),
                ]);
                assistant["tool_calls"]
                    .as_array_mut()?
                    .push(tool_call(&item, "call_id", function));
            }
            "function_call_output" | "custom_tool_call_output" => {
                flush_pending(&mut pending_assistant, &mut messages);
                let output = item.get("output").map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| js_json(value, false))
                });
                let output = match output {
                    Some(text) => Some(text?),
                    None => None,
                };
                messages.push(tool_result(&item, "call_id", output));
            }
            "additional_tools" => {
                if let Some(tools) = item["tools"].as_array() {
                    additional_tools.extend(tools.iter().cloned());
                }
            }
            "reasoning" => remember_reasoning(&mut reasoning, &item),
            _ => return None,
        }
    }
    flush_pending(&mut pending_assistant, &mut messages);
    body["messages"] = Value::Array(messages);

    let mut tools = body["tools"].as_array().cloned().unwrap_or_default();
    tools.extend(additional_tools);
    if !tools.is_empty() {
        let mut converted = Vec::with_capacity(tools.len());
        for tool in tools {
            if !truthy(&tool["function"]) {
                let Some(name) = tool["name"]
                    .as_str()
                    .filter(|name| !name.trim_matches(js_space).is_empty())
                else {
                    continue;
                };
                if tool["type"] == "custom" {
                    remember_tool(&mut custom_names, name);
                    converted.push(custom_tool(name, &tool));
                    continue;
                }
            }
            converted.push(responses_tool(tool)?);
        }
        body["tools"] = Value::Array(converted);
    }
    if !custom_names.is_empty() {
        body["_customToolNames"] = json!(custom_names);
    }

    let map = body.as_object_mut()?;
    if let Some(limit) = map.shift_remove("max_output_tokens") {
        map.entry("max_tokens").or_insert(limit);
    }
    for field in [
        "instructions",
        "include",
        "prompt_cache_key",
        "store",
        "reasoning",
        "client_metadata",
    ] {
        map.shift_remove(field);
    }
    Some(body)
}

pub(super) fn responses_custom_tool_names(body: &Value) -> impl Iterator<Item = &str> {
    let items = body["input"].as_array().into_iter().flatten();
    body["_customToolNames"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .chain(
            body["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(
                    items
                        .clone()
                        .filter(|item| item["type"] == "additional_tools")
                        .flat_map(|item| item["tools"].as_array().into_iter().flatten()),
                )
                .filter(|tool| tool["type"] == "custom" && !truthy(&tool["function"]))
                .chain(items.filter(|item| item["type"] == "custom_tool_call"))
                .filter_map(|item| item["name"].as_str()),
        )
        .filter(|name| !name.trim_matches(js_space).is_empty())
}

fn user_input_text(text: &str) -> Value {
    let text = if text.trim_matches(js_space).is_empty() {
        "..."
    } else {
        text
    };
    json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
}

fn responses_message(mut item: Value, inline_files: bool) -> Option<Value> {
    let mut message = Map::new();
    copy_field(&mut message, "role", &item, "role");
    let inline_files = inline_files && item["role"] == "user";
    if let Some(mut content) = item.as_object_mut()?.shift_remove("content") {
        if let Some(parts) = content.as_array_mut() {
            for part in parts {
                match part["type"].as_str() {
                    Some("input_text" | "output_text") => {
                        let mut text = Map::from_iter([("type".into(), json!("text"))]);
                        copy_field(&mut text, "text", part, "text");
                        *part = Value::Object(text);
                    }
                    Some("input_image") => {
                        let url = part["image_url"].as_str().filter(|url| !url.is_empty())?;
                        let detail = if truthy(&part["detail"]) {
                            part["detail"].clone()
                        } else {
                            json!("auto")
                        };
                        *part = json!({ "type": "image_url", "image_url": { "url": url, "detail": detail } });
                    }
                    Some("input_file") if inline_files => {
                        if !inline_file_fields(part) {
                            return None;
                        }
                        *part = json!({
                            "type":"file",
                            "file":{"file_data":part["file_data"].take(),
                                    "filename":part["filename"].take(),
                                    "mime_type":part["mime_type"].take()},
                        });
                    }
                    Some(
                        "input_file" | "input_audio" | "image" | "image_url" | "audio_url" | "file"
                        | "document" | "reasoning" | "summary_text",
                    ) => return None,
                    _ => (),
                }
            }
        }
        message.insert("content".into(), content);
    }
    Some(Value::Object(message))
}

fn inline_file_fields(file: &Value) -> bool {
    if !file["file_id"].is_null() || !file["file_url"].is_null() {
        return false;
    }
    let Some(data) = file["file_data"]
        .as_str()
        .map(|data| data.trim_matches(js_space))
    else {
        return false;
    };
    if data
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        return data[5..].split_once(',').is_some_and(|(header, payload)| {
            header
                .split(';')
                .any(|value| value.eq_ignore_ascii_case("base64"))
                && base64_payload(payload)
        });
    }
    base64_payload(data)
}

fn base64_payload(data: &str) -> bool {
    !data.trim_matches(js_space).is_empty()
        && data
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=') || js_space(ch))
}

fn tool_call(item: &Value, id_field: &str, function: Map<String, Value>) -> Value {
    let mut call = Map::new();
    copy_field(&mut call, "id", item, id_field);
    call.insert("type".into(), json!("function"));
    call.insert("function".into(), Value::Object(function));
    Value::Object(call)
}

fn tool_result(item: &Value, id_field: &str, text: Option<String>) -> Value {
    let mut output = Map::from_iter([("role".into(), json!("tool"))]);
    copy_field(&mut output, "tool_call_id", item, id_field);
    if let Some(text) = text {
        output.insert("content".into(), json!(text));
    }
    Value::Object(output)
}

fn flush_pending(pending: &mut Option<Value>, messages: &mut Vec<Value>) {
    if let Some(message) = pending.take() {
        messages.push(message);
    }
}

fn remember_reasoning(pending: &mut Map<String, Value>, item: &Value) {
    let summary = ["summary", "content"].into_iter().find_map(|field| {
        let text = item[field]
            .as_array()?
            .iter()
            .filter_map(|part| part.get("text").filter(|value| truthy(value)))
            .map(crate::chat_output::js_text)
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then_some(text)
    });
    if let Some(text) = summary {
        let text = if let Some(previous) = pending.get("reasoning_content").and_then(Value::as_str)
        {
            format!("{previous}\n{text}")
        } else {
            text
        };
        pending.insert("reasoning_content".into(), json!(text));
    }
    if let Some(encrypted) = item["encrypted_content"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        pending.insert("encrypted_content".into(), json!(encrypted));
    }
}

fn remember_tool(names: &mut Vec<String>, name: &str) {
    if !names.iter().any(|existing| existing == name) {
        names.push(name.to_owned());
    }
}

fn custom_tool(name: &str, tool: &Value) -> Value {
    let hint = ["syntax", "definition"]
        .into_iter()
        .map(|field| &tool["format"][field])
        .filter(|value| truthy(value))
        .map(crate::chat_output::js_text)
        .collect::<Vec<_>>()
        .join("\n");
    let description = if truthy(&tool["description"]) {
        crate::chat_output::js_text(&tool["description"])
    } else {
        String::new()
    };
    let description = [description, hint]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    json!({"type":"function","function":{
        "name":name,"description":description,"parameters":{
            "type":"object","properties":{"input":{"type":"string","description":"Raw freeform input for this custom tool"}},
            "required":["input"],"additionalProperties":false,
        },
    }})
}

fn responses_tool(tool: Value) -> Option<Value> {
    if tool["type"] == "custom" {
        return None;
    }
    if truthy(&tool["function"]) {
        return Some(tool);
    }
    let name = tool["name"].as_str()?.trim_matches(js_space);
    if name.is_empty() {
        return None;
    }
    let mut parameters = if truthy(&tool["parameters"]) {
        tool["parameters"].clone()
    } else {
        empty_parameters()
    };
    if parameters["type"] == "object" && !truthy(&parameters["properties"]) {
        parameters["properties"] = json!({});
    }
    let mut function = Map::from_iter([
        ("name".into(), tool["name"].clone()),
        ("description".into(), json!(description(&tool)?)),
        ("parameters".into(), parameters),
    ]);
    copy_field(&mut function, "strict", &tool, "strict");
    Some(json!({ "type": "function", "function": function }))
}

fn empty_parameters() -> Value {
    json!({ "type": "object", "properties": {} })
}

fn description(tool: &Value) -> Option<&str> {
    if truthy(&tool["description"]) {
        tool["description"].as_str()
    } else {
        Some("")
    }
}

fn copy_field(output: &mut Map<String, Value>, name: &str, input: &Value, source: &str) {
    if let Some(field) = input.get(source) {
        output.insert(name.into(), field.clone());
    }
}
