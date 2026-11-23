use std::time::Duration;

use axum::body::to_bytes;
use axum::response::Response;
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::chat_input::{js_space, truthy};
use crate::chat_output::js_text;

const LIMIT: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy)]
pub struct Tuning {
    min_panel: usize,
    grace: Duration,
    timeout: Duration,
}

impl Tuning {
    pub fn new(count: usize, input: &Value) -> Self {
        let min_panel = input["minPanel"]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(2)
            .max(2)
            .min(count);
        let duration = |key: &str, default| {
            Duration::from_millis(
                input[key]
                    .as_u64()
                    .unwrap_or(default)
                    .min(u64::from(u32::MAX)),
            )
        };
        Self {
            min_panel,
            grace: duration("stragglerGraceMs", 8000),
            timeout: duration("panelHardTimeoutMs", 90000),
        }
    }
}

pub fn panel_body(mut body: Value) -> Option<Value> {
    body.as_object_mut()?
        .retain(|key, _| !matches!(key.as_str(), "tools" | "tool_choice" | "stream_options"));
    body["stream"] = json!(false);
    let key = if body["messages"].is_array() {
        "messages"
    } else {
        "input"
    };
    if let Some(messages) = body.get_mut(key).and_then(Value::as_array_mut) {
        messages.retain(truthy);
        for message in messages {
            flatten_message(message)?;
        }
    }
    Some(body)
}

fn extract_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part.get("text").filter(|text| !text.is_null()))
            .map(js_text)
            .collect(),
        _ => String::new(),
    }
}

fn flatten_message(body: &mut Value) -> Option<()> {
    if !body.is_object() {
        return Some(());
    }
    let block = &body["content"];
    if matches!(body["role"].as_str(), Some("tool" | "function")) {
        let extracted = extract_text(block);
        let content = if !extracted.is_empty() {
            extracted
        } else if block.is_null() {
            String::new()
        } else {
            js_text(block)
        };
        *body = json!({"role":"assistant","content":format!("[Tool result: {content}]")});
        return Some(());
    }
    if body["role"] == "assistant"
        && let Some(calls) = body["tool_calls"].as_array()
    {
        let names = calls
            .iter()
            .map(|call| {
                call["function"]["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .or_else(|| call["name"].as_str().filter(|name| !name.is_empty()))
                    .unwrap_or("tool")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let content = extract_text(block);
        let separator = if content.is_empty() { "" } else { "\n" };
        body.as_object_mut()?.shift_remove("tool_calls");
        body["content"] = json!(format!("{content}{separator}[Called tools: {names}]"));
        return Some(());
    }
    if let Some(blocks) = block.as_array().filter(|blocks| {
        blocks
            .iter()
            .any(|block| matches!(block["type"].as_str(), Some("tool_use" | "tool_result")))
    }) {
        let mut text = Vec::new();
        let mut names = Vec::new();
        let mut results = Vec::new();
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => {
                    if truthy(&block["text"]) {
                        text.push(js_text(&block["text"]));
                    }
                }
                Some("tool_use") => names.push(
                    block["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .unwrap_or("tool"),
                ),
                Some("tool_result") => {
                    let result = extract_text(&block["content"]);
                    results.push(if !result.is_empty() {
                        result
                    } else if block["content"].is_null() {
                        String::new()
                    } else {
                        js_text(&block["content"])
                    });
                }
                _ => (),
            }
        }
        if !names.is_empty() {
            text.push(format!("[Called tools: {}]", names.join(", ")));
        }
        if !results.is_empty() {
            text.push(format!("[Tool result: {}]", results.join("\n")));
        }
        body["content"] = json!(text.join("\n"));
    }
    Some(())
}

pub fn judge_body(mut body: Value, answers: &[(usize, String)]) -> Value {
    let panel = answers
        .iter()
        .enumerate()
        .map(|(i, (_, text))| format!("[Source {}]\n{text}", i + 1))
        .collect::<Vec<_>>()
        .join("\n\n");
    let prompt = [
        format!("You are the JUDGE in a model-fusion panel. {} expert models independently answered the user's most recent request. Their responses are below, anonymized by source.",answers.len()),
        String::new(),
        "Do NOT mention that multiple models were used, and do NOT refer to the sources. Produce ONE authoritative final answer addressed directly to the user.".to_owned(),
        String::new(),
        "First, internally analyze the panel along these dimensions: consensus (points most sources agree on — treat as higher-confidence), contradictions (where they disagree — resolve with your own judgment), partial coverage, unique insights only one source surfaced, and blind spots every source missed. Then write the best possible final answer grounded in that analysis — more complete and correct than any single response, with no filler.".into(),
        String::new(),
        "=== PANEL RESPONSES ===".into(),
        panel,
        "=== END PANEL RESPONSES ===".into(),
        String::new(),
        "Now write the final answer to the user's original request.".into(),
    ].join("\n");
    for field in ["messages", "input", "contents"] {
        if let Some(messages) = body.get_mut(field).and_then(Value::as_array_mut) {
            if field == "contents" {
                messages.push(json!({"role":"user","parts":[{"text":prompt}]}));
            } else {
                messages.push(json!({"role":"user","content":prompt}));
            }
            return body;
        }
    }
    body["messages"] = json!([{"role":"user","content":prompt}]);
    body
}

pub async fn collect(tasks: Vec<JoinHandle<Response>>, tuning: Tuning) -> Vec<(usize, String)> {
    let mut pending: FuturesUnordered<_> = tasks
        .into_iter()
        .enumerate()
        .map(|(index, task)| async move { (index, task.await.ok()) })
        .collect();
    let hard = Instant::now() + tuning.timeout;
    let mut finish = hard;
    let mut successes = 0;
    let mut completed = Vec::new();
    while let Ok(Some((index, result))) = tokio::time::timeout_at(finish, pending.next()).await {
        if let Some(response) = result.filter(|response| response.status().is_success()) {
            successes += 1;
            completed.push((index, response));
            if successes == tuning.min_panel {
                finish = finish.min(Instant::now() + tuning.grace);
            }
        }
    }
    completed.sort_by_key(|(index, _)| *index);
    let mut answers = Vec::new();
    for (index, response) in completed {
        let Ok(bytes) = to_bytes(response.into_body(), LIMIT).await else {
            continue;
        };
        let Ok(data) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let text = panel_text(&data);
        if !text.trim_matches(js_space).is_empty() {
            answers.push((index, text));
        }
    }
    answers
}

fn panel_text(value: &Value) -> String {
    if let Some(choice) = value["choices"].get(0) {
        let message = choice
            .get("message")
            .filter(|value| !value.is_null())
            .unwrap_or(&choice["delta"]);
        let text = extract_text(&message["content"]);
        if !text.trim_matches(js_space).is_empty() {
            return text;
        }
        if let Some(text) = choice["text"]
            .as_str()
            .filter(|text| !text.trim_matches(js_space).is_empty())
        {
            return text.into();
        }
    }
    let text = extract_text(&value["content"]);
    if !text.trim_matches(js_space).is_empty() {
        return text;
    }
    let text: String = value["candidates"][0]["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect();
    if !text.trim_matches(js_space).is_empty() {
        return text;
    }
    value["output"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter_map(|part| part["text"].as_str())
        .collect()
}
