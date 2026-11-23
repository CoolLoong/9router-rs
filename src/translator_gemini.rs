use provider_capabilities_rs::port::model_capabilities;
use serde_json::{Value, json};

use crate::chat_format::ChatFormat;
use crate::chat_input::js_json;
use crate::chat_output::js_text;
use crate::gemini_request::{self, DEFAULT_SIGNATURE};
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::thinking_intent::{Thinking, budget_level, model_suffix, numeric, thinking_intent};

pub(super) fn destination(model: &str, stream: bool) -> String {
    let suffix = if stream {
        "streamGenerateContent?alt=sse"
    } else {
        "generateContent"
    };
    format!("{}/{model}:{suffix}", gemini_request::BASE_URL)
}

pub(super) fn credential(account: &Value) -> Option<(&'static str, &str)> {
    [
        ("x-goog-api-key", "apiKey"),
        ("Authorization", "accessToken"),
    ]
    .into_iter()
    .find_map(|(header, field)| {
        account[field]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(|value| (header, value))
    })
}

pub(super) fn preview(body: &Value, model: &str, account: &Value) -> Option<Value> {
    if !body.is_object() || super::source_format(body) != "openai" || body.get("request").is_some()
    {
        return None;
    }
    let stream = body["stream"] != false;
    let mut headers = json!({"Content-Type":"application/json"});
    if let Some((header, _)) = credential(account) {
        headers[header] = json!("[redacted]");
    }
    if stream {
        headers["Accept"] = json!("text/event-stream");
    }
    let prepared = gemini_request::prepare(
        ChatFormat::Chat,
        body.clone(),
        model,
        "",
        |_| None,
        DEFAULT_SIGNATURE,
    )?;
    Some(
        json!({"url":super::redact_url(&destination(model, stream)),"headers":headers,"body":prepared.body}),
    )
}

pub(super) fn to_openai(body: &Value, model: &str, provider: &str) -> Option<Value> {
    let mut messages = Vec::new();
    if let Some(instruction) = body.get("systemInstruction").filter(|value| truthy(value)) {
        let system = if let Some(text) = instruction.as_str() {
            text.to_owned()
        } else {
            instruction["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").filter(|text| truthy(text)))
                .map(js_text)
                .collect()
        };
        if !js_trim(&system).is_empty() {
            messages.push(json!({"role":"system","content":system}));
        }
    }
    for turn in body["contents"].as_array()? {
        append_turn(turn, &mut messages)?;
    }

    let mut result = json!({"model":model,"messages":messages,"stream":body["stream"] != false});
    let config = &body["generationConfig"];
    if let Some(limit) = config.get("maxOutputTokens").filter(|limit| truthy(limit)) {
        let mut limit = limit.clone();
        if !limit.is_number() {
            return None;
        }
        let count = numeric(&limit)?;
        if count > 64_000.0 {
            limit = json!(64_000);
        } else if count < 32_000.0
            && body["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty())
        {
            limit = json!(32_000);
        }
        result["max_tokens"] = limit;
    }
    for (from, to) in [("temperature", "temperature"), ("topP", "top_p")] {
        if let Some(value) = config.get(from) {
            result[to] = value.clone();
        }
    }

    let mut tools = Vec::new();
    for tool in body["tools"].as_array().into_iter().flatten() {
        let Some(declarations) = tool
            .get("functionDeclarations")
            .filter(|field| truthy(field))
        else {
            continue;
        };
        for declaration in declarations.as_array()? {
            let name = declaration["name"].as_str()?;
            let description = declaration
                .get("description")
                .filter(|field| truthy(field))
                .cloned()
                .unwrap_or(json!(""));
            let parameters = declaration
                .get("parameters")
                .filter(|field| truthy(field))
                .cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{}}));
            tools.push(json!({"type":"function","function":{"name":name,"description":description,"parameters":parameters}}));
        }
    }
    if !tools.is_empty() {
        result["tools"] = json!(tools);
    }

    let (clean_model, override_intent) = model_suffix(model);
    if let Some(intent) = override_intent.or_else(|| thinking_intent(body)) {
        let caps = model_capabilities(Some(provider), clean_model, None);
        if caps["reasoning"] == true {
            if provider != "gemini" {
                return None;
            }
            let effort = match &intent {
                Thinking::None if caps["thinkingCanDisable"] == false => "minimal",
                Thinking::None => "none",
                Thinking::Auto => "auto",
                Thinking::Budget(budget) => budget_level(*budget),
                Thinking::Level(level) => level,
            };
            let levels = crate::thinking_catalog::levels(
                provider,
                clean_model,
                &caps,
                crate::openai_models::data(),
            );
            let supports = |level: &str| {
                levels
                    .as_ref()
                    .is_some_and(|items| items.iter().any(|item| item == level))
            };
            result["reasoning_effort"] = json!(match effort {
                "ultra" if !supports("ultra") && supports("max") => "max",
                "max" | "ultra" if !supports(effort) => "xhigh",
                _ => effort,
            });
        }
    }
    Some(result)
}

