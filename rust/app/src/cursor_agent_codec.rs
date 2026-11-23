use std::borrow::Cow;
use std::io::Read;

use flate2::read::GzDecoder;
use serde_json::{Map, Value, json};

use super::super::legacy::{len, message, scalar};
use super::super::request::{array, content, nonempty};
use crate::chat_input::js_json;
use crate::chat_output::js_text;
use crate::cursor_models::{Decoder, Wire, first_fields};
use crate::provider_connections::truthy;

const MAX_FRAME: usize = 16 * 1024 * 1024;

pub(super) fn run(body: &Value, model: &str) -> Option<Vec<u8>> {
    let messages = array(&body["messages"]);
    let system = messages
        .iter()
        .filter(|msg| msg["role"] == "system")
        .map(|msg| content(&msg["content"], "\n"))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let turns = messages
        .iter()
        .filter(|msg| msg["role"] != "system")
        .collect::<Vec<_>>();
    let current = turns
        .iter()
        .rposition(|msg| msg["role"] == "user")
        .unwrap_or_else(|| turns.len().saturating_sub(1));
    let user = turns
        .get(current)
        .map_or_else(String::new, |msg| content(&msg["content"], "\n"));
    let user = if user.is_empty() { "Continue." } else { &user };
    let text = if system.is_empty() {
        Cow::Borrowed(user)
    } else {
        Cow::Owned(format!("{system}\n\n{user}"))
    };
    let mut history = Vec::new();
    for turn in &turns[..current] {
        let mut texts = vec![content(&turn["content"], "\n")];
        if turn["role"] == "assistant" {
            for call in array(&turn["tool_calls"]) {
                texts.push(format!(
                    "[tool_call id={} name={} args={}]",
                    js_or(&call["id"], ""),
                    js_or(&call["function"]["name"], "tool"),
                    js_or(&call["function"]["arguments"], "{}")
                ));
            }
        } else if turn["role"] == "tool" {
            texts.push(format!(
                "[tool_result id={}]",
                js_or(&turn["tool_call_id"], "")
            ));
        }
        texts.retain(|text| !text.is_empty());
        if !texts.is_empty() {
            message(&mut history, 1, |out| {
                message(
                    out,
                    if turn["role"] == "assistant" { 2 } else { 1 },
                    |out| {
                        message(out, 1, |out| {
                            message(out, 1, |out| len(out, 1, texts.join("\n")))
                        });
                    },
                );
            });
        }
    }
    let effort = body
        .get("reasoning_effort")
        .filter(|value| truthy(value))
        .or_else(|| body["reasoning"].get("effort"))
        .map_or_else(String::new, js_text)
        .to_ascii_lowercase();
    let effort = match effort.as_str() {
        "ultra" => "max",
        "minimal" => "low",
        other => other,
    };
    let mut outer = Vec::new();
    message(&mut outer, 1, |out| {
        len(out, 1, []);
        message(out, 2, |out| {
            message(out, 1, |out| {
                message(out, 1, |out| {
                    len(out, 1, text.as_ref());
                    len(out, 2, uuid::Uuid::new_v4().to_string());
                    len(out, 3, []);
                    scalar(out, 4, 1);
                });
                if !history.is_empty() {
                    len(out, 7, history);
                }
            });
        });
        message(out, 3, |out| {
            for field in [1, 3, 4] {
                len(out, field, model);
            }
        });
        let tools = array(&body["tools"]);
        if !tools.is_empty() {
            message(out, 4, |out| {
                for tool in tools {
                    message(out, 1, |out| tool_definition(out, tool));
                }
            });
        }
        message(out, 9, |out| {
            len(out, 1, model);
            scalar(out, 7, 1);
            if !effort.is_empty() && effort != "none" {
                message(out, 3, |out| {
                    len(
                        out,
                        1,
                        if model.to_ascii_lowercase().contains("gpt-") {
                            "reasoning"
                        } else {
                            "effort"
                        },
                    );
                    len(out, 2, effort);
                });
            }
        });
    });
    frame(outer)
}

pub(super) fn tool_name(tool: &Value) -> Option<&str> {
    nonempty(&tool["function"]["name"]).or_else(|| nonempty(&tool["name"]))
}

fn tool_definition(out: &mut Vec<u8>, tool: &Value) {
    let name = tool_name(tool).unwrap_or_default();
    let function = &tool["function"];
    let description = nonempty(&function["description"])
        .or_else(|| nonempty(&tool["description"]))
        .unwrap_or_default();
    let schema = [
        &function["parameters"],
        &tool["parameters"],
        &tool["inputSchema"],
        &tool["input_schema"],
    ]
    .into_iter()
    .find(|value| truthy(value));
    len(out, 1, name);
    len(out, 2, description);
    message(out, 3, |out| {
        if let Some(schema) = schema {
            encode_value(out, schema);
        } else {
            len(out, 5, []);
        }
    });
    len(out, 4, "9router");
    len(out, 5, name);
}

