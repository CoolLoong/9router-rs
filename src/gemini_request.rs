use std::collections::{HashMap, HashSet, VecDeque};

use provider_capabilities_rs::port::model_capabilities;
use serde_json::{Map, Value, json};

use crate::chat_format::ChatFormat;
use crate::chat_input::{
    PreparedChat, js_space, preprocess_tool_history, stream_requested, truthy,
};
use crate::chat_output::js_text;
use crate::thinking_intent::{
    Thinking, budget_level, level_budget, model_suffix, numeric, thinking_intent,
};
use crate::{chat_protocol_input, openai_models, thinking_catalog};

pub const BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
pub const DEFAULT_SIGNATURE: &str = include_str!("gemini-thinking-signature.txt");

const SAFETY_CATEGORIES: &[&str] = &[
    "HARM_CATEGORY_HATE_SPEECH",
    "HARM_CATEGORY_DANGEROUS_CONTENT",
    "HARM_CATEGORY_SEXUALLY_EXPLICIT",
    "HARM_CATEGORY_HARASSMENT",
    "HARM_CATEGORY_CIVIC_INTEGRITY",
];

pub fn prepare(
    format: ChatFormat,
    mut body: Value,
    model: &str,
    accept: &str,
    signature: impl Fn(&str) -> Option<String>,
    signature_fallback: &str,
) -> Option<PreparedChat> {
    if !body.is_object()
        || body["contents"].is_array()
        || body["request"].is_object()
        || body["userAgent"] == "antigravity"
        || (format == ChatFormat::Chat
            && (body["input"].is_array() || body.get("system").is_some()))
    {
        return None;
    }
    let stream = stream_requested(&body, accept);
    let (clean_model, override_intent) = model_suffix(model);
    let intent = override_intent.or_else(|| thinking_intent(&body));
    let caps = model_capabilities(Some("gemini"), clean_model, None);

    if format != ChatFormat::Chat {
        let fields = body.as_object_mut()?;
        for key in [
            "thinking",
            "reasoning_effort",
            "reasoning",
            "thinkingConfig",
            "enable_thinking",
            "thinking_budget",
            "output_config",
        ] {
            fields.shift_remove(key);
        }
        if let Some(config) = fields
            .get_mut("generationConfig")
            .and_then(Value::as_object_mut)
        {
            config.shift_remove("thinkingConfig");
        }
    } else if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        preprocess_tool_history(messages)?;
    }
    let chat = chat_protocol_input::translate(format, body, accept)?;

    let gemini4 = is_gemini4(model);
    let mut generation = Map::new();
    if !gemini4 {
        copy(&chat, &mut generation, "temperature", "temperature");
        copy(&chat, &mut generation, "top_p", "topP");
        copy(&chat, &mut generation, "top_k", "topK");
    }
    copy(&chat, &mut generation, "max_tokens", "maxOutputTokens");
    apply_thinking(&mut generation, clean_model, intent, &caps);

    let messages = chat["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut contents = Vec::new();
    let mut system_instruction = None;
    let mut tool_history = ToolHistory::new(messages);
    for (index, message) in messages.iter().enumerate() {
        message.as_object()?;
        let content = &message["content"];
        match message["role"].as_str() {
            Some("system") if messages.len() > 1 => {
                system_instruction = Some(json!({
                    "role": "user",
                    "parts": [{ "text": extract_text(content) }]
                }));
            }
            Some("user" | "system") => {
                append_turn(
                    &mut contents,
                    "user",
                    content_parts(content, &caps)?,
                    gemini4,
                );
            }
            Some("assistant") => {
                let mut parts = Vec::new();
                if truthy(&message["reasoning_content"]) {
                    parts.push(json!({ "thought": true, "text": message["reasoning_content"] }));
                    parts.push(json!({ "thoughtSignature": signature_fallback, "text": "" }));
                }
                let content = extract_text(content);
                if !content.is_empty() {
                    parts.push(json!({ "text": content }));
                }
                let calls = message["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|call| call["type"] == "function")
                    .enumerate()
                    .map(|(index, call)| {
                        tool_history.call(call, &signature, index == 0, signature_fallback)
                    })
                    .collect::<Option<Vec<_>>>()?;
                for (_, _, _, part) in &calls {
                    parts.push(part.clone());
                }
                append_turn(&mut contents, "model", parts, gemini4);
                if index + 1 < messages.len()
                    || calls.iter().any(|(id, _, _, _)| {
                        tool_history
                            .outputs
                            .get(id.as_str())
                            .is_some_and(|results| !results.is_empty())
                    })
                {
                    let results = calls
                        .into_iter()
                        .map(|(id, emitted, name, _)| tool_history.response(&id, emitted, &name))
                        .collect();
                    append_turn(&mut contents, "user", results, gemini4);
                }
            }
            _ => (),
        }
    }

    if contents.first().is_some_and(|turn| turn["role"] != "user") {
        contents.insert(0, json!({ "role": "user", "parts": [{ "text": "..." }] }));
    }
    if let Some(last) = contents.last().filter(|turn| turn["role"] == "model") {
        let mut continuation = last["parts"]
            .as_array()?
            .iter()
            .filter_map(|part| {
                let call = part.get("functionCall")?;
                let mut result = json!({
                    "name": call["name"].as_str().filter(|name| !name.is_empty()).unwrap_or("tool"),
                    "response": { "result": "Continue." },
                });
                if truthy(&call["id"]) {
                    result["id"] = call["id"].clone();
                }
                Some(json!({ "functionResponse": result }))
            })
            .collect::<Vec<_>>();
        if continuation.is_empty() {
            continuation.push(json!({ "text": "Continue." }));
        }
        append_turn(&mut contents, "user", continuation, gemini4);
    }
    let safety = SAFETY_CATEGORIES
        .iter()
        .map(|category| json!({ "category": category, "threshold": "OFF" }))
        .collect::<Vec<_>>();
    let mut output = json!({
        "model": clean_model,
        "contents": contents,
        "generationConfig": generation,
        "safetySettings": safety,
    });
    if let Some(instruction) = system_instruction {
        output["systemInstruction"] = instruction;
    }
    let tools = declarations(&chat)?;
    if !tools.is_empty() {
        output["tools"] = json!([{ "functionDeclarations": tools }]);
    }

    Some(PreparedChat {
        body: output,
        stream,
    })
}

fn copy(source: &Value, target: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(field) = source.get(from) {
        target.insert(to.into(), field.clone());
    }
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

fn content_parts(content: &Value, caps: &Value) -> Option<Vec<Value>> {
    if let Some(text) = content.as_str() {
        return Some(vec![json!({ "text": text })]);
    }
    let mut output = Vec::new();
    for part in content.as_array().into_iter().flatten() {
        part.as_object()?;
        match part["type"].as_str() {
            Some("text") => {
                if let Some(text) = part.get("text") {
                    output.push(json!({ "text": text }));
                }
            }
            Some("image_url") if caps["vision"] == true => {
                let Some(url) = part["image_url"]["url"].as_str() else {
                    continue;
                };
                if let Some(inline) = inline_data(url) {
                    output.push(inline);
                } else if url.starts_with("http://") || url.starts_with("https://") {
                    let path = url::Url::parse(url)
                        .ok()
                        .map(|url| url.path().to_ascii_lowercase())
                        .unwrap_or_default();
                    let mime = match path.rsplit('.').next() {
                        Some("jpg" | "jpeg") => "image/jpeg",
                        Some("webp") => "image/webp",
                        Some("gif") => "image/gif",
                        Some("bmp") => "image/bmp",
                        _ => "image/png",
                    };
                    output.push(json!({ "fileData": { "fileUri": url, "mimeType": mime } }));
                }
            }
            Some("input_audio") if caps["audioInput"] == true => {
                let audio = &part["input_audio"];
                if truthy(&audio["data"]) {
                    let format = audio["format"]
                        .as_str()
                        .filter(|format| !format.is_empty())
                        .unwrap_or("wav");
                    let mime = if format == "mp3" {
                        "audio/mpeg".to_owned()
                    } else {
                        format!("audio/{format}")
                    };
                    output.push(
                        json!({ "inlineData": { "mime_type": mime, "data": audio["data"] } }),
                    );
                }
            }
            Some("audio_url") if caps["audioInput"] == true => {
                output.extend(part["audio_url"]["url"].as_str().and_then(inline_data));
            }
            Some("file") => {
                return None;
            }
            _ => (),
        }
    }
    Some(output)
}

fn inline_data(url: &str) -> Option<Value> {
    let (prefix, data) = url.strip_prefix("data:")?.split_once(',')?;
    Some(json!({ "inlineData": { "mime_type": prefix.split(';').next()?, "data": data } }))
}

fn is_gemini4(model: &str) -> bool {
    model.to_ascii_lowercase().split('/').any(|segment| {
        segment
            .strip_prefix("gemini-4")
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(['-', '.']))
    })
}

fn append_turn(contents: &mut Vec<Value>, role: &str, mut parts: Vec<Value>, result_last: bool) {
    if parts.is_empty() {
        return;
    }
    if let Some(turn) = contents.last_mut().filter(|last| last["role"] == role)
        && let Some(existing) = turn["parts"].as_array_mut()
    {
        existing.append(&mut parts);
        if result_last {
            existing.sort_by_key(|part| truthy(&part["functionResponse"]));
        }
        return;
    }
    contents.push(json!({ "role": role, "parts": parts }));
}

type ToolCall = (String, Option<String>, String, Value);

struct ToolHistory<'a> {
    outputs: HashMap<&'a str, VecDeque<&'a Value>>,
    names: HashMap<&'a str, &'a str>,
    emitted: HashSet<String>,
}

