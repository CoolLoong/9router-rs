use std::collections::HashSet;

use provider_capabilities_rs::port::model_capabilities;
use serde_json::{Value, json};

use crate::chat_format::ChatFormat;
use crate::chat_input::{PreparedChat, js_json, preprocess_tool_history, truthy};
use crate::provider_index::js_trim;
use crate::thinking_intent::{Thinking, budget_level, model_suffix, thinking_intent};
use crate::{chat_protocol_input, openai_models, responses_request};

const ALLOWED_FIELDS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "stream",
    "store",
    "reasoning",
    "include",
    "temperature",
    "top_p",
    "max_output_tokens",
    "parallel_tool_calls",
    "text",
    "metadata",
    "prompt_cache_key",
];
const HOSTED_TOOLS: &[&str] = &[
    "web_search",
    "x_search",
    "web_search_preview",
    "file_search",
    "image_generation",
    "code_interpreter",
    "mcp",
    "local_shell",
];
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];

pub(super) fn prepare(
    format: ChatFormat,
    mut body: Value,
    model: &str,
    accept: &str,
) -> Option<PreparedChat> {
    body.as_object()?;
    let model = if format == ChatFormat::Messages {
        let (model, suffix) = model_suffix(model);
        let reasoning = if model_capabilities(Some("grok-cli"), model, None)["reasoning"] == true {
            match suffix.or_else(|| thinking_intent(&body)) {
                Some(intent) => Some(("reasoning_effort", json!(anthropic_effort(intent)))),
                None => body
                    .get("reasoning")
                    .cloned()
                    .map(|value| ("reasoning", value)),
            }
        } else {
            None
        };
        body = chat_protocol_input::anthropic_messages_for_responses(body, accept)?;
        if let Some((field, value)) = reasoning {
            body[field] = value;
        }
        model
    } else {
        model
    };
    if format != ChatFormat::Responses {
        if truthy(&body["input"]) {
            if body.get("max_output_tokens").is_none()
                && let Some(limit) = body
                    .get("max_completion_tokens")
                    .or_else(|| body.get("max_tokens"))
            {
                body["max_output_tokens"] = limit.clone();
            }
        } else {
            if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
                preprocess_tool_history(messages)?;
            }
            let options = body.as_object_mut()?;
            let reasoning = options.shift_remove("reasoning");
            let effort = options.shift_remove("reasoning_effort");
            body = responses_request::from_chat(body, model)?;
            if let Some(reasoning) = reasoning {
                body["reasoning"] = reasoning;
            }
            if let Some(effort) = effort {
                if format == ChatFormat::Messages {
                    body["reasoning_effort"] = effort;
                } else {
                    body["reasoning"] = json!({"effort": effort, "summary": "auto"});
                }
            }
        }
    }

    normalize_input(&mut body)?;
    normalize_tools(&mut body);
    let (upstream_model, model_effort) = model_and_effort(model);
    normalize_reasoning(&mut body, upstream_model, model_effort);
    body["model"] = json!(upstream_model);
    body["stream"] = json!(true);
    body["store"] = json!(false);
    body.as_object_mut()?
        .retain(|key, _| ALLOWED_FIELDS.contains(&key.as_str()));
    Some(PreparedChat { body, stream: true })
}

fn anthropic_effort(intent: Thinking) -> String {
    match intent {
        Thinking::Level(level) if matches!(level.as_str(), "max" | "ultra") => "xhigh".into(),
        Thinking::Level(level) => level,
        Thinking::Budget(budget) => match budget_level(budget) {
            "max" => "xhigh".into(),
            level => level.into(),
        },
        Thinking::Auto => "auto".into(),
        Thinking::None => "none".into(),
    }
}

fn model_and_effort(model: &str) -> (&str, Option<&str>) {
    let effort = model
        .rsplit_once('-')
        .filter(|(_, effort)| EFFORTS.contains(effort));
    let (base, model_effort) = effort.map_or((model, None), |(base, effort)| (base, Some(effort)));
    let mapped = openai_models::data()["models"]["gcli"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| entry["id"] == base)
        .and_then(|entry| entry["upstreamModelId"].as_str())
        .unwrap_or(base);
    (mapped, model_effort)
}