fn encode_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => scalar(out, 1, 0),
        Value::Bool(value) => scalar(out, 4, u64::from(*value)),
        Value::Number(number) => {
            out.push(2 << 3 | 1);
            out.extend_from_slice(&number.as_f64().unwrap_or_default().to_le_bytes());
        }
        Value::String(text) => len(out, 3, text),
        Value::Array(values) => message(out, 6, |out| {
            for value in values {
                message(out, 1, |out| encode_value(out, value));
            }
        }),
        Value::Object(fields) => {
            let mut entries: Vec<_> = fields.iter().collect();
            entries.sort_by_key(|(key, _)| {
                key.parse::<u32>()
                    .ok()
                    .filter(|index| index.to_string() == **key)
                    .unwrap_or(u32::MAX)
            });
            message(out, 5, |out| {
                for (key, value) in entries {
                    message(out, 1, |out| {
                        len(out, 1, key);
                        message(out, 2, |out| encode_value(out, value));
                    });
                }
            });
        }
    }
}

fn frame(mut payload: Vec<u8>) -> Option<Vec<u8>> {
    let size = u32::try_from(payload.len()).ok()?;
    let mut request = Vec::with_capacity(payload.len() + 5);
    request.push(0);
    request.extend_from_slice(&size.to_be_bytes());
    request.append(&mut payload);
    Some(request)
}

pub(super) enum Event {
    Text(String),
    Thinking(String),
    Finish,
    Write(Vec<u8>),
    Tool {
        id: String,
        name: String,
        arguments: String,
    },
    Error(String),
}

pub(super) fn take(input: &[u8]) -> Result<Option<(usize, Vec<Event>)>, &'static str> {
    let Some(header) = input.get(..5) else {
        return Ok(None);
    };
    let flags = header[0];
    let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if length > MAX_FRAME {
        return Err("Cursor AgentService response frame is too large");
    }
    let total = length + 5;
    let Some(payload) = input.get(5..total) else {
        return Ok(None);
    };
    let payload = if flags & 1 != 0 {
        let mut decoded = Vec::new();
        GzDecoder::new(payload)
            .take(MAX_FRAME as u64 + 1)
            .read_to_end(&mut decoded)
            .map_err(|_| "Cursor AgentService returned invalid gzip data")?;
        if decoded.len() > MAX_FRAME {
            return Err("Cursor AgentService decompressed frame is too large");
        }
        Cow::Owned(decoded)
    } else {
        Cow::Borrowed(payload)
    };
    let events = if flags & 2 != 0 {
        serde_json::from_slice::<Value>(&payload)
            .ok()
            .and_then(|value| {
                let error = &value["error"];
                nonempty(&error["message"])
                    .or_else(|| nonempty(&error["code"]))
                    .map(|msg| vec![Event::Error(msg.to_owned())])
            })
            .unwrap_or_default()
    } else {
        decode(&payload).ok_or("Cursor AgentService returned an invalid protobuf frame")?
    };
    Ok(Some((total, events)))
}

fn decode(payload: &[u8]) -> Option<Vec<Event>> {
    let [update, exec, kv] = first_fields(payload, [1, 2, 4])?;
    let mut events = Vec::new();
    if let Some(update) = bytes(update) {
        let [text, thinking, done] = first_fields(update, [1, 4, 14])?;
        if let Some(text) = text_delta(text) {
            events.push(Event::Text(text));
        }
        if let Some(thinking) = text_delta(thinking) {
            events.push(Event::Thinking(thinking));
        }
        if done.is_some() {
            events.push(Event::Finish);
        }
    }
    if let Some(kv) = bytes(kv) {
        let [id, get, set, metadata] = first_fields(kv, [1, 2, 3, 4])?;
        if get.is_some() || set.is_some() {
            let mut outer = Vec::new();
            message(&mut outer, 3, |out| {
                let id = number(id);
                if id != 0 {
                    scalar(out, 1, id);
                }
                if get.is_some() {
                    message(out, 2, |out| len(out, 1, []));
                } else {
                    len(out, 3, []);
                }
                if let Some(metadata) = bytes(metadata).filter(|value| !value.is_empty()) {
                    len(out, 4, metadata);
                }
            });
            events.push(Event::Write(frame(outer)?));
        }
    }
    if let Some(exec) = bytes(exec) {
        let [id, exec_id, context, tool] = first_fields(exec, [1, 15, 10, 11])?;
        if context.is_some() {
            events.push(Event::Write(exec_reply(
                number(id),
                bytes(exec_id).unwrap_or_default(),
                10,
            )?));
        } else if let Some(tool) = bytes(tool) {
            events.push(mcp_call(tool).unwrap_or_else(unsupported));
        } else {
            let mut decoder = Decoder(exec);
            let mut variant = 0;
            while !decoder.0.is_empty() {
                let (field, _) = decoder.field()?;
                if !matches!(field, 1 | 15) {
                    variant = u64::from(field);
                    break;
                }
            }
            if matches!(variant, 2 | 3 | 4 | 5 | 7 | 8 | 9 | 16 | 20 | 23) {
                events.push(Event::Write(exec_reply(
                    number(id),
                    bytes(exec_id).unwrap_or_default(),
                    variant,
                )?));
            } else {
                events.push(unsupported());
            }
        }
    }
    Some(events)
}

