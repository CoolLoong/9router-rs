use std::collections::HashSet;

use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::chat_output::js_text;
use crate::provider_index::js_trim;

use super::Failure;

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

pub(super) struct ToolBridge {
    declared: HashSet<String>,
    completed: HashSet<(String, String)>,
    nonce: String,
    manifest: String,
    parallel: bool,
}

impl ToolBridge {
    pub(super) fn from_request(body: &Value, messages: &[Value]) -> Result<Option<Self>, Failure> {
        if body["tool_choice"] == "none" {
            return Ok(None);
        }
        let bad = |message| Failure::new(StatusCode::BAD_REQUEST, message);
        if !body["functions"].is_null()
            && body["functions"]
                .as_array()
                .is_none_or(|tools| !tools.is_empty())
        {
            return Err(bad("Grok Web requires function declarations in tools"));
        }
        if body["tools"].is_null() {
            return Ok(None);
        }
        let tools = body["tools"]
            .as_array()
            .ok_or_else(|| bad("Invalid Grok Web tool declarations"))?;
        let choice = body
            .get("tool_choice")
            .filter(|choice| !choice.is_null())
            .cloned()
            .unwrap_or_else(|| json!("auto"));
        let forced = match choice.as_str() {
            Some("required" | "auto") => None,
            None if choice["type"] == "function" => Some(
                choice["function"]["name"]
                    .as_str()
                    .ok_or_else(|| bad("Invalid Grok Web named tool choice"))?,
            ),
            _ => return Err(bad("Invalid Grok Web tool choice")),
        };
        let mut declared = HashSet::new();
        let mut definitions = Vec::new();
        for tool in tools.iter().filter(|tool| tool["type"] == "function") {
            let definition = &tool["function"];
            let name = definition["name"]
                .as_str()
                .map(js_trim)
                .filter(|name| {
                    (1..=64).contains(&name.len())
                        && name
                            .bytes()
                            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-'))
                })
                .ok_or_else(|| bad("Invalid Grok Web tool name"))?;
            if forced.is_none_or(|forced| forced == name) && declared.insert(name.to_owned()) {
                definitions.push((name, definition));
            }
        }
        if declared.is_empty() {
            return if forced.is_some() || choice == "required" {
                Err(bad("Grok Web tool choice requires a declared function"))
            } else {
                Ok(None)
            };
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let mut manifest = format!(
            "CLIENT_TOOLS: These are the caller's tools for this request. To request one, respond with \
             <tool_call>{{\"name\":\"exact_tool_name\",\"arguments\":{{...}},\"_nonce\":\"{nonce}\"}}</tool_call>. \
             After receiving tool results, answer normally.\ntool_choice={choice}"
        );
        let mut completed = HashSet::new();
        let mut summaries = Vec::new();
        for call in messages
            .iter()
            .filter(|message| message["role"] == "assistant")
            .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
        {
            let function = &call["function"];
            let Some(name) = function["name"].as_str() else {
                continue;
            };
            let Some(args) = arguments(&function["arguments"]) else {
                continue;
            };
            if completed.insert(key(name, &args)) {
                let brief = args
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(_, value)| js_text(value).chars().take(60).collect::<String>())
                    .collect::<Vec<_>>()
                    .join(", ");
                summaries.push(format!("- {name}({brief})"));
            }
        }
        if !summaries.is_empty() {
            manifest.push_str("\ncompleted_tool_calls:\n");
            manifest.push_str(&summaries.join("\n"));
            manifest.push_str("\nDo not repeat completed tool calls unless a different result is required; use their tool results to answer.");
        }
        manifest.push_str("\ntools:");
        for (index, (name, tool)) in definitions.into_iter().enumerate() {
            manifest.push_str(&format!("\n{}. name: {name}", index + 1));
            if let Some(description) = tool["description"].as_str().filter(|text| !text.is_empty())
            {
                manifest.push_str("\n   description: ");
                manifest.push_str(description);
            }
            if let Some(parameters) = tool.get("parameters") {
                manifest.push_str("\n   parameters: ");
                manifest.push_str(&parameters.to_string());
            }
        }
        Ok(Some(Self {
            declared,
            completed,
            nonce,
            manifest,
            parallel: body["parallel_tool_calls"] != false,
        }))
    }

    pub(super) fn manifest(&self) -> &str {
        &self.manifest
    }

    pub(super) fn redact(&self, text: &str) -> String {
        text.replace(&self.nonce, "[redacted]")
    }

    pub(super) fn parse(&self, text: &str) -> (String, Vec<Value>) {
        let mut calls = Vec::new();
        let mut seen = HashSet::new();
        let mut content = String::new();
        let mut cursor = 0;
        let mut copied_until = 0;
        while let Some(begin) = text[cursor..].find(OPEN).map(|begin| cursor + begin) {
            let after_open = begin + OPEN.len();
            let Some(end) = text[after_open..].find(CLOSE).map(|end| after_open + end) else {
                break;
            };
            cursor = end + CLOSE.len();
            let Ok(parsed) = serde_json::from_str::<Value>(js_trim(&text[after_open..end])) else {
                continue;
            };
            let Some(name) = parsed["name"]
                .as_str()
                .filter(|name| self.declared.contains(*name))
            else {
                continue;
            };
            if parsed["_nonce"] != self.nonce {
                continue;
            }
            let Some(arguments) = arguments(&parsed["arguments"]) else {
                continue;
            };
            let args = arguments.to_string();
            let call_key = key(name, &arguments);
            if args.contains(&self.nonce)
                || self.completed.contains(&call_key)
                || !seen.insert(call_key)
            {
                continue;
            }
            calls.push(json!({
                "id":format!("call_{}", uuid::Uuid::new_v4()),"type":"function",
                "function":{"name":name,"arguments":args}
            }));
            content.push_str(&text[copied_until..begin]);
            copied_until = cursor;
            if !self.parallel {
                break;
            }
        }
        if calls.is_empty() {
            (self.redact(text), calls)
        } else {
            content.push_str(&text[copied_until..]);
            (self.redact(js_trim(&content)), calls)
        }
    }
}

fn arguments(value: &Value) -> Option<Value> {
    match value {
        Value::Null => Some(json!({})),
        Value::Object(_) => Some(value.clone()),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object),
        _ => None,
    }
}

fn key(name: &str, args: &Value) -> (String, String) {
    let mut ordered = args.clone();
    ordered.sort_all_objects();
    (name.to_owned(), ordered.to_string())
}
