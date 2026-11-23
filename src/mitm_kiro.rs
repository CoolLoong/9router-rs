use std::collections::BTreeMap;

use serde_json::{Value, json};

pub(super) fn request(original: &Value, model: &str) -> Option<Value> {
    let conversation = &original["conversationState"];
    let history = conversation["history"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut messages = Vec::new();
    for entry in history
        .iter()
        .chain(std::iter::once(&conversation["currentMessage"]))
    {
        if let Some(user) = entry
            .get("userInputMessage")
            .filter(|entry| entry.is_object())
        {
            append_user(&mut messages, user);
        } else if let Some(assistant) = entry
            .get("assistantResponseMessage")
            .filter(|entry| entry.is_object())
        {
            let tools = assistant["toolUses"].as_array();
            let mut message = json!({"role":"assistant","content":assistant["content"].as_str().unwrap_or_default()});
            if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
                if message["content"] == "" {
                    message["content"] = Value::Null;
                }
                message["tool_calls"] = json!(tools.iter().map(|tool| {
                    let name = tool["name"].as_str().unwrap_or_default();
                    let id = tool["toolUseId"].as_str().filter(|id| !id.is_empty()).map_or_else(
                        || format!("call_{}", uuid::Uuid::new_v4()), str::to_owned);
                    let args = if let Some(text) = tool["input"].as_str() {
                        text.to_owned()
                    } else {
                        tool.get("input").filter(|input| !input.is_null()).map_or_else(|| "{}".into(), Value::to_string)
                    };
                    json!({"id":id,"type":"function","function":{"name":name,"arguments":args}})
                }).collect::<Vec<_>>());
            }
            messages.push(message);
        }
    }
    if messages.is_empty() {
        return None;
    }
    let tools =
        conversation["currentMessage"]["userInputMessage"]["userInputMessageContext"]["tools"]
            .as_array()
            .filter(|tools| !tools.is_empty())
            .or_else(|| {
                history.iter().find_map(|entry| {
                    entry["userInputMessage"]["userInputMessageContext"]["tools"].as_array()
                })
            });
    let mut result = json!({"model":model,"messages":messages,"stream":true});
    if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
        result["tools"] = json!(tools.iter().map(|tool| {
            let tool = tool.get("toolSpecification").filter(|spec| spec.is_object()).unwrap_or(tool);
            let name = tool["name"].as_str().unwrap_or_default();
            let description = tool["description"].as_str().filter(|text| !text.is_empty())
                .map_or_else(|| format!("Tool: {}", if name.is_empty() { "unknown" } else { name }), str::to_owned);
            let parameters = tool["inputSchema"].get("json").cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{},"required":[]}));
            json!({"type":"function","function":{"name":name,"description":description,"parameters":parameters}})
        }).collect::<Vec<_>>());
        result["tool_choice"] = json!("auto");
    }
    Some(result)
}

