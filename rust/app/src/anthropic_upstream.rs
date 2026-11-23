use std::collections::HashMap;

use serde_json::{Map, Value, json};

pub fn to_chat(value: Value) -> Value {
    to_chat_at(value, now_millis())
}

pub(crate) fn to_chat_at(value: Value, now: i64) -> Value {
    if truthy(&value["choices"]) || (truthy(&value["content"]) && !value["content"].is_array()) {
        return value;
    }

    let mut text = String::new();
    let mut thinking = String::new();
    let mut tools = Vec::new();
    for block in value["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => {
                text.push_str(strip_code_fences(
                    block["text"].as_str().unwrap_or_default(),
                ));
            }
            Some("thinking") if truthy(&block["thinking"]) => {
                thinking.push_str(&js_text(&block["thinking"]));
            }
            Some("tool_use") => {
                let mut call = Map::new();
                if let Some(id) = block.get("id") {
                    call.insert("id".into(), id.clone());
                }
                call.insert("type".into(), json!("function"));
                let mut function = Map::new();
                if let Some(name) = block.get("name") {
                    function.insert("name".into(), name.clone());
                }
                let mut args = if truthy(&block["input"]) {
                    block["input"].clone()
                } else {
                    json!({})
                };
                normalize_numbers(&mut args);
                function.insert("arguments".into(), json!(args.to_string()));
                call.insert("function".into(), Value::Object(function));
                tools.push(Value::Object(call));
            }
            _ => {}
        }
    }

    let mut message = json!({"role":"assistant"});
    if !text.is_empty() {
        message["content"] = json!(text);
    }
    if !thinking.is_empty() {
        message["reasoning_content"] = json!(thinking);
    }
    if !tools.is_empty() {
        message["tool_calls"] = Value::Array(tools);
    }
    if !truthy(&message["content"]) && !truthy(&message["tool_calls"]) {
        message["content"] = json!("");
    }

    let stop = &value["stop_reason"];
    let finish = match stop.as_str() {
        Some("end_turn") => json!("stop"),
        Some("tool_use") => json!("tool_calls"),
        _ if truthy(stop) => stop.clone(),
        _ => json!("stop"),
    };
    let id = if truthy(&value["id"]) {
        js_text(&value["id"])
    } else {
        now.to_string()
    };
    let model = if truthy(&value["model"]) {
        value["model"].clone()
    } else {
        json!("claude")
    };
    let mut result = json!({
        "id": format!("chatcmpl-{id}"),
        "object": "chat.completion",
        "created": now.div_euclid(1000),
        "model": model,
        "choices": [{"index":0, "message": message, "finish_reason": finish}],
    });

    if truthy(&value["usage"]) {
        let usage = &value["usage"];
        let input = token_or_zero(&usage["input_tokens"]);
        let output = token_or_zero(&usage["output_tokens"]);
        result["usage"] = json!({
            "prompt_tokens": input,
            "completion_tokens": output,
            "total_tokens": js_add(&input, &output),
        });
    }
    result
}

struct ToolState {
    index: usize,
    id: Option<Value>,
}

pub struct ClaudeToChat {
    now: fn() -> i64,
    message_id: Value,
    model: Option<Value>,
    next_tool: usize,
    tools: HashMap<Option<String>, ToolState>,
    server_tool_index: Option<String>,
    finish_sent: bool,
    finish_reason: Option<&'static str>,
    usage: Option<Value>,
}

impl Default for ClaudeToChat {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeToChat {
    pub fn new() -> Self {
        Self::with_clock(now_millis)
    }

    pub(crate) fn with_clock(now: fn() -> i64) -> Self {
        Self {
            now,
            message_id: Value::Null,
            model: Some(Value::Null),
            next_tool: 0,
            tools: HashMap::new(),
            server_tool_index: None,
            finish_sent: false,
            finish_reason: None,
            usage: None,
        }
    }

    pub(super) fn set_model(&mut self, model: &str) {
        self.model = Some(json!(model));
    }

    pub fn usage(&self) -> Option<&Value> {
        self.usage.as_ref()
    }

