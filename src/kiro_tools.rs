use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::provider_index::js_trim;

const MAX_ID: usize = 64;
const MAX_DESCRIPTION: usize = 10_237;

pub(super) fn specs(body: &Value) -> Option<Vec<Value>> {
    if body
        .get("tool_choice")
        .is_some_and(|choice| !choice.is_null() && choice != "auto")
    {
        return None;
    }
    let tools = match body.get("tools") {
        Some(tools) if !tools.is_null() => tools.as_array()?.as_slice(),
        _ => &[],
    };
    let mut names = HashSet::new();
    tools.iter().map(|tool| {
        if tool["type"] != "function" { return None }
        let function = &tool["function"];
        let name = function["name"].as_str().filter(|name| valid_name(name))?;
        if !names.insert(name) { return None }
        let fallback = format!("Tool: {name}");
        let description: String = function["description"].as_str().filter(|value| !value.is_empty())
            .unwrap_or(&fallback).chars().take(MAX_DESCRIPTION).collect();
        let mut schema = function["parameters"].as_object().cloned().unwrap_or_default();
        schema.insert("type".into(),json!("object"));
        if !schema.get("properties").is_some_and(Value::is_object) {
            schema.insert("properties".into(),json!({}));
        }
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            let properties = schema["properties"].as_object().expect("properties object");
            let mut unique = HashSet::new();
            let fields: Vec<_> = required.iter().filter_map(Value::as_str)
                .filter(|field| properties.contains_key(*field) && unique.insert(*field)).collect();
            schema.insert("required".into(),json!(fields));
        }
        let mut schema = Value::Object(schema);
        clean_schema(&mut schema);
        Some(json!({"toolSpecification":{"name":name,"description":description,"inputSchema":{"json":schema}}}))
    }).collect()
}

fn clean_schema(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|name, child| {
                name != "additionalProperties"
                    && !(name == "required" && child.as_array().is_some_and(Vec::is_empty))
            });
            for child in fields.values_mut() {
                clean_schema(child);
            }
        }
        Value::Array(parts) => parts.iter_mut().for_each(clean_schema),
        _ => (),
    }
}

pub(super) fn call(call: &Value, specs: &[Value]) -> Option<Value> {
    if call["type"] != "function" {
        return None;
    }
    let id = call["id"].as_str().filter(|id| valid_id(id))?;
    let name = call["function"]["name"].as_str()?;
    if !specs
        .iter()
        .any(|spec| spec["toolSpecification"]["name"] == name)
    {
        return None;
    }
    let arguments: Value = serde_json::from_str(call["function"]["arguments"].as_str()?).ok()?;
    if !arguments.is_object() {
        return None;
    }
    Some(json!({"toolUseId":id,"name":name,"input":arguments}))
}

pub(super) fn result(message: &Value) -> Option<Value> {
    let id = message["tool_call_id"].as_str().filter(|id| valid_id(id))?;
    let content = message["content"].as_str()?;
    let status = if super::truthy(&message["is_error"]) || message["status"] == "error" {
        "error"
    } else {
        "success"
    };
    Some(json!({"toolUseId":id,"status":status,"content":[{"text":content}]}))
}

pub(super) fn pair(
    calls: &[Value],
    results: Vec<Value>,
    ids: &mut HashSet<String>,
) -> Option<Vec<Value>> {
    if calls.len() != results.len() {
        return None;
    }
    calls
        .iter()
        .map(|call| {
            let id = call["toolUseId"].as_str()?;
            if !ids.insert(id.to_owned()) {
                return None;
            }
            results
                .iter()
                .find(|result| result["toolUseId"] == id)
                .cloned()
        })
        .collect()
}

pub(super) fn names(body: &Value) -> HashSet<String> {
    body["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]["tools"]
        .as_array().into_iter().flatten()
        .filter_map(|tool|tool["toolSpecification"]["name"].as_str().map(str::to_owned)).collect()
}

fn valid_name(value: &str) -> bool {
    valid_id(value) && !value.starts_with('_') && !value.ends_with('_') && value != "tool_call"
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID
        && value
            .bytes()
            .all(|part| part.is_ascii_alphanumeric() || matches!(part, b'-' | b'_'))
}

#[derive(Default)]
pub(super) struct StreamCalls {
    calls: Vec<Pending>,
    indexes: HashMap<String, usize>,
    malformed: bool,
}

struct Pending {
    id: String,
    name: String,
    arguments: Option<Arguments>,
}

enum Arguments {
    Fragments(String),
    Object(Value),
}

impl StreamCalls {
    pub(super) fn push(&mut self, allowed: &HashSet<String>, frame: &Value, created: i64) {
        let calls = frame
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_else(|| std::slice::from_ref(frame));
        if calls.is_empty() {
            self.malformed = true;
        }
        for call in calls {
            if self.push_one(allowed, call, created).is_none() {
                self.malformed = true;
            }
        }
    }

    fn push_one(&mut self, allowed: &HashSet<String>, call: &Value, created: i64) -> Option<()> {
        let name = call["name"]
            .as_str()
            .map(js_trim)
            .filter(|name| allowed.contains(*name))?;
        let id = match call.get("toolUseId") {
            Some(id) if !id.is_null() => id.as_str().filter(|id| valid_id(id))?.to_owned(),
            _ => format!("call_{created}_{}", self.calls.len() + 1),
        };
        let slot = if let Some(index) = self.indexes.get(&id).copied() {
            let slot = self.calls.get_mut(index)?;
            if slot.name != name {
                return None;
            }
            slot
        } else {
            self.indexes.insert(id.clone(), self.calls.len());
            self.calls.push(Pending {
                id,
                name: name.to_owned(),
                arguments: None,
            });
            self.calls.last_mut()?
        };
        if let Some(input) = call.get("input") {
            match (input, &mut slot.arguments) {
                (Value::String(fragment), state @ None) => {
                    *state = Some(Arguments::Fragments(fragment.clone()))
                }
                (Value::String(fragment), Some(Arguments::Fragments(text))) => {
                    text.push_str(fragment)
                }
                (Value::Object(_), state @ (None | Some(Arguments::Object(_)))) => {
                    *state = Some(Arguments::Object(input.clone()))
                }
                _ => return None,
            }
        }
        Some(())
    }

    pub(super) fn finish(self) -> (Vec<Value>, usize, bool) {
        let mut output = Vec::new();
        let mut units = 0;
        let mut malformed = self.malformed;
        for call in self.calls {
            let arguments = call.arguments.and_then(|input| match input {
                Arguments::Fragments(text) => serde_json::from_str::<Value>(&text).ok(),
                Arguments::Object(value) => Some(value),
            });
            let Some(arguments) = arguments.filter(Value::is_object) else {
                malformed = true;
                continue;
            };
            let index = output.len() / 2;
            let arguments = arguments.to_string();
            units += call.name.encode_utf16().count() + arguments.encode_utf16().count();
            output.push(
                json!({"tool_calls":[{"index":index,"id":call.id,"type":"function",
                "function":{"name":call.name,"arguments":""}}]}),
            );
            output.push(json!({"tool_calls":[{"index":index,"function":{"arguments":arguments}}]}));
        }
        (output, units, malformed)
    }
}