impl<'a> ToolHistory<'a> {
    fn new(messages: &'a [Value]) -> Self {
        let mut outputs: HashMap<_, VecDeque<_>> = HashMap::new();
        let mut names = HashMap::new();
        for message in messages {
            if message["role"] == "tool"
                && let Some(id) = message["tool_call_id"].as_str().filter(|id| !id.is_empty())
            {
                outputs
                    .entry(id)
                    .or_default()
                    .push_back(&message["content"]);
            }
            if message["role"] == "assistant" {
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    if call["type"] == "function"
                        && let Some(id) = call["id"].as_str().filter(|id| !id.is_empty())
                        && let Some(name) = call["function"]["name"]
                            .as_str()
                            .filter(|name| !name.is_empty())
                    {
                        names.insert(id, name);
                    }
                }
            }
        }
        Self {
            outputs,
            names,
            emitted: HashSet::new(),
        }
    }

    fn call(
        &mut self,
        call: &Value,
        signature: &impl Fn(&str) -> Option<String>,
        first: bool,
        signature_fallback: &str,
    ) -> Option<ToolCall> {
        let function = call["function"].as_object()?;
        let id = call["id"].as_str().unwrap_or_default().to_owned();
        let emitted = call.get("id").and_then(Value::as_str).map(|id| {
            let mut next = id.to_owned();
            for suffix in 2.. {
                if self.emitted.insert(next.clone()) {
                    break;
                }
                next = format!("{id}-{suffix}");
            }
            next
        });
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let args = function
            .get("arguments")
            .filter(|args| truthy(args))
            .map_or(json!({}), parse_json);
        let mut payload = json!({ "name": sanitize_name(&name), "args": args });
        if let Some(emitted) = &emitted {
            payload["id"] = json!(emitted);
        }
        let mut part = json!({ "functionCall": payload });
        if let Some(signature) =
            signature(&id).or_else(|| first.then(|| signature_fallback.to_owned()))
        {
            part["thoughtSignature"] = json!(signature);
        }
        Some((id, emitted, name, part))
    }

    fn response(&mut self, id: &str, emitted: Option<String>, name: &str) -> Value {
        let empty = Value::String(String::new());
        let raw = self
            .outputs
            .get_mut(id)
            .and_then(VecDeque::pop_front)
            .unwrap_or(&empty);
        let parsed = parse_json(raw);
        let result = match parsed {
            Value::Null => json!({ "result": raw }),
            Value::Array(_) | Value::Object(_) => sanitize_response(parsed),
            value => json!({ "result": value }),
        };
        let name = if !name.is_empty() {
            name.to_owned()
        } else if let Some(name) = self.names.get(id) {
            (*name).to_owned()
        } else {
            let parts: Vec<_> = id.split('-').collect();
            if parts.len() > 2 {
                parts[..parts.len() - 2].join("-")
            } else {
                id.to_owned()
            }
        };
        let mut response =
            json!({ "name": sanitize_name(&name), "response": { "result": result } });
        if let Some(id) = emitted {
            response["id"] = json!(id);
        }
        json!({ "functionResponse": response })
    }
}

