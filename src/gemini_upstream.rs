use chrono::{DateTime, NaiveDate, NaiveDateTime};
use serde_json::{Map, Value, json};

use crate::chat_input::{js_json, truthy};

pub fn to_chat(value: Value) -> Value {
    to_chat_at(value, now_millis)
}

pub(crate) fn to_chat_at(value: Value, now: fn() -> i64) -> Value {
    let response = response(&value);
    let Some(candidate) = response["candidates"]
        .get(0)
        .filter(|candidate| truthy(candidate))
    else {
        return value;
    };
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tools = Vec::new();
    for part in candidate["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(part_text) = part.get("text") {
            if part["thought"] == true && truthy(part_text) {
                reasoning.push_str(&js_text(part_text));
            } else {
                text.push_str(&js_text(part_text));
            }
        }
        if truthy(&part["functionCall"]) {
            let call = &part["functionCall"];
            let name = call
                .get("name")
                .map_or_else(|| "undefined".to_owned(), js_text);
            let id = format!("call_{name}_{}_{}", now(), tools.len());
            tools.push(json!({
                "id": id,
                "type": "function",
                "function": function_fields(call),
            }));
        }
        if let Some(url) = image_url(part) {
            text.push_str(&format!("\n![image]({url})\n"));
        }
    }

    let mut message = json!({ "role": "assistant" });
    if !text.is_empty() {
        message["content"] = json!(text);
    }
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    let has_tools = !tools.is_empty();
    if has_tools {
        message["tool_calls"] = Value::Array(tools);
    }
    if !truthy(&message["content"]) && !has_tools {
        message["content"] = json!("");
    }

    let finish = text_or(&candidate["finishReason"], "stop").to_lowercase();
    let finish = if finish == "stop" && has_tools {
        "tool_calls"
    } else {
        &finish
    };
    let id = if truthy(&response["responseId"]) {
        js_text(&response["responseId"])
    } else {
        now().to_string()
    };
    let created = if truthy(&response["createTime"]) {
        parse_date(&response["createTime"]).map_or(Value::Null, |time| json!(time.div_euclid(1000)))
    } else {
        json!(now().div_euclid(1000))
    };
    let model = fallback(&response["modelVersion"], json!("gemini"));
    let mut result = json!({
        "id": format!("chatcmpl-{id}"),
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
    });
    if let Some(usage) = preferred_usage(&value) {
        let thought = token_or_zero(&usage["thoughtsTokenCount"]);
        let mut details = json!({
            "prompt_tokens": js_add(&token_or_zero(&usage["promptTokenCount"]), &thought),
            "completion_tokens": token_or_zero(&usage["candidatesTokenCount"]),
            "total_tokens": token_or_zero(&usage["totalTokenCount"]),
        });
        if numeric(&thought).is_some_and(|value| value > 0.0) {
            details["completion_tokens_details"] = json!({ "reasoning_tokens": thought });
        }
        result["usage"] = details;
    }
    result
}

pub fn raw_usage(value: &Value) -> Option<Value> {
    let usage = raw_usage_metadata(value)?;
    Some(json!({
        "prompt_tokens": token_or_zero(&usage["promptTokenCount"]),
        "completion_tokens": token_or_zero(&usage["candidatesTokenCount"]),
        "cached_tokens": token_or_zero(&usage["cachedContentTokenCount"]),
        "reasoning_tokens": token_or_zero(&usage["thoughtsTokenCount"]),
    }))
}

#[derive(Clone, Debug)]
pub struct ThoughtSignature {
    pub tool_call_id: String,
    pub signature: String,
    pub model: String,
}

pub struct GeminiToChat {
    now: fn() -> i64,
    message_id: Option<Value>,
    model: Value,
    function_index: usize,
    pending_signature: Option<String>,
    signatures: Vec<ThoughtSignature>,
    usage: Option<Value>,
}

impl GeminiToChat {
    pub fn new() -> Self {
        Self::with_clock("", now_millis)
    }

    pub fn with_model(model: &str) -> Self {
        let mut state = Self::new();
        state.model = json!(model);
        state
    }

