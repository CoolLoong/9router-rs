use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::chat_input::js_json;
use crate::chat_output::{js_text, reasoning_text};
use crate::provider_connections::truthy;

pub(crate) fn request(original: &Value, model: &str) -> Option<Value> {
    let body = original
        .get("request")
        .filter(|body| truthy(body))
        .unwrap_or(original);
    body.as_object()?;
    let mut result = json!({"model":model,"stream":true});
    let config = &body["generationConfig"];
    if let Some(tokens) = config
        .get("maxOutputTokens")
        .filter(|tokens| truthy(tokens))
    {
        let minimum = body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty());
        result["max_tokens"] = match number(tokens) {
            Some(value) if value > 64_000.0 => json!(64_000),
            Some(value) if minimum && value < 32_000.0 => json!(32_000),
            _ => tokens.clone(),
        };
    }
    for (source, target) in [
        ("temperature", "temperature"),
        ("topP", "top_p"),
        ("topK", "top_k"),
    ] {
        if let Some(value) = config.get(source) {
            result[target] = value.clone();
        }
    }
    let budget = &config["thinkingConfig"]["thinkingBudget"];
    if truthy(budget) {
        let effort = match number(budget) {
            Some(value) if value <= 0.0 => None,
            Some(value) if value <= 2048.0 => Some("low"),
            Some(value) if value <= 16_384.0 => Some("medium"),
            _ => Some("high"),
        };
        if let Some(effort) = effort {
            result["reasoning_effort"] = json!(effort);
        }
    }

    let mut messages = Vec::new();
    let system = &body["systemInstruction"];
    let text = system.as_str().map_or_else(
        || {
            system["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").filter(|text| truthy(text)).map(js_text))
                .collect::<String>()
        },
        str::to_owned,
    );
    if !text.is_empty() {
        messages.push(json!({"role":"system","content":text}));
    }
    for content in body["contents"].as_array().into_iter().flatten() {
        append_content(content, &mut messages);
    }
    result["messages"] = Value::Array(messages);

    if let Some(tools) = body["tools"].as_array() {
        result["tools"] = Value::Array(tools.iter().flat_map(|tool| {
            tool["functionDeclarations"].as_array().into_iter().flatten()
        }).map(|function| {
            let mut parameters = function.get("parameters").filter(|value| truthy(value)).cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{}}));
            normalize_schema(&mut parameters);
            let mut definition = json!({
                "description":function.get("description").filter(|value| truthy(value)).cloned().unwrap_or(json!("")),
                "parameters":parameters,
            });
            if let Some(name) = function.get("name") {
                definition["name"] = name.clone();
            }
            json!({"type":"function","function":definition})
        }).collect());
    }
    Some(result)
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| match value {
        Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        Value::String(_) | Value::Array(_) => {
            let text = js_text(value);
            let text = text.trim();
            if text.is_empty() {
                Some(0.0)
            } else {
                text.parse().ok()
            }
        }
        _ => None,
    })
}

fn normalize_schema(schema: &mut Value) {
    let Some(fields) = schema.as_object_mut() else {
        return;
    };
    if let Some(value) = fields.get_mut("type")
        && let Some(name) = value.as_str()
    {
        *value = json!(name.to_lowercase());
    }
    fields.shift_remove("enumDescriptions");
    if let Some(properties) = fields.get_mut("properties").and_then(Value::as_object_mut) {
        for field in properties.values_mut() {
            normalize_schema(field);
        }
    }
    if let Some(items) = fields.get_mut("items") {
        normalize_schema(items);
    }
}