fn append_user(messages: &mut Vec<Value>, user: &Value) {
    let start = messages.len();
    for result in user["userInputMessageContext"]["toolResults"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let content = result["content"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|part| part["text"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        messages.push(json!({"role":"tool","tool_call_id":result["toolUseId"].as_str().unwrap_or_default(),"content":content}));
    }
    let text = user["content"].as_str().unwrap_or_default().trim();
    let mut parts = Vec::new();
    for image in user["images"].as_array().into_iter().flatten() {
        let media = match image["format"].as_str() {
            Some("png") => "image/png",
            Some("jpeg" | "jpg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => continue,
        };
        if let Some(encoded) = image["source"]["bytes"]
            .as_str()
            .filter(|encoded| !encoded.is_empty())
        {
            parts.push(json!({"type":"image_url","image_url":{"url":format!("data:{media};base64,{encoded}")}}));
        }
    }
    if !parts.is_empty() {
        if !text.is_empty() {
            parts.insert(0, json!({"type":"text","text":text}));
        }
        messages.push(json!({"role":"user","content":parts}));
    } else if !text.is_empty() || start == messages.len() {
        messages.push(json!({"role":"user","content":text}));
    }
}

pub(super) struct Stream {
    model: String,
    initialized: bool,
    finished: bool,
    has_tools: bool,
    tools: BTreeMap<u64, (String, String)>,
    usage: Option<Value>,
    pending: String,
    end_tag: Option<&'static str>,
}

impl Stream {
    pub(super) fn new(model: String) -> Self {
        Self {
            model,
            initialized: false,
            finished: false,
            has_tools: false,
            tools: BTreeMap::new(),
            usage: None,
            pending: String::new(),
            end_tag: None,
        }
    }

    pub(super) fn chunk(&mut self, value: Value, output: &mut Vec<u8>) {
        self.initial(output);
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            self.usage = Some(usage.clone());
        }
        if self.finished {
            return;
        }
        let choice = &value["choices"][0];
        let delta = &choice["delta"];
        if let Some(calls) = delta["tool_calls"].as_array() {
            self.has_tools = true;
            for call in calls {
                let index = call["index"].as_u64().unwrap_or_default();
                let id = call["id"].as_str().unwrap_or_default();
                let name = call["function"]["name"].as_str().unwrap_or_default();
                if !id.is_empty() && !name.is_empty() && !self.tools.contains_key(&index) {
                    self.tools.insert(index, (id.to_owned(), name.to_owned()));
                    frame(output, "toolUseEvent", json!({"name":name,"toolUseId":id}));
                }
                if let Some(input) = call["function"]["arguments"]
                    .as_str()
                    .filter(|input| !input.is_empty())
                {
                    let (id, name) = self
                        .tools
                        .get(&index)
                        .map_or((id, name), |(id, name)| (id.as_str(), name.as_str()));
                    frame(
                        output,
                        "toolUseEvent",
                        json!({"input":input,"name":name,"toolUseId":id}),
                    );
                }
            }
        }
        if let Some(content) = delta["reasoning_content"]
            .as_str()
            .filter(|text| !text.is_empty())
        {
            self.text_frame(output, "reasoningContentEvent", content);
        }
        if let Some(text) = delta["content"].as_str().filter(|text| !text.is_empty()) {
            self.pending.push_str(text);
            self.drain_thinking(output);
        }
        if choice["finish_reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty())
        {
            self.finish(output);
        }
    }

    fn drain_thinking(&mut self, output: &mut Vec<u8>) {
        loop {
            let lower = self.pending.to_ascii_lowercase();
            if let Some(close) = self.end_tag {
                let Some(end) = lower.find(close) else {
                    if self.pending.len() > 1024 * 1024 {
                        let emit = self
                            .pending
                            .floor_char_boundary(self.pending.len() - close.len());
                        self.text_frame(output, "reasoningContentEvent", &self.pending[..emit]);
                        self.pending.drain(..emit);
                    }
                    break;
                };
                let remainder = self.pending.split_off(end + close.len());
                self.pending.truncate(end);
                self.text_frame(output, "reasoningContentEvent", &self.pending);
                self.pending = remainder;
                self.end_tag = None;
                continue;
            }
            let tags = [("<thinking>", "</thinking>"), ("<think>", "</think>")];
            if let Some((start, open, close)) = tags
                .iter()
                .filter_map(|(open, close)| lower.find(open).map(|start| (start, *open, *close)))
                .min_by_key(|(start, _, _)| *start)
            {
                self.text_frame(
                    output,
                    "assistantResponseEvent",
                    self.pending[..start].trim(),
                );
                self.pending.drain(..start + open.len());
                self.end_tag = Some(close);
                continue;
            }
            let keep = lower
                .rfind('<')
                .filter(|&start| tags.iter().any(|(tag, _)| tag.starts_with(&lower[start..])))
                .map_or(0, |start| lower.len() - start);
            let emit = self.pending.len() - keep;
            self.text_frame(output, "assistantResponseEvent", &self.pending[..emit]);
            self.pending.drain(..emit);
            break;
        }
    }

    pub(super) fn finish(&mut self, output: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        self.initial(output);
        if !self.pending.is_empty() {
            let kind = if self.end_tag.is_some() {
                "reasoningContentEvent"
            } else {
                "assistantResponseEvent"
            };
            self.text_frame(output, kind, &self.pending);
            self.pending.clear();
        }
        if self.has_tools {
            for (id, name) in self.tools.values() {
                frame(
                    output,
                    "toolUseEvent",
                    json!({"name":name,"stop":true,"toolUseId":id}),
                );
            }
        } else {
            frame(output, "messageStopEvent", json!({}));
        }
        if let Some(usage) = &self.usage {
            frame(
                output,
                "usageEvent",
                json!({
                    "inputTokens":usage["prompt_tokens"].as_u64().unwrap_or_default(),
                    "outputTokens":usage["completion_tokens"].as_u64().unwrap_or_default(),
                }),
            );
        }
        self.finished = true;
    }

    fn initial(&mut self, output: &mut Vec<u8>) {
        if !self.initialized {
            frame(output, "initial-response", json!({"conversationId":""}));
            self.initialized = true;
        }
    }

    fn text_frame(&self, output: &mut Vec<u8>, kind: &str, content: &str) {
        if !content.is_empty() {
            frame(
                output,
                kind,
                json!({"content":content,"modelId":self.model}),
            );
        }
    }
}

fn frame(output: &mut Vec<u8>, event: &str, body: Value) {
    let mut headers = Vec::new();
    let content_type = if event == "initial-response" {
        "application/x-amz-json-1.0"
    } else {
        "application/json"
    };
    for (name, value) in [
        (":message-type", "event"),
        (":event-type", event),
        (":content-type", content_type),
    ] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = body.to_string();
    let start = output.len();
    let total = 16 + headers.len() + payload.len();
    output.extend_from_slice(&(total as u32).to_be_bytes());
    output.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    output.extend_from_slice(&crc(&output[start..]).to_be_bytes());
    output.extend_from_slice(&headers);
    output.extend_from_slice(payload.as_bytes());
    output.extend_from_slice(&crc(&output[start..]).to_be_bytes());
}

fn crc(bytes: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(bytes);
    crc.sum()
}
