use std::collections::BTreeMap;

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::chat_input::{js_json, truthy};
use crate::chat_output::js_text;
use crate::provider_index::js_trim;
use crate::thinking_intent::numeric;

const WIRE_ALPHABET: &[u8; 64] =
    b"_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";

pub(super) fn encode(bytes: &[u8]) -> Vec<u8> {
    let text = STANDARD.encode(bytes);
    let text = text.as_bytes();
    let third = text.len() / 3;
    let pivot = text.len() - third;
    text[pivot..]
        .iter()
        .chain(&text[third..pivot])
        .chain(&text[..third])
        .map(|byte| match *byte {
            b'A'..=b'Z' => WIRE_ALPHABET[usize::from(*byte - b'A')],
            b'a'..=b'z' => WIRE_ALPHABET[26 + usize::from(*byte - b'a')],
            b'0'..=b'9' => WIRE_ALPHABET[52 + usize::from(*byte - b'0')],
            b'+' => WIRE_ALPHABET[62],
            b'/' => WIRE_ALPHABET[63],
            b'=' => b'$',
            _ => *byte,
        })
        .collect()
}

pub(super) fn static_config(key: &str) -> Option<Value> {
    let name = match key {
        "smodel" => "Sonus",
        "cmodel" => "Cantus",
        _ => return None,
    };
    Some(json!({
        "model":"","format":"openai","source":"system","url":"",
        "key":key,"display_name":name,"is_vl":true,"is_reasoning":true,
        "max_input_tokens":180000,
    }))
}

pub(super) fn build(
    key: &str,
    body: &Value,
    mut messages: Vec<Value>,
    mut config: Value,
    user: &Value,
) -> Option<Value> {
    let mut systems = Vec::new();
    messages.retain_mut(|message| {
        if !message.is_object() {
            return false;
        }
        if message["role"] == "system" {
            let text = extract_text(&message["content"]);
            if !text.is_empty() {
                systems.push(text);
            }
            return false;
        }
        message["content"] = normalize_content(&message["content"]);
        true
    });
    let system = systems.join("\n\n");
    let last = messages
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .map(|message| extract_text(&message["content"]))
        .unwrap_or_default();
    let max = numeric(&config["max_output_tokens"])
        .filter(|limit| *limit > 0.0)
        .unwrap_or(32_768.0);
    let max = ["max_tokens", "max_completion_tokens"]
        .into_iter()
        .filter_map(|key| body[key].as_f64().filter(|limit| *limit > 0.0))
        .fold(max, f64::min);
    let thinking = truthy(&config["is_reasoning"]);
    let tools = &body["tools"];
    let record = record_id(key, &messages, tools, max)?;
    let session = format!(
        "{:x}",
        Sha256::new()
            .chain_update(b"qoder-session\0")
            .chain_update(js_text(user))
            .chain_update([0])
            .chain_update(key)
            .finalize()
    );
    let tier = context_tier(&config, &system, &messages, tools);
    if let Some(tier) = tier {
        config["max_input_tokens"] = json!(tier);
    }
    let business_name = if last.encode_utf16().count() > 30 {
        format!(
            "{}...",
            String::from_utf16_lossy(&last.encode_utf16().take(30).collect::<Vec<_>>())
        )
    } else {
        last.clone()
    };
    let mut output = json!({
        "request_id":Uuid::new_v4(),
        "request_set_id":record,
        "chat_record_id":record,
        "session_id":&session[..16],
        "stream":true,
        "chat_task":"FREE_INPUT",
        "is_reply":true,
        "is_retry":false,
        "source":1,
        "version":"3",
        "session_type":"qodercli",
        "agent_id":"agent_common",
        "task_id":"common",
        "code_language":"",
        "chat_prompt":"",
        "image_urls":null,
        "aliyun_user_type":"",
        "system":system,
        "messages":messages,
        "tools":tools.as_array().map_or(&[][..], Vec::as_slice),
        "parameters":{"max_tokens":number_value(max)},
        "chat_context":{
            "chatPrompt":"",
            "imageUrls":null,
            "extra":{"context":[],"modelConfig":{"key":key,"is_reasoning":thinking},"originalContent":last},
            "features":[],
            "text":last,
        },
        "model_config":config,
        "business":{
            "product":"cli","version":"1.0.0","type":"agent","stage":"start",
            "id":Uuid::new_v4(),"name":business_name,"begin_at":Utc::now().timestamp_millis(),
        },
    });
    if let Some(tier) = tier {
        output["parameters"]["context_length"] = json!(tier);
        output["chat_context"]["extra"]["ideModelConfigOverride"] =
            json!({"max_input_tokens":tier});
    }
    Some(output)
}