fn append_content(content: &Value, messages: &mut Vec<Value>) {
    let Some(parts) = content["parts"].as_array() else {
        return;
    };
    let before = messages.len();
    let mut text = Vec::new();
    let mut calls = Vec::new();
    let mut reasoning = String::new();
    for part in parts {
        let argument = &part["text"];
        if part["thought"] == true && truthy(argument) {
            reasoning.push_str(&js_text(argument));
            continue;
        }
        if truthy(&part["thoughtSignature"]) && part.get("text").is_some() {
            if truthy(argument) {
                text.push(json!({"type":"text","text":argument}));
            }
            continue;
        }
        if part.get("text").is_some() && argument != "" {
            text.push(json!({"type":"text","text":argument}));
        }
        if let Some(inline) = part.get("inlineData").filter(|data| truthy(data)) {
            let mime = property_text(inline, "mimeType");
            let data = property_text(inline, "data");
            text.push(json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{data}")}}));
        }
        if let Some(call) = part.get("functionCall").filter(|call| truthy(call)) {
            let mut function = json!({"arguments":json_text(&call["args"])});
            if let Some(name) = call.get("name") {
                function["name"] = name.clone();
            }
            calls.push(json!({"id":tool_id(call),"type":"function","function":function}));
        }
        if let Some(call) = part.get("functionResponse").filter(|call| truthy(call)) {
            let response = &call["response"];
            let result = response
                .get("result")
                .filter(|result| truthy(result))
                .unwrap_or(response);
            messages.push(
                json!({"role":"tool","tool_call_id":tool_id(call),"content":json_text(result)}),
            );
        }
    }
    if text.is_empty() && calls.is_empty() && reasoning.is_empty() {
        return;
    }
    let mut message = json!({});
    if before != messages.len() || !calls.is_empty() || content["role"] == "model" {
        message["role"] = json!("assistant");
    } else if let Some(role) = content.get("role") {
        message["role"] = role.clone();
    }
    if !text.is_empty() {
        message["content"] = if text.len() == 1 && text[0]["type"] == "text" {
            text.swap_remove(0)["text"].clone()
        } else {
            Value::Array(text)
        };
    }
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    messages.push(message);
}

fn tool_id(tool: &Value) -> Value {
    tool.get("id")
        .filter(|id| truthy(id))
        .cloned()
        .unwrap_or_else(|| json!(format!("call_{}", property_text(tool, "name"))))
}

fn property_text(value: &Value, key: &str) -> String {
    value.get(key).map_or_else(|| "undefined".into(), js_text)
}

fn json_text(value: &Value) -> String {
    if truthy(value) {
        js_json(value, false).unwrap_or_else(|| value.to_string())
    } else {
        "{}".into()
    }
}

#[derive(Default)]
struct Call {
    name: String,
    arguments: String,
}

#[derive(Default)]
pub(super) struct Stream {
    calls: BTreeMap<u64, Call>,
    response_id: Value,
    model_version: Value,
    usage: Value,
    ended: bool,
}

impl Stream {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn chunk(&mut self, chunk: Value, output: &mut Vec<u8>) {
        if self.ended {
            return;
        }
        let choice = &chunk["choices"][0];
        if !truthy(choice) {
            if truthy(&chunk["usage"]) {
                self.usage = chunk["usage"].clone();
            }
            return;
        }
        if !truthy(&self.response_id) {
            self.response_id = chunk
                .get("id")
                .filter(|id| truthy(id))
                .cloned()
                .unwrap_or_else(|| {
                    json!(format!("resp_{}", chrono::Utc::now().timestamp_millis()))
                });
        }
        if !truthy(&self.model_version) {
            self.model_version = chunk
                .get("model")
                .filter(|model| truthy(model))
                .cloned()
                .unwrap_or(json!(""));
        }
        let delta = &choice["delta"];
        let mut parts = Vec::new();
        let reasoning = reasoning_text(delta);
        if !reasoning.is_empty() {
            parts.push(json!({"thought":true,"text":reasoning}));
        }
        if truthy(&delta["content"]) {
            parts.push(json!({"text":delta["content"]}));
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = &call["index"];
            let index = index
                .as_u64()
                .or_else(|| index.as_str()?.parse().ok())
                .unwrap_or_default();
            let accumulated = self.calls.entry(index).or_default();
            let function = &call["function"];
            if truthy(&function["name"]) {
                accumulated.name.push_str(&js_text(&function["name"]));
            }
            if truthy(&function["arguments"]) {
                accumulated
                    .arguments
                    .push_str(&js_text(&function["arguments"]));
            }
        }
        let finish = &choice["finish_reason"];
        if truthy(finish) {
            parts.extend(self.calls.values().map(|call| {
                let args: Value =
                    serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
                json!({"functionCall":{"name":call.name,"args":args}})
            }));
            if parts.is_empty() {
                parts.push(json!({"text":""}));
            }
        }
        if parts.is_empty() {
            return;
        }
        let mut candidate = json!({"content":{"role":"model","parts":parts}});
        if truthy(finish) {
            candidate["finishReason"] = json!(match finish.as_str() {
                Some("length") => "MAX_TOKENS",
                Some("content_filter") => "SAFETY",
                _ => "STOP",
            });
        }
        let mut response = json!({"candidates":[candidate],"modelVersion":self.model_version,"responseId":self.response_id});
        let usage = chunk
            .get("usage")
            .filter(|usage| truthy(usage))
            .unwrap_or(&self.usage);
        if truthy(usage) {
            let mut counts = json!({});
            for (source, target) in [
                ("prompt_tokens", "promptTokenCount"),
                ("completion_tokens", "candidatesTokenCount"),
                ("total_tokens", "totalTokenCount"),
            ] {
                counts[target] = usage
                    .get(source)
                    .filter(|value| truthy(value))
                    .cloned()
                    .unwrap_or(json!(0));
            }
            for (details, source, target) in [
                (
                    "completion_tokens_details",
                    "reasoning_tokens",
                    "thoughtsTokenCount",
                ),
                (
                    "prompt_tokens_details",
                    "cached_tokens",
                    "cachedContentTokenCount",
                ),
            ] {
                if let Some(value) = usage[details].get(source).filter(|value| truthy(value)) {
                    counts[target] = value.clone();
                }
            }
            response["usageMetadata"] = counts;
        }
        output.extend_from_slice(b"data: ");
        output.extend_from_slice(json!({"response":response}).to_string().as_bytes());
        output.extend_from_slice(b"\n\n");
    }

    pub(super) fn finish(&mut self, _output: &mut Vec<u8>) {
        self.ended = true;
    }
}