fn normalize_reasoning(body: &mut Value, model: &str, model_effort: Option<&str>) {
    let supports_effort = model == "grok-4.5" || model.starts_with("grok-4.5-");
    let effort = [&body["reasoning"]["effort"], &body["reasoning_effort"]]
        .into_iter()
        .find(|value| truthy(value))
        .map(Value::as_str)
        .unwrap_or(model_effort)
        .map(js_trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !body["reasoning"].is_array() {
        if !body["reasoning"].is_object() {
            body["reasoning"] = json!({});
        }
        if !truthy(&body["reasoning"]["summary"]) {
            body["reasoning"]["summary"] = json!("concise");
        }
        if supports_effort {
            let effort = match effort.as_str() {
                "max" => "xhigh",
                effort if EFFORTS.contains(&effort) => effort,
                _ => "high",
            };
            body["reasoning"]["effort"] = json!(effort);
        } else if let Some(reasoning) = body["reasoning"].as_object_mut() {
            reasoning.shift_remove("effort");
        }
    }

    const ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";
    if !body["include"].is_array() {
        body["include"] = json!([]);
    }
    if let Some(include) = body["include"].as_array_mut()
        && !include.iter().any(|field| field == ENCRYPTED_REASONING)
    {
        include.push(json!(ENCRYPTED_REASONING));
    }
}

fn normalize_input(body: &mut Value) -> Option<()> {
    match &body["input"] {
        Value::String(text) => {
            let text = if js_trim(text).is_empty() {
                "..."
            } else {
                text
            };
            body["input"] = json!([{"type":"message","role":"user",
                "content":[{"type":"input_text","text":text}]}]);
        }
        Value::Array(input) if input.is_empty() => {
            body["input"] = json!([{"type":"message","role":"user",
                "content":[{"type":"input_text","text":"..."}]}]);
        }
        input if !truthy(input) => {
            let messages = body["messages"].as_array().filter(|rows| !rows.is_empty());
            body["input"] = match messages {
                Some(messages) => Value::Array(
                    messages
                        .iter()
                        .map(|message| {
                            let role = message
                                .get("role")
                                .filter(|value| truthy(value))
                                .cloned()
                                .unwrap_or(json!("user"));
                            let content = match message.get("content") {
                                Some(Value::String(text)) => text.clone(),
                                Some(value) if !value.is_null() => js_json(value, false)?,
                                _ => "\"\"".to_owned(),
                            };
                            Some(json!({"type":"message","role":role,"content":content}))
                        })
                        .collect::<Option<Vec<_>>>()?,
                ),
                None => json!([{"type":"message","role":"user","content":"..."}]),
            };
        }
        _ => {}
    }
    let Some(input) = body["input"].as_array_mut() else {
        return Some(());
    };
    let mut normalized = Vec::with_capacity(input.len());
    for item in std::mem::take(input) {
        if let Some(item) = normalize_item(item)? {
            normalized.push(item);
        }
    }
    let calls: HashSet<_> = normalized
        .iter()
        .filter(|item| item["type"] == "function_call")
        .filter_map(|item| call_id_key(&item["call_id"]))
        .collect();
    normalized.retain(|item| {
        item["type"] != "function_call_output"
            || call_id_key(&item["call_id"]).is_some_and(|id| calls.contains(&id))
    });
    *input = normalized;
    Some(())
}

fn normalize_item(mut item: Value) -> Option<Option<Value>> {
    if !truthy(&item) || item.as_str().is_some_and(stored_item_id) {
        return Some(None);
    }
    let Some(fields) = item.as_object_mut() else {
        return Some(Some(item));
    };
    fields.shift_remove("internal_chat_message_metadata_passthrough");
    let kind = item["type"].as_str().unwrap_or_default();
    let native_id = item["id"].as_str().is_some_and(native_item_id);
    match kind {
        "item_reference" => Some(None),
        "reasoning" if !native_id || !item["encrypted_content"].is_string() => Some(None),
        "function_call"
        | "custom_tool_call"
        | "function_call_output"
        | "custom_tool_call_output" => {
            let call_id = if truthy(&item["call_id"]) {
                &item["call_id"]
            } else {
                &item["id"]
            };
            if !truthy(call_id) {
                return Some(None);
            }
            if matches!(kind, "function_call_output" | "custom_tool_call_output") {
                return Some(Some(
                    json!({"type":"function_call_output","call_id":call_id,
                    "output":stringify_output(item.get("output"))?}),
                ));
            }
            let name = item["name"].as_str().map(js_trim).unwrap_or_default();
            if name.is_empty() {
                return Some(None);
            }
            let arguments = if kind == "custom_tool_call" {
                let input = item
                    .get("input")
                    .filter(|value| !value.is_null())
                    .or_else(|| item.get("arguments"));
                js_json(&json!({"input":stringify_output(input)?}), false)?
            } else {
                let arguments = item.get("arguments").filter(|value| !value.is_null());
                match arguments {
                    Some(value) => stringify_output(Some(value))?,
                    None => "{}".to_owned(),
                }
            };
            let mut call =
                json!({"type":"function_call","call_id":call_id,"name":name,"arguments":arguments});
            if kind == "function_call" {
                if native_id {
                    call["id"] = item["id"].clone();
                }
                if item["status"].is_string() {
                    call["status"] = item["status"].clone();
                }
            }
            Some(Some(call))
        }
        _ => {
            if !native_id && item["id"].as_str().is_some_and(stored_item_id) {
                item.as_object_mut()?.shift_remove("id");
            }
            Some(Some(item))
        }
    }
}

fn stored_item_id(id: &str) -> bool {
    ["rs_", "fc_", "resp_", "msg_"]
        .iter()
        .any(|prefix| id.starts_with(prefix))
}

fn native_item_id(id: &str) -> bool {
    id.split_once('_').is_some_and(|(prefix, id)| {
        matches!(prefix, "rs" | "msg" | "fc") && id.len() == 36 && uuid::Uuid::parse_str(id).is_ok()
    })
}

fn stringify_output(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => Some(text.clone()),
        Some(value) => js_json(value, false),
        None => Some(String::new()),
    }
}

