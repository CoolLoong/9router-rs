use std::collections::HashMap;

use serde_json::{Value, json};

use crate::chat_input::js_json;
use crate::provider_connections::truthy;

pub(crate) fn prepare_openai(mut body: Value) -> Value {
    let messages = array(&body["messages"]);
    let mut names = HashMap::new();
    for message in messages.iter().filter(|item| item["role"] == "assistant") {
        for call in array(&message["tool_calls"]) {
            remember(&mut names, &call["id"], &call["function"]["name"]);
        }
        for part in array(&message["content"])
            .iter()
            .filter(|part| part["type"] == "tool_use")
        {
            remember(&mut names, &part["id"], &part["name"]);
        }
    }
    let mut prepared = Vec::with_capacity(messages.len());
    for message in messages {
        let role = message["role"].as_str().unwrap_or_default();
        let text = content(&message["content"], "");
        match role {
            "system" => prepared
                .push(json!({"role":"user","content":format!("[System Instructions]\n{text}")})),
            "tool" => {
                let id = message["tool_call_id"].as_str().unwrap_or_default();
                let name = nonempty(&message["name"])
                    .or_else(|| names.get(id).copied())
                    .unwrap_or("tool");
                prepared.push(json!({"role":"user","content":tool_result(name,id,&text)}));
            }
            "user" if let Some(parts) = message["content"].as_array() => {
                let text = parts
                    .iter()
                    .filter_map(|part| match part["type"].as_str() {
                        Some("text") => nonempty(&part["text"]).map(str::to_owned),
                        Some("tool_result") => {
                            let id = part["tool_use_id"].as_str().unwrap_or_default();
                            let name = names
                                .get(id)
                                .or_else(|| names.get(first_line(id)))
                                .copied()
                                .unwrap_or("tool");
                            Some(tool_result(name, id, &content(&part["content"], "")))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    prepared.push(json!({"role":"user","content":text}));
                }
            }
            "assistant" => {
                let calls = array(&message["tool_calls"]);
                let calls: Vec<Value> = if calls.is_empty() {
                    array(&message["content"]).iter().filter_map(|part| {
                        let id = nonempty(&part["id"]).filter(|_| part["type"] == "tool_use")?;
                        let arguments = if truthy(&part["input"]) {
                            js_json(&part["input"], false).unwrap_or_else(|| part["input"].to_string())
                        } else {
                            "{}".into()
                        };
                        Some(json!({"id":id,"type":"function","function":{
                            "name":nonempty(&part["name"]).unwrap_or("tool"),"arguments":arguments,
                        }}))
                    }).collect()
                } else {
                    calls
                        .iter()
                        .map(|call| {
                            let mut call = call.as_object().cloned().unwrap_or_default();
                            call.shift_remove("index");
                            Value::Object(call)
                        })
                        .collect()
                };
                if !calls.is_empty() {
                    prepared.push(json!({"role":"assistant","content":text,"tool_calls":calls}));
                } else if !text.is_empty() {
                    prepared.push(json!({"role":"assistant","content":text}));
                }
            }
            "user" if !text.is_empty() => prepared.push(json!({"role":"user","content":text})),
            _ => (),
        }
    }
    if let Some(fields) = body.as_object_mut() {
        for key in [
            "user",
            "metadata",
            "tool_choice",
            "stream_options",
            "system",
        ] {
            fields.shift_remove(key);
        }
        fields.insert("messages".into(), json!(prepared));
        fields.insert("max_tokens".into(), json!(32_000));
    }
    body
}

pub(super) fn content(value: &Value, separator: &str) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    array(value)
        .iter()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join(separator)
}

fn remember<'a>(names: &mut HashMap<&'a str, &'a str>, id: &'a Value, name: &'a Value) {
    if let Some(id) = nonempty(id) {
        let name = nonempty(name).unwrap_or("tool");
        names.insert(id, name);
        let first = first_line(id);
        if !first.is_empty() && first != id {
            names.insert(first, name);
        }
    }
}

fn tool_result(name: &str, id: &str, text: &str) -> String {
    let clean: String = text.chars().filter(|ch| !matches!(ch,
        '\u{0000}'..='\u{0008}' | '\u{000b}' | '\u{000c}' | '\u{000e}'..='\u{001f}' | '\u{007f}'
    )).collect();
    format!(
        "<tool_result>\n<tool_name>{}</tool_name>\n<tool_call_id>{}</tool_call_id>\n<result>{}</result>\n</tool_result>",
        xml(name),
        xml(id),
        xml(&clean)
    )
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn first_line(id: &str) -> &str {
    id.split_once('\n').map_or(id, |(first, _)| first)
}

pub(super) fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

pub(super) fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}