fn record_id(key: &str, messages: &[Value], tools: &Value, max: f64) -> Option<String> {
    let mut hash = Sha256::new()
        .chain_update(b"qoder-record\0")
        .chain_update(key);
    for message in messages {
        if truthy(&message["role"]) {
            hash.update([0]);
            hash.update(js_text(&message["role"]));
        }
        match &message["content"] {
            Value::String(text) if !text.is_empty() => {
                hash.update([0]);
                hash.update(text);
            }
            content @ Value::Array(_) => {
                hash.update([0]);
                hash.update(js_json(content, false)?);
            }
            _ => (),
        }
    }
    if truthy(tools) {
        hash.update([0]);
        hash.update(js_json(tools, false)?);
    }
    hash.update(format!("\0mt={max}"));
    Some(format!("{:x}", hash.finalize())[..16].to_owned())
}

fn extract_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => js_text(content),
    }
}

fn normalize_content(content: &Value) -> Value {
    let Some(parts) = content.as_array() else {
        return json!(extract_text(content));
    };
    let mut blocks = Vec::new();
    let mut text_parts = Vec::new();
    for part in parts {
        let candidate = match part["type"].as_str() {
            Some("image_url") => part["image_url"]
                .as_str()
                .or_else(|| part["image_url"]["url"].as_str())
                .filter(|url| !url.is_empty())
                .map(str::to_owned),
            Some("image") => {
                let source = &part["source"];
                if source["type"] == "base64" {
                    source["data"]
                        .as_str()
                        .filter(|data| !data.is_empty())
                        .map(|data| {
                            let mime = source["media_type"].as_str().unwrap_or("image/png");
                            format!("data:{mime};base64,{data}")
                        })
                } else {
                    source["url"]
                        .as_str()
                        .filter(|url| !url.is_empty())
                        .map(str::to_owned)
                }
            }
            _ => None,
        };
        if let Some(url) = candidate {
            blocks.push(json!({"type":"image_url","image_url":{"url":url}}));
            continue;
        }
        let text = match part["type"].as_str() {
            Some("file") => document_stub(
                first(&part["file"], &["filename", "name"])
                    .as_str()
                    .unwrap_or("file"),
            ),
            Some("document") => document_stub(part["title"].as_str().unwrap_or("document")),
            _ => part["text"].as_str().unwrap_or_default().to_owned(),
        };
        if !text.is_empty() {
            if blocks.is_empty() {
                text_parts.push(text);
            } else {
                blocks.push(json!({"type":"text","text":text}));
            }
        }
    }
    if blocks.is_empty() {
        json!(text_parts.join("\n"))
    } else {
        if !text_parts.is_empty() {
            blocks.insert(0, json!({"type":"text","text":text_parts.join("\n")}));
        }
        json!(blocks)
    }
}

fn document_stub(name: &str) -> String {
    format!("[file omitted: {name} — Qoder reads documents via its file API, not inlined bytes]")
}

pub(super) fn number_value(number: f64) -> Value {
    if number.fract() == 0.0 && number >= i64::MIN as f64 && number < i64::MAX as f64 {
        json!(number as i64)
    } else {
        json!(number)
    }
}

