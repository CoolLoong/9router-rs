use std::collections::{BTreeMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

pub struct ResponsesSse {
    response_id: String,
    created: u64,
    sequence: u64,
    started: bool,
    completion_pending: bool,
    await_usage_trailer: bool,
    completed: bool,
    messages: BTreeMap<i64, Message>,
    reasoning: Reasoning,
    tools: BTreeMap<i64, ToolCall>,
    custom_tool_names: HashSet<String>,
    output: BTreeMap<i64, Value>,
    usage: Option<Value>,
}

#[derive(Default)]
struct Message {
    text: String,
    closed: bool,
}

struct Reasoning {
    id: String,
    index: i64,
    text: String,
    closed: bool,
    in_think_tag: bool,
}

#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
    added: bool,
    closed: bool,
}

impl Default for ResponsesSse {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesSse {
    pub fn new() -> Self {
        Self::with_custom_tools(std::iter::empty())
    }

    pub fn with_custom_tools(names: impl IntoIterator<Item = String>) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            response_id: format!("resp_{}", now.as_millis()),
            created: now.as_secs(),
            sequence: 0,
            started: false,
            completion_pending: false,
            await_usage_trailer: true,
            completed: false,
            messages: BTreeMap::new(),
            reasoning: Reasoning {
                id: String::new(),
                index: -1,
                text: String::new(),
                closed: false,
                in_think_tag: false,
            },
            tools: BTreeMap::new(),
            custom_tool_names: names.into_iter().collect(),
            output: BTreeMap::new(),
            usage: None,
        }
    }

    pub fn without_usage_wait(mut self) -> Self {
        self.await_usage_trailer = false;
        self
    }

    pub fn completion_pending(&self) -> bool {
        self.completion_pending && !self.completed
    }

    pub fn is_completed(&self) -> bool {
        self.completed
    }

    pub fn push(&mut self, chunk: &Value) -> Vec<String> {
        if let Some(usage) = responses_usage(&chunk["usage"]) {
            self.usage = Some(usage);
        }
        let Some(choice) = chunk["choices"].get(0) else {
            return if self.completion_pending() && self.usage.is_some() {
                self.finish()
            } else {
                Vec::new()
            };
        };

        let mut frames = Vec::new();
        if !self.started {
            self.start(chunk, &mut frames);
        }
        let index = integer(&choice["index"]).unwrap_or(0);
        let delta = &choice["delta"];
        let reasoning = reasoning_text(delta);
        if !reasoning.is_empty() {
            self.start_reasoning(index, &mut frames);
            self.reasoning_delta(&reasoning, &mut frames);
        }
        if let Some(content) = nonempty_string(&delta["content"]) {
            let cleaned;
            let mut content = content;
            if content.contains("<think>") {
                cleaned = content.replacen("<think>", "", 1);
                content = &cleaned;
                self.reasoning.in_think_tag = true;
                self.start_reasoning(index, &mut frames);
            }
            if let Some((reasoning, rest)) = content.split_once("</think>") {
                self.reasoning_delta(reasoning, &mut frames);
                self.close_reasoning(&mut frames);
                self.reasoning.in_think_tag = false;
                content = rest;
            }
            if !content.is_empty() {
                if self.reasoning.in_think_tag {
                    self.reasoning_delta(content, &mut frames);
                    return frames;
                }
                self.close_reasoning(&mut frames);
                self.text_delta(index, content, &mut frames);
            }
        }

        if let Some(calls) = delta["tool_calls"]
            .as_array()
            .filter(|calls| !calls.is_empty())
        {
            self.close_reasoning(&mut frames);
            self.close_message(index, &mut frames);
            for call in calls {
                self.tool_delta(call, &mut frames);
            }
        }

        if truthy(&choice["finish_reason"]) {
            self.close_items(&mut frames);
            if self.usage.is_some() || !self.await_usage_trailer {
                self.complete(&mut frames);
            } else {
                self.completion_pending = true;
            }
        }
        frames
    }

    pub fn finish(&mut self) -> Vec<String> {
        if self.completed {
            return Vec::new();
        }
        let mut frames = Vec::new();
        self.close_items(&mut frames);
        self.complete(&mut frames);
        frames
    }

    fn start(&mut self, chunk: &Value, frames: &mut Vec<String>) {
        self.started = true;
        if truthy(&chunk["id"]) {
            self.response_id = format!("resp_{}", js_text(&chunk["id"]));
        }
        self.emit(
            frames,
            "response.created",
            json!({
                "response": {
                    "id": self.response_id,
                    "object": "response",
                    "created_at": self.created,
                    "status": "in_progress",
                    "background": false,
                    "error": null,
                    "output": [],
                },
            }),
        );
        self.emit(
            frames,
            "response.in_progress",
            json!({
                "response": {
                    "id": self.response_id,
                    "object": "response",
                    "created_at": self.created,
                    "status": "in_progress",
                },
            }),
        );
    }

    fn start_reasoning(&mut self, index: i64, frames: &mut Vec<String>) {
        if !self.reasoning.id.is_empty() {
            return;
        }
        self.reasoning.id = format!("rs_{}_{index}", self.response_id);
        self.reasoning.index = index;
        self.emit(
            frames,
            "response.output_item.added",
            json!({
                "output_index": index,
                "item": { "id": self.reasoning.id, "type": "reasoning", "summary": [] },
            }),
        );
        self.emit(
            frames,
            "response.reasoning_summary_part.added",
            json!({
                "item_id": self.reasoning.id,
                "output_index": index,
                "summary_index": 0,
                "part": summary_part(""),
            }),
        );
    }

    fn reasoning_delta(&mut self, text: &str, frames: &mut Vec<String>) {
        if text.is_empty() {
            return;
        }
        self.reasoning.text.push_str(text);
        self.emit(
            frames,
            "response.reasoning_summary_text.delta",
            json!({
                "item_id": self.reasoning.id,
                "output_index": self.reasoning.index,
                "summary_index": 0,
                "delta": text,
            }),
        );
    }

    fn close_reasoning(&mut self, frames: &mut Vec<String>) {
        if self.reasoning.id.is_empty() || self.reasoning.closed {
            return;
        }
        self.reasoning.closed = true;
        let index = self.reasoning.index;
        self.emit(
            frames,
            "response.reasoning_summary_text.done",
            json!({
                "item_id": self.reasoning.id,
                "output_index": index,
                "summary_index": 0,
                "text": self.reasoning.text,
            }),
        );
        self.emit(
            frames,
            "response.reasoning_summary_part.done",
            json!({
                "item_id": self.reasoning.id,
                "output_index": index,
                "summary_index": 0,
                "part": summary_part(&self.reasoning.text),
            }),
        );
        let item = json!({
            "id": self.reasoning.id,
            "type": "reasoning",
            "summary": [summary_part(&self.reasoning.text)],
        });
        self.close_item(index, item, frames);
    }

    fn text_delta(&mut self, index: i64, text: &str, frames: &mut Vec<String>) {
        let id = format!("msg_{}_{index}", self.response_id);
        let first = !self.messages.contains_key(&index);
        self.messages.entry(index).or_default().text.push_str(text);
        if first {
            self.emit(
                frames,
                "response.output_item.added",
                json!({
                    "output_index": index,
                    "item": message_item(&id, Vec::new()),
                }),
            );
            self.emit(
                frames,
                "response.content_part.added",
                json!({
                    "item_id": id,
                    "output_index": index,
                    "content_index": 0,
                    "part": text_part(""),
                }),
            );
        }
        self.emit(
            frames,
            "response.output_text.delta",
            json!({
                "item_id": id,
                "output_index": index,
                "content_index": 0,
                "delta": text,
                "logprobs": [],
            }),
        );
    }

    fn close_message(&mut self, index: i64, frames: &mut Vec<String>) {
        let Some(message) = self
            .messages
            .get_mut(&index)
            .filter(|message| !message.closed)
        else {
            return;
        };
        message.closed = true;
        let id = format!("msg_{}_{index}", self.response_id);
        let done = json!({
            "item_id": id,
            "output_index": index,
            "content_index": 0,
            "text": message.text,
            "logprobs": [],
        });
        let part = text_part(&message.text);
        self.emit(frames, "response.output_text.done", done);
        self.emit(
            frames,
            "response.content_part.done",
            json!({
                "item_id": id,
                "output_index": index,
                "content_index": 0,
                "part": part,
            }),
        );
        self.close_item(index, message_item(&id, vec![part]), frames);
    }

    fn tool_delta(&mut self, chunk: &Value, frames: &mut Vec<String>) {
        let index = integer(&chunk["index"]).unwrap_or(0);
        let call = self.tools.entry(index).or_default();
        let function = &chunk["function"];
        if let Some(name) = nonempty_string(&function["name"]) {
            call.name = name.to_owned();
        }
        if let Some(id) = nonempty_string(&chunk["id"]) {
            call.id = id.to_owned();
        }
        let custom = !call.name.is_empty() && self.custom_tool_names.contains(&call.name);
        let added = if !call.added && !call.id.is_empty() && !call.name.is_empty() {
            call.added = true;
            Some(tool_item(call, custom, ""))
        } else {
            None
        };
        let delta = if let Some(arguments) = nonempty_string(&function["arguments"]) {
            call.arguments.push_str(arguments);
            (call.added && !call.id.is_empty() && !custom).then(|| {
                json!({
                    "item_id": format!("fc_{}", call.id),
                    "output_index": index,
                    "delta": arguments,
                })
            })
        } else {
            None
        };
        if let Some(item) = added {
            self.emit(
                frames,
                "response.output_item.added",
                json!({ "output_index": index, "item": item }),
            );
        }
        if let Some(delta) = delta {
            self.emit(frames, "response.function_call_arguments.delta", delta);
        }
    }

    fn close_tool(&mut self, index: i64, frames: &mut Vec<String>) {
        let Some(call) = self
            .tools
            .get_mut(&index)
            .filter(|call| !call.closed && !call.id.is_empty())
        else {
            return;
        };
        call.closed = true;
        let custom = !call.name.is_empty() && self.custom_tool_names.contains(&call.name);
        let arguments = if call.arguments.is_empty() {
            "{}"
        } else {
            &call.arguments
        };
        let input = if custom {
            custom_tool_input(arguments)
        } else {
            arguments.to_owned()
        };
        let item = tool_item(call, custom, &input);
        let id = &item["id"];
        if custom {
            self.emit(
                frames,
                "response.custom_tool_call_input.delta",
                json!({
                    "item_id": id,
                    "output_index": index,
                    "delta": input,
                }),
            );
            self.emit(
                frames,
                "response.custom_tool_call_input.done",
                json!({
                    "item_id": id,
                    "output_index": index,
                    "input": input,
                }),
            );
        } else {
            self.emit(
                frames,
                "response.function_call_arguments.done",
                json!({
                    "item_id": id,
                    "output_index": index,
                    "arguments": input,
                }),
            );
        }
        self.close_item(index, item, frames);
    }

    fn close_item(&mut self, index: i64, item: Value, frames: &mut Vec<String>) {
        self.emit(
            frames,
            "response.output_item.done",
            json!({ "output_index": index, "item": item }),
        );
        self.output.insert(index, item);
    }

    fn close_items(&mut self, frames: &mut Vec<String>) {
        for index in self.messages.keys().copied().collect::<Vec<_>>() {
            self.close_message(index, frames);
        }
        self.close_reasoning(frames);
        for index in self.tools.keys().copied().collect::<Vec<_>>() {
            self.close_tool(index, frames);
        }
    }

    fn complete(&mut self, frames: &mut Vec<String>) {
        if self.completed {
            return;
        }
        self.completed = true;
        self.completion_pending = false;
        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created,
            "status": "completed",
            "background": false,
            "error": null,
            "output": self.output.values().collect::<Vec<_>>(),
        });
        if let Some(usage) = &self.usage {
            response["usage"] = usage.clone();
        }
        self.emit(
            frames,
            "response.completed",
            json!({ "response": response }),
        );
    }

    fn emit(&mut self, frames: &mut Vec<String>, event: &str, fields: Value) {
        self.sequence += 1;
        let mut data = Map::from_iter([("type".into(), json!(event))]);
        if let Value::Object(fields) = fields {
            data.extend(fields);
        }
        data.insert("sequence_number".into(), json!(self.sequence));
        frames.push(format!("event: {event}\ndata: {}\n\n", Value::Object(data)));
    }
}