    pub(crate) fn with_clock(model: &str, now: fn() -> i64) -> Self {
        Self {
            now,
            message_id: None,
            model: json!(model),
            function_index: 0,
            pending_signature: None,
            signatures: Vec::new(),
            usage: None,
        }
    }

    pub fn usage(&self) -> Option<&Value> {
        self.usage.as_ref()
    }

    pub fn take_thought_signatures(&mut self) -> Vec<ThoughtSignature> {
        std::mem::take(&mut self.signatures)
    }

    pub fn push(&mut self, _event: Option<&str>, chunk: &Value) -> Vec<Value> {
        let response = response(chunk);
        let Some(candidate) = response["candidates"]
            .get(0)
            .filter(|candidate| truthy(candidate))
        else {
            return Vec::new();
        };
        let mut frames = Vec::new();
        if self.message_id.is_none() {
            self.message_id = Some(if truthy(&response["responseId"]) {
                response["responseId"].clone()
            } else {
                json!(format!("msg_{}", (self.now)()))
            });
            self.model = fallback(
                &response["modelVersion"],
                fallback(&self.model, json!("gemini")),
            );
            frames.push(self.chunk(json!({ "role": "assistant" }), None));
        }
        for part in candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let signature = first_truthy([&part["thoughtSignature"], &part["thought_signature"]]);
            if let Some(signature) = signature.and_then(Value::as_str) {
                self.pending_signature = Some(signature.to_owned());
            }
            if let Some(text) = part.get("text").filter(|text| *text != "") {
                let delta = if part["thought"] == true {
                    json!({ "reasoning_content": text })
                } else {
                    json!({ "content": text })
                };
                frames.push(self.chunk(delta, None));
            }
            if truthy(&part["functionCall"]) {
                let pending = self.pending_signature.take();
                let signature = if let Some(signature) = signature {
                    signature.as_str()
                } else {
                    pending.as_deref()
                };
                frames.push(self.function_call(&part["functionCall"], signature));
            }
            if signature.is_none()
                && let Some(url) = image_url(part)
            {
                frames.push(self.chunk(
                    json!({
                        "images": [{ "type": "image_url", "image_url": { "url": url } }],
                    }),
                    None,
                ));
            }
        }

