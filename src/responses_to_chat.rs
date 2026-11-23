use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::chat_input::js_json;
use crate::chat_output::{js_add, js_text};
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;

#[derive(Hash, PartialEq, Eq)]
enum ToolKey {
    String(String),
    Number(u64),
    True,
}

impl ToolKey {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(text) if !text.is_empty() => Some(Self::String(text.to_owned())),
            Value::Number(number) => number
                .as_f64()
                .filter(|value| *value != 0.0)
                .map(|value| Self::Number(value.to_bits())),
            Value::Bool(true) => Some(Self::True),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct ResponsesToChat {
    model: String,
    started: bool,
    finished: bool,
    id: String,
    created: i64,
    next_tool: usize,
    tool_index: HashMap<ToolKey, usize>,
    arguments_emitted: HashSet<usize>,
    usage: Option<Value>,
}

impl ResponsesToChat {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_model(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            ..Self::new()
        }
    }

    pub fn usage(&self) -> Option<&Value> {
        self.usage.as_ref()
    }

    pub fn push(&mut self, event: Option<&str>, data: &Value) -> Vec<Value> {
        if !truthy(data) {
            return Vec::new();
        }
        if !self.started {
            let now = chrono::Utc::now();
            self.started = true;
            self.id = format!("chatcmpl-{}", now.timestamp_millis());
            self.created = now.timestamp();
        }

        let kind = first_truthy([&data["type"], &data["event"]])
            .map(|kind| kind.as_str().unwrap_or_default())
            .or(event)
            .unwrap_or_default();
        let payload = first_truthy([&data["data"]]).unwrap_or(data);
        let item = &payload["item"];
        let tool = matches!(
            item["type"].as_str(),
            Some("function_call" | "custom_tool_call")
        );
        let chunk = match kind {
            "response.output_text.delta" => self.text_delta("content", &payload["delta"]),
            "response.reasoning_summary_text.delta" => {
                self.text_delta("reasoning_content", &payload["delta"])
            }
            "response.output_item.added" if tool => Some(self.tool_added(payload, item)),
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                self.tool_arguments_delta(payload)
            }
            "response.output_item.done" if tool => self.tool_done(payload, item),
            "response.completed" | "response.done" => {
                if let Some(usage) = completion_usage(&payload["response"]["usage"]) {
                    self.usage = Some(usage);
                }
                self.finish_chunk()
            }
            "error" | "response.failed" => self.error_chunk(payload),
            _ => None,
        };
        chunk.into_iter().collect()
    }

    pub fn finish(&mut self) -> Vec<Value> {
        self.finish_chunk().into_iter().collect()
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        let model = if self.model.is_empty() {
            "unknown"
        } else {
            &self.model
        };
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    }

    fn text_delta(&self, field: &str, text: &Value) -> Option<Value> {
        truthy(text).then(|| self.chunk(json!({ (field): text }), None))
    }

    fn tool_added(&mut self, payload: &Value, item: &Value) -> Value {
        let id = first_truthy([&item["call_id"]])
            .cloned()
            .unwrap_or_else(|| json!(format!("call_{}", chrono::Utc::now().timestamp_millis())));
        let key =
            first_truthy([&item["id"], &payload["item_id"], &id]).and_then(ToolKey::from_value);
        let known = key
            .as_ref()
            .and_then(|key| self.tool_index.get(key))
            .copied();
        let index = known.unwrap_or_else(|| {
            let index = self.next_tool;
            self.next_tool += 1;
            if let Some(key) = key {
                self.tool_index.insert(key, index);
            }
            index
        });
        let name = first_truthy([&item["name"]])
            .cloned()
            .unwrap_or_else(|| json!(""));
        self.chunk(
            json!({
                "tool_calls": [{
                    "index": index, "id": id, "type": "function",
                    "function": { "name": name, "arguments": "" },
                }],
            }),
            None,
        )
    }

    fn index_for_item(&self, item_id: Option<&Value>) -> usize {
        item_id
            .and_then(ToolKey::from_value)
            .and_then(|key| self.tool_index.get(&key))
            .copied()
            .unwrap_or_else(|| self.next_tool.saturating_sub(1))
    }

    fn tool_arguments_delta(&mut self, payload: &Value) -> Option<Value> {
        let delta = first_truthy([&payload["delta"]])?;
        let index = self.index_for_item(payload.get("item_id"));
        self.arguments_emitted.insert(index);
        Some(self.argument_chunk(index, delta))
    }

    fn tool_done(&mut self, payload: &Value, item: &Value) -> Option<Value> {
        let arguments = item.get("arguments")?;
        arguments.as_str().filter(|text| !text.is_empty())?;
        let index = self.index_for_item(first_truthy([&item["id"], &payload["item_id"]]));
        self.arguments_emitted
            .insert(index)
            .then(|| self.argument_chunk(index, arguments))
    }

    fn argument_chunk(&self, index: usize, arguments: &Value) -> Value {
        self.chunk(
            json!({ "tool_calls": [{ "index": index, "function": { "arguments": arguments } }] }),
            None,
        )
    }

    fn finish_chunk(&mut self) -> Option<Value> {
        if self.finished || !self.started {
            return None;
        }
        self.finished = true;
        let reason = if self.next_tool == 0 {
            "stop"
        } else {
            "tool_calls"
        };
        let mut chunk = self.chunk(json!({}), Some(reason));
        if let Some(usage) = &self.usage {
            chunk["usage"] = usage.clone();
        }
        Some(chunk)
    }

    fn error_chunk(&mut self, payload: &Value) -> Option<Value> {
        if self.finished {
            return None;
        }
        let error = first_truthy([&payload["error"], &payload["response"]["error"]])?;
        let message = first_truthy([&error["message"]])
            .map(js_text)
            .unwrap_or_else(|| js_json(error, false).unwrap_or_else(|| error.to_string()));
        self.finished = true;
        Some(self.chunk(
            json!({ "content": format!("[Error] {message}") }),
            Some("stop"),
        ))
    }
}

fn first_truthy<const N: usize>(values: [&Value; N]) -> Option<&Value> {
    values.into_iter().find(|value| truthy(value))
}

fn token_positive(value: &Value) -> bool {
    value == true
        || value
            .as_f64()
            .or_else(|| js_trim(&js_text(value)).parse().ok())
            .is_some_and(|number| number > 0.0)
}

fn completion_usage(usage: &Value) -> Option<Value> {
    if !(usage.is_object() || usage.is_array()) {
        return None;
    }
    let zero = json!(0);
    let input = first_truthy([&usage["input_tokens"], &usage["prompt_tokens"]]).unwrap_or(&zero);
    let output =
        first_truthy([&usage["output_tokens"], &usage["completion_tokens"]]).unwrap_or(&zero);
    let mut result = json!({
        "prompt_tokens": input, "completion_tokens": output,
        "total_tokens": js_add(input, output),
    });
    let cached = first_truthy([
        &usage["input_tokens_details"]["cached_tokens"],
        &usage["cache_read_input_tokens"],
    ]);
    if let Some(cached) = cached.filter(|value| token_positive(value)) {
        result["prompt_tokens_details"] = json!({ "cached_tokens": cached });
    }
    Some(result)
}
