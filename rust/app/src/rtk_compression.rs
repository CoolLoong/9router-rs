use serde_json::Value;

use crate::chat_input::truthy;
use crate::rtk_filters;

pub fn apply(body: &mut Value) {
    if body.get("conversationState").is_some_and(truthy) {
        if let Some(state) = body.get_mut("conversationState") {
            if let Some(history) = state.get_mut("history").and_then(Value::as_array_mut) {
                for message in history {
                    kiro_result(message);
                }
            }
            if let Some(message) = state.get_mut("currentMessage") {
                kiro_result(message);
            }
        }
        return;
    }

    let key = if body["messages"].is_array() {
        "messages"
    } else {
        "input"
    };
    let Some(items) = body.get_mut(key).and_then(Value::as_array_mut) else {
        return;
    };
    for message in items {
        if message["type"] == "function_call_output" {
            if let Some(output) = message.get_mut("output") {
                inner(output, "input_text");
            }
            continue;
        }
        if message["role"] == "tool" {
            if let Some(content) = message.get_mut("content") {
                inner(content, "text");
            }
            continue;
        }
        let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in parts {
            if part["type"] == "tool_result"
                && part["is_error"] != true
                && let Some(content) = part.get_mut("content")
            {
                inner(content, "text");
            }
        }
    }
}

fn kiro_result(message: &mut Value) {
    let Some(results) = message
        .get_mut("userInputMessage")
        .and_then(|body| body.get_mut("userInputMessageContext"))
        .and_then(|context| context.get_mut("toolResults"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for item in results.iter_mut().filter(|item| item["status"] != "error") {
        if let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) {
            for part in parts {
                if let Some(text) = part.get_mut("text") {
                    compress(text);
                }
            }
        }
    }
}

fn inner(value: &mut Value, part_type: &str) {
    if let Some(parts) = value.as_array_mut() {
        for part in parts {
            if part["type"] == part_type
                && let Some(text) = part.get_mut("text")
            {
                compress(text);
            }
        }
    } else {
        compress(value);
    }
}

fn compress(value: &mut Value) {
    if let Some(text) = value.as_str().and_then(rtk_filters::compress) {
        *value = Value::String(text);
    }
}
