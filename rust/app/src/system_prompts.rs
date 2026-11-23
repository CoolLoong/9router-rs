use std::sync::OnceLock;

use serde_json::{Value, json};

use crate::chat_input::{js_space, truthy};

const SEP: &str = "\n\n";
static PROMPTS: OnceLock<Value> = OnceLock::new();

#[derive(Clone, Copy)]
pub enum Wire {
    OpenAi,
    Claude,
    Gemini,
    CommandCode,
}

pub fn selected<'a>(settings: &'a Value, token_saver: &str) -> Vec<&'a str> {
    let mut prompts = Vec::with_capacity(3);
    if !token_saver.eq_ignore_ascii_case("off") {
        let data = PROMPTS.get_or_init(|| {
            serde_json::from_str(include_str!("system-prompts.json"))
                .expect("embedded system prompts")
        });
        for (style, enabled, level) in [
            ("caveman", "cavemanEnabled", "cavemanLevel"),
            ("ponytail", "ponytailEnabled", "ponytailLevel"),
        ] {
            if truthy(&settings[enabled]) {
                let level = if truthy(&settings[level]) {
                    settings[level].as_str()
                } else {
                    Some("full")
                };
                if let Some(prompt) = level.and_then(|level| data[style][level].as_str()) {
                    prompts.push(prompt);
                }
            }
        }
    }
    if truthy(&settings["customPromptEnabled"])
        && let Some(prompt) = settings["customPrompt"]
            .as_str()
            .filter(|text| !text.trim_matches(js_space).is_empty())
    {
        prompts.push(prompt);
    }
    prompts
}

fn contains(text: &str, prompt: &str) -> bool {
    text == prompt || text.split(SEP).any(|part| part == prompt)
}

fn append(value: &mut Value, prompt: &str) {
    let current = value.as_str().unwrap_or_default();
    if !contains(current, prompt) {
        *value = if current.is_empty() {
            json!(prompt)
        } else {
            json!(format!("{current}{SEP}{prompt}"))
        };
    }
}

pub fn inject(body: &mut Value, wire: Wire, prompt: &str) {
    if !body.is_object() || prompt.is_empty() {
        return;
    }
    let is_kiro = body["conversationState"]["history"]
        .as_array()
        .is_some_and(|turns| {
            turns.iter().any(|turn| {
                truthy(&turn["userInputMessage"]) || truthy(&turn["assistantResponseMessage"])
            })
        })
        || truthy(&body["conversationState"]["currentMessage"]["userInputMessage"]);
    if is_kiro {
        let state = &mut body["conversationState"];
        let first = state["history"].as_array().and_then(|turns| {
            turns
                .iter()
                .position(|turn| truthy(&turn["userInputMessage"]))
        });
        let target = if let Some(first) = first {
            state["history"][first].get_mut("userInputMessage")
        } else {
            state
                .get_mut("currentMessage")
                .and_then(|message| message.get_mut("userInputMessage"))
        };
        if let Some(target) = target.filter(|target| target.is_object()) {
            append(&mut target["content"], prompt);
        }
        return;
    }

    match wire {
        Wire::CommandCode => {
            if let Some(params) = body.get_mut("params").filter(|params| params.is_object()) {
                append(&mut params["system"], prompt);
            }
        }
        Wire::Claude => {
            let system = &mut body["system"];
            if let Some(parts) = system.as_array_mut() {
                if !parts.iter().any(|part| part["text"] == prompt) {
                    let before = parts
                        .iter()
                        .rposition(|part| truthy(&part["cache_control"]))
                        .unwrap_or(parts.len());
                    parts.insert(before, json!({"type":"text", "text":prompt}));
                }
            } else {
                append(system, prompt);
            }
        }
        Wire::Gemini => {
            let target = if body["request"].is_object() {
                &mut body["request"]
            } else {
                body
            };
            let key = if target.get("system_instruction").is_some() {
                "system_instruction"
            } else {
                "systemInstruction"
            };
            let system = &mut target[key];
            if let Some(parts) = system.get_mut("parts").and_then(Value::as_array_mut) {
                if !parts.iter().any(|part| part["text"] == prompt) {
                    parts.push(json!({"text":prompt}));
                }
            } else {
                *system = json!({"parts":[{"text":prompt}]});
            }
        }
        Wire::OpenAi => {
            if body["instructions"].is_string() {
                append(&mut body["instructions"], prompt);
            } else if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
                inject_messages(messages, false, prompt);
            } else if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
                inject_messages(input, true, prompt);
            }
        }
    }
}

fn inject_messages(messages: &mut Vec<Value>, responses: bool, prompt: &str) {
    let instruction = |item: &Value| {
        matches!(item["role"].as_str(), Some("system" | "developer"))
            && (!responses || item["type"] == "message")
    };
    if messages
        .iter()
        .filter(|item| instruction(item))
        .any(|item| {
            let content = &item["content"];
            content
                .as_str()
                .is_some_and(|value| contains(value, prompt))
                || content.as_array().is_some_and(|parts| {
                    parts.iter().any(|part| {
                        part["text"]
                            .as_str()
                            .is_some_and(|text| contains(text, prompt))
                    })
                })
        })
    {
        return;
    }
    let block = json!({"type":if responses { "input_text" } else { "text" }, "text":prompt});
    if let Some(item) = messages.iter_mut().find(|item| instruction(item)) {
        let content = &mut item["content"];
        if let Some(parts) = content.as_array_mut() {
            parts.push(block);
        } else if responses && !content.is_string() {
            *content = json!([block]);
        } else {
            append(content, prompt);
        }
    } else if responses {
        messages.insert(
            0,
            json!({"type":"message", "role":"system", "content":[block]}),
        );
    } else {
        messages.insert(0, json!({"role":"system", "content":prompt}));
    }
}