fn first<'a>(value: &'a Value, fields: &[&str]) -> &'a Value {
    fields
        .iter()
        .map(|key| &value[*key])
        .find(|value| !value.is_null())
        .unwrap_or(&Value::Null)
}

fn parse_tokens(value: &Value) -> Option<u64> {
    let number = if let Some(raw) = value.as_str() {
        let raw = js_trim(raw).to_ascii_uppercase();
        let (number, scale) = match raw.strip_suffix('K').or_else(|| raw.strip_suffix('M')) {
            Some(number) => (
                js_trim(number),
                if raw.ends_with('K') {
                    1_000.0
                } else {
                    1_000_000.0
                },
            ),
            None => (raw.as_str(), 1.0),
        };
        let mut fields = number.split('.');
        let mut parts = fields.by_ref().take(2);
        if !parts.all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
            || fields.next().is_some()
        {
            return None;
        }
        number.parse::<f64>().ok()? * scale
    } else {
        value.as_f64()?
    };
    (number.is_finite() && number >= 1.0 && number < u64::MAX as f64)
        .then_some(number.floor() as u64)
}

fn context_tier(config: &Value, system: &str, messages: &[Value], tools: &Value) -> Option<u64> {
    let rows = first(config, &["context_config", "contextConfig"]).as_array()?;
    let mut tiers: BTreeMap<u64, (&str, bool)> = BTreeMap::new();
    for row in rows {
        let Some(tokens) = parse_tokens(first(
            row,
            &[
                "tokenCount",
                "token_count",
                "max_input_tokens",
                "maxInputTokens",
                "contextLength",
                "context_length",
            ],
        )) else {
            continue;
        };
        let name = first(
            row,
            &["name", "label", "display_name", "displayName", "key", "id"],
        )
        .as_str()
        .map(js_trim)
        .unwrap_or_default();
        let default = ["isDefault", "is_default", "default"]
            .into_iter()
            .any(|key| row[key] == true)
            || tiers.get(&tokens).is_some_and(|(_, default)| *default);
        tiers.insert(tokens, (name, default));
    }
    let smallest = *tiers.first_key_value()?.0;
    let largest = *tiers.last_key_value()?.0;
    let default = tiers
        .iter()
        .find_map(|(count, (_, default))| default.then_some(*count))
        .unwrap_or(smallest);
    let preference = std::env::var("QODER_CONTEXT_TIER").ok().unwrap_or_default();
    let mode = js_trim(&preference).to_ascii_lowercase();
    match mode.as_str() {
        "max" => return Some(largest),
        "default" => return Some(default),
        "" | "auto" => (),
        _ => {
            let clean = |value: &str| {
                value
                    .split_whitespace()
                    .collect::<String>()
                    .to_ascii_lowercase()
            };
            let wanted = clean(&mode);
            let count = parse_tokens(&json!(wanted));
            if let Some(number) = tiers.iter().find_map(|(tokens, (name, _))| {
                (count == Some(*tokens) || clean(name) == wanted).then_some(*tokens)
            }) {
                return Some(number);
            }
        }
    }
    let current =
        parse_tokens(first(config, &["max_input_tokens", "maxInputTokens"])).unwrap_or(default);
    let text = js_json(
        &json!({"system":system,"messages":messages,"tools":if truthy(tools) {tools.clone()} else {json!([])}}),
        false,
    )?;
    let units = text.encode_utf16().count();
    let cjk = text
        .chars()
        .filter(|ch| {
            matches!(*ch,
                '\u{1100}'..='\u{11ff}' | '\u{2e80}'..='\u{9fff}' | '\u{ac00}'..='\u{d7af}'
                | '\u{f900}'..='\u{faff}' | '\u{ff00}'..='\u{ffef}'
            )
        })
        .count();
    let estimate = cjk + (units - cjk).div_ceil(4);
    let needed = (estimate as f64 * 1.15).ceil() as u64;
    (needed > current && largest > current).then(|| {
        tiers
            .keys()
            .find(|count| **count >= needed && **count > current)
            .copied()
            .unwrap_or(largest)
    })
}