fn unsupported() -> Event {
    Event::Error("Cursor AgentService requested an unsupported IDE tool".into())
}

fn exec_reply(id: u64, exec_id: &[u8], variant: u64) -> Option<Vec<u8>> {
    let mut outer = Vec::new();
    message(&mut outer, 2, |out| {
        if id != 0 {
            scalar(out, 1, id);
        }
        len(out, 15, exec_id);
        match variant {
            10 => message(out, 10, |out| message(out, 1, |out| len(out, 1, []))),
            9 => len(out, 9, []),
            field => message(out, field, |out| {
                message(out, 2, |out| {
                    len(
                        out,
                        2,
                        "Tool not available in this environment. Use the MCP tools provided instead.",
                    );
                })
            }),
        }
    });
    frame(outer)
}

fn mcp_call(payload: &[u8]) -> Option<Event> {
    let [name, call_id, actual_name] = first_fields(payload, [1, 3, 5])?;
    let actual_name = wire_text(actual_name);
    let name = if actual_name.is_empty() {
        wire_text(name)
    } else {
        actual_name
    };
    if name.is_empty() {
        return None;
    }
    let args = Value::Object(map(payload, 2, 0)?);
    let id = wire_text(call_id);
    Some(Event::Tool {
        id: if id.is_empty() {
            format!("call_{}", uuid::Uuid::new_v4())
        } else {
            id.into_owned()
        },
        name: name.into_owned(),
        arguments: js_json(&args, false).unwrap_or_else(|| args.to_string()),
    })
}

fn decode_value(payload: &[u8], depth: usize) -> Option<Value> {
    if depth > 64 {
        return None;
    }
    let [null, num, text, flag, map_value, list] = first_fields(payload, [1, 2, 3, 4, 5, 6])?;
    if null.is_some() {
        return Some(Value::Null);
    }
    if flag.is_some() {
        return Some(json!(number(flag) != 0));
    }
    if let Some(bytes) = bytes(num) {
        return Some(json!(f64::from_le_bytes(bytes.try_into().ok()?)));
    }
    if text.is_some() {
        return Some(json!(wire_text(text)));
    }
    if let Some(field) = bytes(map_value) {
        return map(field, 1, depth).map(Value::Object);
    }
    if let Some(field) = bytes(list) {
        let mut decoder = Decoder(field);
        let mut values = Vec::new();
        while !decoder.0.is_empty() {
            let (id, value) = decoder.field()?;
            if id == 1
                && let Wire::Bytes(value) = value
            {
                values.push(decode_value(value, depth + 1)?);
            }
        }
        return Some(Value::Array(values));
    }
    Some(Value::Null)
}

fn map(payload: &[u8], id: u32, depth: usize) -> Option<Map<String, Value>> {
    let mut decoder = Decoder(payload);
    let mut output = Map::new();
    while !decoder.0.is_empty() {
        let (field, value) = decoder.field()?;
        if field == id
            && let Wire::Bytes(pair) = value
        {
            let [key, value] = first_fields(pair, [1, 2])?;
            let key = wire_text(key);
            if !key.is_empty() && key != "__proto__" {
                let value = decode_value(bytes(value).unwrap_or_default(), depth + 1)?;
                output.insert(key.into_owned(), value);
            }
        }
    }
    Some(output)
}

fn text_delta(value: Option<Wire<'_>>) -> Option<String> {
    let [text] = first_fields(bytes(value)?, [1])?;
    let text = wire_text(text);
    (!text.is_empty()).then(|| text.into_owned())
}

fn wire_text(value: Option<Wire<'_>>) -> Cow<'_, str> {
    bytes(value).map_or(Cow::Borrowed(""), String::from_utf8_lossy)
}

fn bytes(value: Option<Wire<'_>>) -> Option<&[u8]> {
    match value {
        Some(Wire::Bytes(bytes)) => Some(bytes),
        _ => None,
    }
}

fn number(value: Option<Wire<'_>>) -> u64 {
    match value {
        Some(Wire::Varint(value)) => value,
        _ => 0,
    }
}

fn js_or(value: &Value, fallback: &str) -> String {
    if truthy(value) {
        js_text(value)
    } else {
        fallback.to_owned()
    }
}
