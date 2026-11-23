use serde_json::{Value, json};

use crate::chat_format::ChatFormat;

use super::{js_add, js_text, normalize_numbers, truthy};

pub(super) fn translate(body: Value, context: &Value, format: ChatFormat) -> Value {
    let choice = &body["choices"][0];
    if !truthy(choice) {
        return body;
    }
    match format {
        ChatFormat::Chat => body,
        ChatFormat::Messages => to_message(&body, choice),
        ChatFormat::Responses => to_response(&body, choice, context),
    }
}

fn or_else(values: &[&Value], fallback: Value) -> Value {
    values
        .iter()
        .find(|value| truthy(value))
        .map_or(fallback, |value| (*value).clone())
}

fn tool_calls(message: &Value) -> impl Iterator<Item = &Value> {
    message["tool_calls"].as_array().into_iter().flatten()
}

fn tokens(usage: &Value, openai: &str, alternate: &str) -> Value {
    or_else(&[&usage[openai], &usage[alternate]], json!(0))
}

fn model(body: &Value) -> Value {
    or_else(&[&body["model"]], json!("unknown"))
}

fn to_message(body: &Value, choice: &Value) -> Value {
    let message = &choice["message"];
    let mut content = Vec::new();
    let reasoning = or_else(
        &[
            &message["reasoning_content"],
            &message["provider_specific_fields"]["reasoning_content"],
        ],
        if message["reasoning"].is_string() {
            message["reasoning"].clone()
        } else {
            Value::Null
        },
    );
    if truthy(&reasoning) {
        content.push(json!({"type": "thinking", "thinking": reasoning}));
    }
    if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
        content.push(json!({"type": "text", "text": text}));
    }
    for tool in tool_calls(message) {
        let function = &tool["function"];
        let args = or_else(&[&function["arguments"], &tool["arguments"]], Value::Null);
        let id = if truthy(&tool["id"]) {
            tool["id"].clone()
        } else {
            json!(format!(
                "toolu_{}_{}",
                chrono::Utc::now().timestamp_millis(),
                content.len()
            ))
        };
        content.push(json!({
            "type": "tool_use",
            "id": id,
            "name": or_else(&[&function["name"], &tool["name"]], json!("")),
            "input": message_tool_input(args),
        }));
    }
    if content.is_empty() {
        content.push(json!({"type": "text", "text": ""}));
    }

    let id = if truthy(&body["id"]) {
        js_text(&body["id"])
    } else {
        format!("msg_{}", chrono::Utc::now().timestamp_millis())
    };
    let usage = &body["usage"];
    json!({
        "id": id.strip_prefix("chatcmpl-").unwrap_or(&id),
        "type": "message",
        "role": "assistant",
        "model": model(body),
        "content": content,
        "stop_reason": message_stop_reason(choice["finish_reason"].as_str()),
        "stop_sequence": null,
        "usage": {
            "input_tokens": tokens(usage, "prompt_tokens", "input_tokens"),
            "output_tokens": tokens(usage, "completion_tokens", "output_tokens"),
        },
    })
}

fn message_tool_input(value: Value) -> Value {
    if !truthy(&value) {
        json!({})
    } else if value.is_object() || value.is_array() {
        value
    } else {
        serde_json::from_str(&js_text(&value)).unwrap_or_else(|_| json!({}))
    }
}

fn message_stop_reason(reason: Option<&str>) -> &str {
    match reason {
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        Some("content_filter") => "refusal",
        _ => "end_turn",
    }
}

fn to_response(body: &Value, choice: &Value, context: &Value) -> Value {
    let message = &choice["message"];
    let mut output = Vec::new();
    let reasoning = or_else(
        &[&message["reasoning_content"], &message["reasoning"]],
        Value::Null,
    );
    if let Some(text) = reasoning.as_str().filter(|text| !text.is_empty()) {
        output.push(
            json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": text}]}),
        );
    }
    if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
        output.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
    }
    for tool in tool_calls(message) {
        let function = &tool["function"];
        let name = or_else(&[&function["name"]], json!(""));
        let id = or_else(&[&tool["id"]], json!(""));
        let args = response_tool_arguments(&function["arguments"]);
        if is_custom_tool(context, &name) {
            let parsed = serde_json::from_str::<Value>(&args).ok();
            let input = parsed
                .as_ref()
                .and_then(|value| value["input"].as_str())
                .unwrap_or(&args);
            output.push(json!({
                "type": "custom_tool_call",
                "id": format!("ctc_{}", js_text(&id)),
                "call_id": id,
                "name": name,
                "input": input,
            }));
        } else {
            output.push(json!({
                "type": "function_call",
                "id": format!("fc_{}", js_text(&id)),
                "call_id": id,
                "name": name,
                "arguments": args,
            }));
        }
    }

    let original_id = or_else(&[&body["id"]], json!(""));
    let id = js_text(&original_id);
    let finish_reason = &choice["finish_reason"];
    let status = if matches!(finish_reason.as_str(), Some("stop" | "tool_calls"))
        || !truthy(finish_reason)
    {
        json!("completed")
    } else {
        finish_reason.clone()
    };
    let usage = &body["usage"];
    let total_tokens = if truthy(&usage["total_tokens"]) {
        usage["total_tokens"].clone()
    } else {
        js_add(
            &or_else(&[&usage["prompt_tokens"]], json!(0)),
            &or_else(&[&usage["completion_tokens"]], json!(0)),
        )
    };
    json!({
        "id": format!("resp_{}", id.strip_prefix("chatcmpl-").unwrap_or(&id)),
        "object": "response",
        "created_at": or_else(&[&body["created"]], json!(chrono::Utc::now().timestamp())),
        "model": model(body),
        "status": status,
        "background": false,
        "error": null,
        "output": output,
        "usage": {
            "input_tokens": tokens(usage, "prompt_tokens", "input_tokens"),
            "output_tokens": tokens(usage, "completion_tokens", "output_tokens"),
            "total_tokens": total_tokens,
        },
    })
}

fn is_custom_tool(context: &Value, name: &Value) -> bool {
    name.as_str().is_some_and(|name| {
        crate::chat_protocol_input::responses_custom_tool_names(context)
            .any(|custom| custom == name)
    })
}

fn response_tool_arguments(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    let mut mapped = if truthy(value) {
        value.clone()
    } else {
        json!({})
    };
    normalize_numbers(&mut mapped);
    mapped.to_string()
}
