use std::collections::HashMap;

use serde_json::{Value, json};

struct ToolBlock {
    index: usize,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub struct ClaudeSse {
    started: bool,
    terminal: bool,
    closed: bool,
    next_block: usize,
    thinking: Option<usize>,
    text: Option<usize>,
    tool_order: Vec<ToolBlock>,
    tool_indexes: HashMap<String, usize>,
    usage: Option<Value>,
}

impl ClaudeSse {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn usage(&self) -> Option<&Value> {
        self.usage.as_ref()
    }

    pub fn set_usage(&mut self, usage: Option<Value>) {
        self.usage = usage;
    }

    pub fn finished(&self) -> bool {
        self.terminal
    }

    pub fn push(&mut self, chunk: &Value) -> Vec<Value> {
        let mut output = Vec::new();
        let choice = &chunk["choices"][0];
        if self.closed || self.terminal || !truthy(choice) {
            return output;
        }
        if chunk["usage"].is_object() || chunk["usage"].is_array() {
            self.usage = Some(claude_usage(&chunk["usage"]));
        }

        if !self.started {
            self.started = true;
            output.push(json!({
                "type": "message_start",
                "message": {
                    "id": message_id(chunk),
                    "type": "message",
                    "role": "assistant",
                    "model": if truthy(&chunk["model"]) { chunk["model"].clone() } else { json!("unknown") },
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 },
                },
            }));
        }

        let delta = &choice["delta"];
        let reasoning = reasoning_text(delta);
        if !reasoning.is_empty() {
            stop_block(&mut self.text, &mut output);
            let index = self.thinking.unwrap_or_else(|| {
                let index = self.alloc();
                self.thinking = Some(index);
                output.push(block_start(
                    index,
                    json!({"type":"thinking", "thinking":""}),
                ));
                index
            });
            output.push(block_delta(
                index,
                json!({"type":"thinking_delta", "thinking":reasoning}),
            ));
        }
        if truthy(&delta["content"]) {
            stop_block(&mut self.thinking, &mut output);
            let index = self.text.unwrap_or_else(|| {
                let index = self.alloc();
                self.text = Some(index);
                output.push(block_start(index, json!({"type":"text", "text":""})));
                index
            });
            output.push(block_delta(
                index,
                json!({"type":"text_delta", "text":delta["content"]}),
            ));
        }
        for tool in delta["tool_calls"].as_array().into_iter().flatten() {
            self.push_tool(tool, &mut output);
        }
        if truthy(&choice["finish_reason"]) {
            self.terminal = true;
            stop_block(&mut self.thinking, &mut output);
            stop_block(&mut self.text, &mut output);
            for block in &self.tool_order {
                if !block.arguments.is_empty() {
                    output.push(block_delta(
                        block.index,
                        json!({
                            "type":"input_json_delta",
                            "partial_json": sanitize_arguments(&block.name, &block.arguments),
                        }),
                    ));
                }
                output.push(block_stop(block.index));
            }
            output.push(json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason(choice["finish_reason"].as_str()) },
                "usage": self.usage.clone().unwrap_or_else(|| json!({"input_tokens":0, "output_tokens":0})),
            }));
            output.push(json!({"type":"message_stop"}));
        }
        output
    }

    pub fn finish(&mut self) -> Vec<Value> {
        self.closed = true;
        Vec::new()
    }

    fn alloc(&mut self) -> usize {
        let index = self.next_block;
        self.next_block += 1;
        index
    }

    fn push_tool(&mut self, tool: &Value, output: &mut Vec<Value>) {
        let key = tool_key(&tool["index"]);
        let slot = match self.tool_indexes.get(&key).copied() {
            Some(slot) => slot,
            None if truthy(&tool["id"]) => {
                stop_block(&mut self.thinking, output);
                stop_block(&mut self.text, output);
                let index = self.alloc();
                let name = tool["function"]["name"].as_str().unwrap_or_default();
                output.push(block_start(
                    index,
                    json!({
                        "type":"tool_use",
                        "id": tool["id"],
                        "name": name.strip_prefix("proxy_").unwrap_or(name),
                        "input": {},
                    }),
                ));
                let slot = self.tool_order.len();
                self.tool_indexes.insert(key, slot);
                self.tool_order.push(ToolBlock {
                    index,
                    name: name.to_owned(),
                    arguments: String::new(),
                });
                slot
            }
            None => return,
        };
        let arguments = &tool["function"]["arguments"];
        if truthy(arguments) {
            self.tool_order[slot]
                .arguments
                .push_str(&js_text(arguments));
        }
    }
}