fn parse_json(value: &Value) -> Value {
    match value {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        _ => value.clone(),
    }
}

fn sanitize_name(name: &str) -> String {
    if name.is_empty() {
        return "_unknown".into();
    }
    let mut output = String::new();
    for code in name.encode_utf16() {
        let ch = u8::try_from(code).ok().map(char::from).unwrap_or('_');
        let ch = if ch.is_ascii_alphanumeric() || "_-.:".contains(ch) {
            ch
        } else {
            '_'
        };
        if output.is_empty() && !ch.is_ascii_alphabetic() && ch != '_' {
            output.push('_');
        }
        if output.len() == 64 {
            break;
        }
        output.push(ch);
    }
    output
}

fn sanitize_response(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(sanitize_response).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| {
                    (
                        if key == "$ref" { "_ref".into() } else { key },
                        sanitize_response(value),
                    )
                })
                .collect(),
        ),
        value => value,
    }
}

fn declarations(chat: &Value) -> Option<Vec<Value>> {
    let mut output = Vec::new();
    for tool in chat["tools"].as_array().into_iter().flatten() {
        let (tool, schema) = if truthy(&tool["name"]) && truthy(&tool["input_schema"]) {
            (tool, &tool["input_schema"])
        } else if tool["type"] == "function" && truthy(&tool["function"]) {
            (&tool["function"], &tool["function"]["parameters"])
        } else {
            continue;
        };
        let params = if truthy(schema) {
            super::gemini_schema::clean(schema.clone())
        } else {
            super::gemini_schema::clean(json!({ "type": "object", "properties": {} }))
        };
        output.push(json!({
            "name": sanitize_name(tool["name"].as_str().unwrap_or_default()),
            "description": tool.get("description").filter(|value| truthy(value)).cloned().unwrap_or(json!("")),
            "parameters": params,
        }));
    }
    Some(output)
}