fn call_id_key(id: &Value) -> Option<String> {
    if id.is_array() || id.is_object() {
        None
    } else {
        js_json(id, false)
    }
}

fn normalize_tools(body: &mut Value) {
    let mut functions = HashSet::new();
    let mut hosted = HashSet::new();
    let tools = body
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    let tools: Vec<_> = tools.into_iter().filter_map(|tool| {
        tool.as_object()?;
        let kind = tool["type"].as_str().unwrap_or_default();
        if HOSTED_TOOLS.contains(&kind) {
            hosted.insert(kind.to_owned());
            return Some(tool);
        }
        if !(kind.is_empty() || kind == "function" || tool["name"].is_string()) {
            return None;
        }
        let name = tool["name"].as_str().or_else(|| tool["function"]["name"].as_str()).map(tool_name)?;
        if name.is_empty() {
            return None;
        }
        let parameters = if kind == "custom" {
            json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"]})
        } else {
            [&tool["parameters"], &tool["function"]["parameters"]].into_iter()
                .find(|parameter| parameter.is_object()).cloned()
                .unwrap_or(json!({"type":"object","properties":{}}))
        };
        let mut function = json!({"type":"function","name":name,"parameters":parameters});
        if let Some(description) = tool["description"].as_str().or_else(|| tool["function"]["description"].as_str())
            && !description.is_empty()
        {
            function["description"] = json!(description);
        }
        functions.insert(name.to_owned());
        Some(function)
    }).collect();
    let Some(fields) = body.as_object_mut() else {
        return;
    };
    if tools.is_empty() {
        fields.shift_remove("tools");
        fields.shift_remove("tool_choice");
        return;
    }
    fields.insert("tools".into(), Value::Array(tools));
    let Some(choice) = fields
        .get("tool_choice")
        .filter(|choice| choice.is_object())
    else {
        return;
    };
    let kind = choice["type"].as_str().unwrap_or_default();
    if matches!(kind, "function" | "custom") {
        let name = choice
            .get("name")
            .filter(|value| !value.is_null())
            .unwrap_or(&choice["function"]["name"])
            .as_str()
            .map(tool_name);
        if let Some(name) = name.filter(|name| functions.contains(*name)) {
            fields.insert("tool_choice".into(), json!({"type":"function","name":name}));
        } else {
            fields.shift_remove("tool_choice");
        }
    } else if !hosted.contains(kind) {
        fields.shift_remove("tool_choice");
    }
}

fn tool_name(name: &str) -> &str {
    let name = js_trim(name);
    let mut units = 0;
    for (index, ch) in name.char_indices() {
        units += ch.len_utf16();
        if units > 128 {
            return &name[..index];
        }
    }
    name
}