fn message_id(chunk: &Value) -> Value {
    let id = chunk["id"]
        .as_str()
        .map(|s| s.replacen("chatcmpl-", "", 1))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(generated_message_id);
    if id == "chat" || id.encode_utf16().count() < 8 {
        for key in ["requestId", "traceId"] {
            let alternate = &chunk["extend_fields"][key];
            if truthy(alternate) {
                return alternate.clone();
            }
        }
        json!(generated_message_id())
    } else {
        json!(id)
    }
}

fn generated_message_id() -> String {
    format!("msg_{}", chrono::Utc::now().timestamp_millis())
}

fn block_start(index: usize, content: Value) -> Value {
    json!({"type":"content_block_start", "index":index, "content_block":content})
}

fn block_delta(index: usize, delta: Value) -> Value {
    json!({"type":"content_block_delta", "index":index, "delta":delta})
}

fn block_stop(index: usize) -> Value {
    json!({"type":"content_block_stop", "index":index})
}

fn stop_block(block: &mut Option<usize>, output: &mut Vec<Value>) {
    if let Some(index) = block.take() {
        output.push(block_stop(index));
    }
}

fn stop_reason(reason: Option<&str>) -> &str {
    match reason {
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        Some("content_filter") => "refusal",
        _ => "end_turn",
    }
}

fn claude_usage(usage: &Value) -> Value {
    let details = &usage["prompt_tokens_details"];
    let prompt = usage["prompt_tokens"].as_f64().unwrap_or(0.0);
    let completion = usage["completion_tokens"].as_f64().unwrap_or(0.0);
    let read = details["cached_tokens"].as_f64().unwrap_or(0.0);
    let create = details["cache_creation_tokens"].as_f64().unwrap_or(0.0);
    let mut result = json!({
        "input_tokens": json_number(prompt - read - create),
        "output_tokens": json_number(completion),
    });
    if read > 0.0 {
        result["cache_read_input_tokens"] = json_number(read);
    }
    if create > 0.0 {
        result["cache_creation_input_tokens"] = json_number(create);
    }
    result
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn pdf_pages(path: &Value, pages: &Value) -> bool {
    path.as_str()
        .is_some_and(|path| path.to_ascii_lowercase().ends_with(".pdf"))
        && pages
            .as_str()
            .is_some_and(|pages| match pages.split_once('-') {
                Some((lo, hi)) => digits(lo) && digits(hi),
                None => digits(pages),
            })
}

fn fix_read_arguments(args: &mut Value) {
    let Some(args) = args.as_object_mut() else {
        return;
    };
    for key in ["limit", "offset"] {
        if let Some(args) = args.get_mut(key)
            && let Some(text) = args.as_str()
            && digits(if key == "offset" {
                text.strip_prefix('-').unwrap_or(text)
            } else {
                text
            })
            && let Ok(number) = text.parse::<f64>()
        {
            *args = if key == "limit" && number > 2000.0 {
                json!(2000)
            } else if key == "offset" && number < 0.0 {
                json!(0)
            } else {
                json_number(number)
            };
        }
    }
    if let Some(limit) = args.get_mut("limit")
        && let Some(number) = limit.as_f64()
    {
        if number > 2000.0 {
            *limit = json!(2000);
        } else if number < 1.0 {
            args.shift_remove("limit");
        }
    }
    if let Some(offset) = args.get_mut("offset")
        && offset.as_f64().is_some_and(|number| number < 0.0)
    {
        *offset = json!(0);
    }
    if let Some(pages) = args.get("pages")
        && !args
            .get("file_path")
            .is_some_and(|path| pdf_pages(path, pages))
    {
        args.shift_remove("pages");
    }
}

fn sanitize_arguments(name: &str, json: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(json) else {
        return json.to_owned();
    };
    if name.strip_prefix("proxy_").unwrap_or(name) == "Read" {
        if !value.is_object() && !value.is_array() {
            return json.to_owned();
        }
        fix_read_arguments(&mut value);
    }
    normalize_numbers(&mut value);
    value.to_string()
}

fn reasoning_text(delta: &Value) -> String {
    for key in ["reasoning_content", "reasoning"] {
        if let Some(text) = delta[key].as_str().filter(|s| !s.is_empty()) {
            return text.to_owned();
        }
    }
    delta["reasoning_details"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|part| {
            if let Some(text) = part.as_str() {
                text.to_owned()
            } else {
                ["text", "content"]
                    .into_iter()
                    .find_map(|key| truthy(&part[key]).then(|| js_text(&part[key])))
                    .unwrap_or_default()
            }
        })
        .collect()
}

fn tool_key(id: &Value) -> String {
    if id.is_null() {
        "0".to_owned()
    } else if let Some(number) = id.as_f64() {
        json_number(number).to_string()
    } else {
        id.to_string()
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
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