fn append_turn(turn: &Value, messages: &mut Vec<Value>) -> Option<()> {
    if !turn.is_object() {
        return None;
    }
    let Some(parts) = turn["parts"].as_array() else {
        return Some(());
    };
    let mut blocks = Vec::new();
    let mut calls = Vec::new();
    for part in parts {
        if let Some(text) = part.get("text") {
            text.as_str()?;
            blocks.push(json!({"type":"text","text":text}));
        }
        if let Some(data) = part.get("inlineData").filter(|data| truthy(data)) {
            let mime = data["mimeType"].as_str()?;
            let data = data["data"].as_str()?;
            blocks.push(json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{data}")}}));
        }
        if let Some(call) = part.get("functionCall").filter(|call| truthy(call)) {
            let name = call["name"].as_str()?;
            let id = call_id(call, name)?;
            let arguments = json_payload(call.get("args"))?;
            calls.push(
                json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments}}),
            );
        }
        if let Some(response) = part.get("functionResponse").filter(|value| truthy(value)) {
            let name = response["name"].as_str()?;
            let id = call_id(response, name)?;
            let payload = response["response"]
                .get("result")
                .filter(|value| truthy(value))
                .or_else(|| response.get("response"));
            messages
                .push(json!({"role":"tool","tool_call_id":id,"content":json_payload(payload)?}));
        }
    }

    if calls.is_empty() {
        let role = if turn["role"] == "user" {
            "user"
        } else {
            "assistant"
        };
        let has_content = blocks.iter().any(|block| {
            block["type"] != "text"
                || block["text"]
                    .as_str()
                    .is_some_and(|text| !js_trim(text).is_empty())
        });
        if has_content {
            let content = if blocks.len() == 1 && blocks[0]["type"] == "text" {
                blocks.swap_remove(0)["text"].clone()
            } else {
                Value::Array(blocks)
            };
            messages.push(json!({"role":role,"content":content}));
        }
    } else {
        let mut message = json!({"role":"assistant"});
        if blocks.len() > 1 {
            message["content"] = Value::Array(blocks);
        } else if let Some(text) = blocks.first().and_then(|block| block.get("text")) {
            message["content"] = text.clone();
        }
        message["tool_calls"] = json!(calls);
        messages.push(message);
    }
    Some(())
}

fn call_id(call: &Value, name: &str) -> Option<String> {
    if let Some(id) = call.get("id").filter(|id| truthy(id)) {
        return Some(id.as_str()?.to_owned());
    }
    Some(format!("call_{name}"))
}

fn json_payload(value: Option<&Value>) -> Option<String> {
    match value.filter(|value| truthy(value)) {
        Some(value) => js_json(value, false),
        None => Some("{}".to_owned()),
    }
}