fn apply_thinking(
    generation: &mut Map<String, Value>,
    model: &str,
    intent: Option<Thinking>,
    caps: &Value,
) {
    let Some(intent) = intent.filter(|_| caps["reasoning"] == true) else {
        return;
    };
    let (config, floor) = match caps["thinkingFormat"].as_str() {
        Some("gemini-level") => {
            let level = match &intent {
                Thinking::None => "minimal",
                Thinking::Auto => "high",
                Thinking::Level(level) => level.trim_matches(js_space),
                Thinking::Budget(budget) => budget_level(*budget),
            };
            let level = match level {
                "xhigh" | "max" => "high",
                "none" | "off" => "minimal",
                level => level,
            };
            let levels = thinking_catalog::levels("gemini", model, caps, openai_models::data());
            let level = levels
                .as_ref()
                .filter(|levels| !levels.iter().any(|item| item == level))
                .and_then(|levels| levels.iter().find(|item| *item != "none"))
                .map(String::as_str)
                .unwrap_or(level);
            let floor = match level {
                "minimal" => 4096,
                "low" => 8192,
                "medium" => 16384,
                _ => 65535,
            };
            (
                json!({ "thinkingLevel": level, "includeThoughts": level != "minimal" }),
                Some(floor),
            )
        }
        _ if matches!(intent, Thinking::None) && caps["thinkingCanDisable"] != false => (
            json!({ "thinkingBudget": 0, "includeThoughts": false }),
            None,
        ),
        _ => {
            let budget = match &intent {
                Thinking::None => Some(512.0),
                Thinking::Auto => None,
                Thinking::Level(level) => level_budget(level),
                Thinking::Budget(budget) => Some(*budget),
            };
            let budget = budget
                .map(|mut budget| {
                    if let Some(min) = caps["thinkingRange"]["min"].as_f64() {
                        budget = budget.max(min);
                    }
                    if let Some(max) = caps["thinkingRange"]["max"].as_f64() {
                        budget = budget.min(max);
                    }
                    budget
                })
                .unwrap_or(-1.0);
            let floor = if budget == -1.0 {
                32768
            } else if budget <= 1024.0 {
                8192
            } else if budget <= 8192.0 {
                16384
            } else if budget <= 24576.0 {
                32768
            } else {
                65535
            };
            (
                json!({ "thinkingBudget": budget, "includeThoughts": true }),
                Some(floor),
            )
        }
    };
    generation.insert("thinkingConfig".into(), config);
    if let Some(floor) = floor {
        let target = caps["maxOutput"]
            .as_i64()
            .map_or(floor, |limit| floor.min(limit));
        if generation
            .get("maxOutputTokens")
            .and_then(numeric)
            .is_none_or(|count| count < target as f64)
        {
            generation.insert("maxOutputTokens".into(), json!(target));
        }
    }
}