    pub fn push(&mut self, event: Option<&str>, chunk: &Value) -> Vec<Value> {
        if chunk.is_null() {
            return Vec::new();
        }
        let event = chunk["type"].as_str().or(event).unwrap_or_default();
        match event {
            "message_start" => {
                let message = &chunk["message"];
                self.message_id = if truthy(&message["id"]) {
                    message["id"].clone()
                } else {
                    json!(format!("msg_{}", (self.now)()))
                };
                self.model = message.get("model").cloned();
                self.next_tool = 0;
                self.set_usage(&message["usage"], false);
                vec![self.chunk(json!({"role":"assistant"}), None)]
            }
            "content_block_start" => self.start_block(chunk),
            "content_block_delta" => self.block_delta(chunk),
            "content_block_stop" => {
                if block_index(chunk) == self.server_tool_index {
                    self.server_tool_index = Some("-1".into());
                }
                Vec::new()
            }
            "message_delta" => self.message_delta(chunk),
            "message_stop" if !self.finish_sent => {
                let fallback = if self.tools.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                };
                let mut chunk = self.chunk(json!({}), Some(self.finish_reason.unwrap_or(fallback)));
                if let Some(usage) = &self.usage {
                    let input = token_or_zero(&usage["input_tokens"]);
                    let output = token_or_zero(&usage["output_tokens"]);
                    chunk["usage"] = json!({
                        "prompt_tokens": input,
                        "completion_tokens": output,
                        "total_tokens": js_add(&input, &output),
                    });
                }
                self.finish_sent = true;
                vec![chunk]
            }
            _ => Vec::new(),
        }
    }

    pub fn finish(&mut self) -> Vec<Value> {
        Vec::new()
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        let mut result = json!({
            "id": format!("chatcmpl-{}", js_text(&self.message_id)),
            "object": "chat.completion.chunk",
            "created": (self.now)().div_euclid(1000),
        });
        if let Some(model) = &self.model {
            result["model"] = model.clone();
        }
        result["choices"] = json!([{"index":0, "delta":delta, "finish_reason":finish}]);
        result
    }

    fn start_block(&mut self, chunk: &Value) -> Vec<Value> {
        let block = &chunk["content_block"];
        match block["type"].as_str() {
            Some("server_tool_use") => {
                self.server_tool_index = block_index(chunk);
                Vec::new()
            }
            Some("tool_use") => {
                let mut call = json!({"index": self.next_tool});
                let id = block.get("id").cloned();
                if let Some(id) = &id {
                    call["id"] = id.clone();
                }
                call["type"] = json!("function");
                let mut function = Map::new();
                if let Some(name) = block.get("name") {
                    function.insert("name".into(), name.clone());
                }
                function.insert("arguments".into(), json!(""));
                call["function"] = Value::Object(function);
                self.tools.insert(
                    block_index(chunk),
                    ToolState {
                        index: self.next_tool,
                        id,
                    },
                );
                self.next_tool += 1;
                vec![self.chunk(json!({"tool_calls":[call]}), None)]
            }
            _ => Vec::new(),
        }
    }

    fn block_delta(&self, chunk: &Value) -> Vec<Value> {
        if block_index(chunk) == self.server_tool_index {
            return Vec::new();
        }
        let delta = &chunk["delta"];
        let outgoing = match delta["type"].as_str() {
            Some("text_delta") if truthy(&delta["text"]) => json!({"content":delta["text"]}),
            Some("thinking_delta") if truthy(&delta["thinking"]) => {
                json!({"reasoning_content":delta["thinking"]})
            }
            Some("input_json_delta") if truthy(&delta["partial_json"]) => {
                let Some(tool) = self.tools.get(&block_index(chunk)) else {
                    return Vec::new();
                };
                let mut call = json!({"index":tool.index});
                if let Some(id) = &tool.id {
                    call["id"] = id.clone();
                }
                call["function"] = json!({"arguments":delta["partial_json"]});
                json!({"tool_calls":[call]})
            }
            _ => return Vec::new(),
        };
        vec![self.chunk(outgoing, None)]
    }

    fn message_delta(&mut self, chunk: &Value) -> Vec<Value> {
        self.set_usage(&chunk["usage"], true);
        let delta = &chunk["delta"];
        if !truthy(&delta["stop_reason"]) {
            return Vec::new();
        }

        let finish = match delta["stop_reason"].as_str() {
            Some("max_tokens") => "length",
            Some("tool_use") => "tool_calls",
            Some("refusal") => "content_filter",
            _ => "stop",
        };
        self.finish_reason = Some(finish);
        let mut output = Vec::new();
        if delta["stop_reason"] == "refusal" && truthy(&delta["stop_details"]["explanation"]) {
            output.push(self.chunk(
                json!({"content":delta["stop_details"]["explanation"]}),
                None,
            ));
        }
        let mut final_chunk = self.chunk(json!({}), Some(finish));
        if let Some(usage) = &self.usage {
            final_chunk["usage"] = canonical_usage(usage);
        }
        output.push(final_chunk);
        self.finish_sent = true;
        output
    }