        if let Some(usage) = preferred_usage(chunk).and_then(stream_usage) {
            self.usage = Some(usage);
        }
        if truthy(&candidate["finishReason"]) {
            let reason = match js_text(&candidate["finishReason"])
                .to_ascii_uppercase()
                .as_str()
            {
                "STOP" if self.function_index > 0 => "tool_calls",
                "MAX_TOKENS" => "length",
                "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => "content_filter",
                _ => "stop",
            };
            let mut finished = self.chunk(json!({}), Some(reason));
            if let Some(usage) = &self.usage {
                finished["usage"] = usage.clone();
            }
            frames.push(finished);
        }
        frames
    }

    pub fn finish(&mut self) -> Vec<Value> {
        Vec::new()
    }

    fn function_call(&mut self, call: &Value, signature: Option<&str>) -> Value {
        let index = self.function_index;
        self.function_index += 1;
        let id = if let Some(id) = call.get("id").filter(|id| truthy(id)) {
            id.clone()
        } else {
            let name = call
                .get("name")
                .map_or_else(|| "undefined".to_owned(), js_text);
            json!(format!("{name}-{}-{index}", (self.now)()))
        };
        if let Some(signature) = signature
            && let Some(tool_call_id) = id.as_str()
        {
            self.signatures.push(ThoughtSignature {
                tool_call_id: tool_call_id.to_owned(),
                signature: signature.to_owned(),
                model: self.model.as_str().unwrap_or_default().to_owned(),
            });
        }
        self.chunk(
            json!({
                "tool_calls": [{
                    "id": id,
                    "index": index,
                    "type": "function",
                    "function": function_fields(call),
                }],
            }),
            None,
        )
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        let id = self
            .message_id
            .as_ref()
            .map_or_else(|| "null".to_owned(), js_text);
        json!({
            "id": format!("chatcmpl-{id}"),
            "object": "chat.completion.chunk",
            "created": (self.now)().div_euclid(1000),
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    }
}

fn response(chunk: &Value) -> &Value {
    chunk
        .get("response")
        .filter(|response| truthy(response))
        .unwrap_or(chunk)
}

fn preferred_usage(chunk: &Value) -> Option<&Value> {
    first_truthy([&response(chunk)["usageMetadata"], &chunk["usageMetadata"]])
}

fn raw_usage_metadata(chunk: &Value) -> Option<&Value> {
    first_truthy([&chunk["usageMetadata"], &chunk["response"]["usageMetadata"]])
}

fn stream_usage(usage: &Value) -> Option<Value> {
    if !usage.is_object() && !usage.is_array() {
        return None;
    }
    let token = |key: &str| usage[key].as_f64().unwrap_or_default();
    let prompt = token("promptTokenCount");
    let thoughts = token("thoughtsTokenCount");
    let total = token("totalTokenCount");
    let mut candidates = token("candidatesTokenCount");
    if candidates == 0.0 && total > 0.0 {
        candidates = (total - prompt - thoughts).max(0.0);
    }
    let mut result = json!({
        "prompt_tokens": json_number(prompt),
        "completion_tokens": json_number(candidates + thoughts),
        "total_tokens": json_number(total),
    });
    let cached = token("cachedContentTokenCount");
    if cached > 0.0 {
        result["prompt_tokens_details"] = json!({ "cached_tokens": json_number(cached) });
    }
    if thoughts > 0.0 {
        result["completion_tokens_details"] = json!({ "reasoning_tokens": json_number(thoughts) });
    }
    Some(result)
}

fn function_fields(call: &Value) -> Value {
    let mut function = Map::new();
    if let Some(name) = call.get("name") {
        function.insert("name".into(), name.clone());
    }
    let args = fallback(&call["args"], json!({}));
    let serialized = js_json(&args, false).unwrap_or_else(|| args.to_string());
    function.insert("arguments".into(), json!(serialized));
    Value::Object(function)
}

fn image_url(part: &Value) -> Option<String> {
    let image = first_truthy([&part["inlineData"], &part["inline_data"]])?;
    let data = image.get("data").filter(|data| truthy(data))?;
    let mime = first_truthy([&image["mimeType"], &image["mime_type"]])
        .map_or_else(|| "image/png".to_owned(), js_text);
    Some(format!("data:{mime};base64,{}", js_text(data)))
}

fn fallback(value: &Value, fallback: Value) -> Value {
    if truthy(value) {
        value.clone()
    } else {
        fallback
    }
}

fn first_truthy<const N: usize>(values: [&Value; N]) -> Option<&Value> {
    values.into_iter().find(|value| truthy(value))
}

fn text_or<'a>(value: &'a Value, fallback: &'a str) -> &'a str {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .unwrap_or(fallback)
}

fn token_or_zero(value: &Value) -> Value {
    fallback(value, json!(0))
}

fn numeric(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| match value {
        Value::String(text) => text.trim().parse().ok(),
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        _ => None,
    })
}

fn js_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(_) => "[object Object]".to_owned(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    js_text(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Number(number) => number
            .as_f64()
            .map_or_else(|| value.to_string(), |value| json_number(value).to_string()),
        _ => value.to_string(),
    }
}

fn js_add(left: &Value, right: &Value) -> Value {
    if left.is_string()
        || right.is_string()
        || left.is_object()
        || right.is_object()
        || left.is_array()
        || right.is_array()
    {
        json!(format!("{}{}", js_text(left), js_text(right)))
    } else {
        json_number(numeric(left).unwrap_or_default() + numeric(right).unwrap_or_default())
    }
}

fn json_number(value: f64) -> Value {
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value < -(i64::MIN as f64) {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn parse_date(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        if let Ok(time) =
            DateTime::parse_from_rfc3339(text).or_else(|_| DateTime::parse_from_rfc2822(text))
        {
            return Some(time.timestamp_millis());
        }
        if let Ok(time) = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f") {
            return Some(time.and_utc().timestamp_millis());
        }
        return NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .ok()
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .map(|time| time.and_utc().timestamp_millis());
    }
    numeric(value)
        .filter(|millis| millis.abs() <= 8_640_000_000_000_000.0)
        .map(|millis| millis as i64)
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