fn summary_part(text: &str) -> Value {
    json!({ "type": "summary_text", "text": text })
}

fn text_part(text: &str) -> Value {
    json!({ "type": "output_text", "annotations": [], "logprobs": [], "text": text })
}

fn message_item(id: &str, parts: Vec<Value>) -> Value {
    json!({ "id": id, "type": "message", "content": parts, "role": "assistant" })
}

fn tool_item(call: &ToolCall, custom: bool, value: &str) -> Value {
    if custom {
        json!({
            "id": format!("ctc_{}", call.id),
            "type": "custom_tool_call",
            "input": value,
            "call_id": call.id,
            "name": call.name,
        })
    } else {
        json!({
            "id": format!("fc_{}", call.id),
            "type": "function_call",
            "arguments": value,
            "call_id": call.id,
            "name": call.name,
        })
    }
}

fn custom_tool_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|parsed| parsed["input"].as_str().map(str::to_owned))
        .unwrap_or_else(|| arguments.to_owned())
}

fn responses_usage(value: &Value) -> Option<Value> {
    let input = integer(&value["input_tokens"]).or_else(|| integer(&value["prompt_tokens"]))?;
    let output =
        integer(&value["output_tokens"]).or_else(|| integer(&value["completion_tokens"]))?;
    let total = input.checked_add(output)?;
    if total <= 0 {
        return None;
    }
    let mut usage =
        json!({ "input_tokens": input, "output_tokens": output, "total_tokens": total });
    if let Some(cached) = integer(&value["input_tokens_details"]["cached_tokens"])
        .or_else(|| integer(&value["prompt_tokens_details"]["cached_tokens"]))
    {
        usage["input_tokens_details"] = json!({ "cached_tokens": cached });
    }
    if let Some(reasoning) = integer(&value["output_tokens_details"]["reasoning_tokens"])
        .or_else(|| integer(&value["completion_tokens_details"]["reasoning_tokens"]))
    {
        usage["output_tokens_details"] = json!({ "reasoning_tokens": reasoning });
    }
    Some(usage)
}

fn reasoning_text(delta: &Value) -> String {
    if let Some(text) = nonempty_string(&delta["reasoning_content"])
        .or_else(|| nonempty_string(&delta["reasoning"]))
    {
        return text.to_owned();
    }
    delta["reasoning_details"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|part| {
            if let Some(text) = part.as_str() {
                text.to_owned()
            } else if truthy(&part["text"]) {
                js_text(&part["text"])
            } else if truthy(&part["content"]) {
                js_text(&part["content"])
            } else {
                String::new()
            }
        })
        .collect()
}

fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        let number = value.as_f64()?;
        (number.fract() == 0.0 && number >= i64::MIN as f64 && number < -(i64::MIN as f64))
            .then_some(number as i64)
    })
}

fn nonempty_string(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

fn js_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(_) if let Some(number) = integer(value) => number.to_string(),
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
        Value::Object(_) => "[object Object]".to_owned(),
        _ => value.to_string(),
    }
}