    fn set_usage(&mut self, raw: &Value, delta: bool) {
        if !raw.is_object() && !raw.is_array() {
            return;
        }
        let fallback = |key: &str| {
            if delta {
                self.usage
                    .as_ref()
                    .and_then(|usage| usage[key].as_f64())
                    .unwrap_or_default()
            } else {
                0.0
            }
        };
        let input = raw["input_tokens"]
            .as_f64()
            .unwrap_or_else(|| fallback("input_tokens"));
        let read = raw["cache_read_input_tokens"]
            .as_f64()
            .unwrap_or_else(|| fallback("cache_read_input_tokens"));
        let create = raw["cache_creation_input_tokens"]
            .as_f64()
            .unwrap_or_else(|| fallback("cache_creation_input_tokens"));
        let output = if delta {
            raw["output_tokens"].as_f64().unwrap_or_default()
        } else {
            0.0
        };
        let prompt = input + read + create;
        let mut usage = json!({
            "prompt_tokens": json_number(prompt),
            "completion_tokens": json_number(output),
            "total_tokens": json_number(prompt + output),
            "input_tokens": json_number(input),
            "output_tokens": json_number(output),
        });
        if read > 0.0 {
            usage["cache_read_input_tokens"] = json_number(read);
        }
        if create > 0.0 {
            usage["cache_creation_input_tokens"] = json_number(create);
        }
        self.usage = Some(usage);
    }
}

fn canonical_usage(usage: &Value) -> Value {
    let input = usage["input_tokens"].as_f64().unwrap_or_default();
    let output = usage["output_tokens"].as_f64().unwrap_or_default();
    let read = usage["cache_read_input_tokens"]
        .as_f64()
        .unwrap_or_default();
    let create = usage["cache_creation_input_tokens"]
        .as_f64()
        .unwrap_or_default();
    let prompt = input + read + create;
    let mut result = json!({
        "prompt_tokens": json_number(prompt),
        "completion_tokens": json_number(output),
        "total_tokens": json_number(prompt + output),
    });
    if read > 0.0 || create > 0.0 {
        let mut details = Map::new();
        if read > 0.0 {
            details.insert("cached_tokens".into(), json_number(read));
        }
        if create > 0.0 {
            details.insert("cache_creation_tokens".into(), json_number(create));
        }
        result["prompt_tokens_details"] = Value::Object(details);
    }
    result
}

fn block_index(chunk: &Value) -> Option<String> {
    chunk.get("index").map(|value| {
        value
            .as_f64()
            .map_or_else(|| value.to_string(), |num| json_number(num).to_string())
    })
}

fn strip_code_fences(text: &str) -> &str {
    const FENCE: &str = "```";
    let mut body = text;
    let leading = text.trim_start_matches(js_whitespace);
    if let Some(open) = leading.strip_prefix(FENCE) {
        let trimmed = open.trim_start_matches(js_whitespace);
        if trimmed
            .get(..4)
            .is_some_and(|lang| lang.eq_ignore_ascii_case("json"))
        {
            body = trimmed[4..].trim_start_matches(js_whitespace);
        }
    }
    if let Some(end) = body.trim_end_matches(js_whitespace).strip_suffix(FENCE) {
        body = end.trim_end_matches(js_whitespace);
    }
    body
}

fn js_whitespace(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn token_or_zero(value: &Value) -> Value {
    if truthy(value) {
        value.clone()
    } else {
        json!(0)
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

fn js_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(_) => "[object Object]".into(),
        Value::Array(items) => items
            .iter()
            .map(|value| {
                if value.is_null() {
                    String::new()
                } else {
                    js_text(value)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), |n| n.to_string()),
        value => value.to_string(),
    }
}

fn js_add(left: &Value, right: &Value) -> Value {
    if left.is_string()
        || left.is_object()
        || left.is_array()
        || right.is_string()
        || right.is_object()
        || right.is_array()
    {
        json!(js_text(left) + &js_text(right))
    } else {
        let to_number = |value: &Value| {
            value.as_f64().unwrap_or_else(|| {
                if value == &Value::Bool(true) {
                    1.0
                } else {
                    0.0
                }
            })
        };
        json_number(to_number(left) + to_number(right))
    }
}

fn json_number(number: f64) -> Value {
    if number.is_finite() && number.fract() == 0.0 && number.abs() < 9_007_199_254_740_992.0 {
        json!(number as i64)
    } else {
        json!(number)
    }
}

fn normalize_numbers(value: &mut Value) {
    match value {
        Value::Number(number) if let Some(n) = number.as_f64() => *value = json_number(n),
        Value::Array(items) => items.iter_mut().for_each(normalize_numbers),
        Value::Object(items) => items.values_mut().for_each(normalize_numbers),
        _ => {}
    }
}
