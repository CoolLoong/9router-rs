use std::borrow::Cow;
use std::collections::HashSet;
use std::io::Read;

use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::chat_input::{js_json, js_space};
use crate::chat_output::estimated_usage;
use crate::cursor_models::{Wire, first_fields};
use crate::provider_connections::truthy;

const FRAME_LIMIT: usize = 16 * 1024 * 1024;

pub(crate) fn is_legacy_body(body: &Value) -> bool {
    body["messages"].as_array().is_none_or(|messages| {
        messages.is_empty()
            || messages.iter().any(|message| match &message["content"] {
                Value::Array(parts) => parts
                    .iter()
                    .any(|part| truthy(part) && !part.is_string() && part["type"] != "text"),
                Value::Null | Value::String(_) => false,
                _ => true,
            })
    })
}

pub(super) fn request(body: &Value, model: &str, force_agent: bool) -> Option<Vec<u8>> {
    let messages = array(&body["messages"]);
    let tools = array(&body["tools"]);
    let has_tools = !tools.is_empty();
    let agentic = has_tools || force_agent;
    let mut normalized = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        let role = if message["role"] == "user" { 1 } else { 2 };
        let content = message["content"].as_str().unwrap_or_default();
        let results = array(&message["tool_results"]);
        if message["role"] == "assistant"
            && !array(&message["tool_calls"]).is_empty()
            && !results.is_empty()
        {
            normalized.push((role, content, &[][..], Uuid::new_v4().to_string()));
            let ids = result_ids(results);
            if !messages.get(index + 1).is_some_and(|next| {
                next["role"] == "assistant"
                    && !ids.is_empty()
                    && ids == result_ids(array(&next["tool_results"]))
            }) {
                normalized.push((2, "", results, Uuid::new_v4().to_string()));
            }
        } else {
            normalized.push((role, content, results, Uuid::new_v4().to_string()));
        }
    }
    let mut request = Vec::new();
    for (position, (role, content, results, id)) in normalized.iter().enumerate() {
        message(&mut request, 1, |out| {
            len(out, 1, content);
            scalar(out, 2, *role);
            len(out, 13, id);
            for result in *results {
                message(out, 18, |out| tool_result(out, result));
            }
            scalar(out, 29, u64::from(has_tools));
            scalar(out, 47, if has_tools { 2 } else { 1 });
            if position + 1 == normalized.len() && has_tools {
                len(out, 51, [1]);
            }
        });
    }
    scalar(&mut request, 2, 1);
    len(&mut request, 3, []);
    scalar(&mut request, 4, 1);
    message(&mut request, 5, |out| {
        len(out, 1, model);
        len(out, 4, []);
    });
    len(&mut request, 8, []);
    scalar(&mut request, 13, 1);
    message(&mut request, 15, |out| {
        len(out, 1, r"cursor\aisettings");
        len(out, 3, []);
        message(out, 6, |out| {
            len(out, 1, []);
            len(out, 2, []);
        });
        scalar(out, 8, 1);
        scalar(out, 9, 1);
    });
    scalar(&mut request, 19, 1);
    len(&mut request, 23, Uuid::new_v4().to_string());
    message(&mut request, 26, metadata);
    scalar(&mut request, 27, u64::from(agentic));
    if agentic {
        len(&mut request, 29, [1]);
    }
    for (role, _, _, id) in normalized {
        message(&mut request, 30, |out| {
            len(out, 1, id);
            scalar(out, 3, role);
        });
    }
    for tool in tools {
        message(&mut request, 34, |out| mcp_tool(out, tool));
    }
    scalar(&mut request, 35, 0);
    scalar(&mut request, 38, 0);
    scalar(&mut request, 46, if agentic { 2 } else { 1 });
    len(&mut request, 47, []);
    scalar(&mut request, 48, u64::from(!agentic));
    scalar(
        &mut request,
        49,
        match body["reasoning_effort"].as_str() {
            Some("medium") => 1,
            Some("high") => 2,
            _ => 0,
        },
    );
    scalar(&mut request, 51, 0);
    scalar(&mut request, 53, 1);
    len(&mut request, 54, if agentic { "Agent" } else { "Ask" });
    let mut framed = Vec::with_capacity(request.len() + 10);
    framed.extend_from_slice(&[0; 5]);
    len(&mut framed, 1, request);
    let size = u32::try_from(framed.len() - 5).ok()?;
    framed[1..5].copy_from_slice(&size.to_be_bytes());
    Some(framed)
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn result_ids(results: &[Value]) -> HashSet<&str> {
    results
        .iter()
        .filter_map(|result| result["tool_call_id"].as_str())
        .collect()
}

fn metadata(out: &mut Vec<u8>) {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        os => os,
    };
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        arch => arch,
    };
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    len(out, 1, platform);
    len(out, 2, architecture);
    len(out, 3, "v20.0.0");
    len(
        out,
        4,
        std::env::current_dir()
            .map_or_else(|_| "/".into(), |path| path.to_string_lossy().into_owned()),
    );
    len(out, 5, timestamp);
}

fn mcp_tool(out: &mut Vec<u8>, tool: &Value) {
    let function = &tool["function"];
    for (field, name) in [(1, "name"), (2, "description")] {
        if let Some(value) = nonempty(&function[name]).or_else(|| nonempty(&tool[name])) {
            len(out, field, value);
        }
    }
    let schema = function
        .get("parameters")
        .filter(|schema| truthy(schema))
        .unwrap_or(&tool["input_schema"]);
    let present = match schema {
        Value::Object(fields) => !fields.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::String(text) => !text.is_empty(),
        _ => false,
    };
    if present {
        len(
            out,
            3,
            js_json(schema, false).unwrap_or_else(|| schema.to_string()),
        );
    }
    len(out, 4, "custom");
}

fn tool_result(out: &mut Vec<u8>, result: &Value) {
    let name = nonempty(&result["tool_name"])
        .or_else(|| nonempty(&result["name"]))
        .unwrap_or("tool");
    let name = if let Some(raw) = name.strip_prefix("mcp__") {
        raw.split_once("__").map_or_else(
            || format!("mcp_custom_{}", or(raw, "tool")),
            |(server, tool)| format!("mcp_{}_{}", or(server, "custom"), or(tool, "tool")),
        )
    } else if name.starts_with("mcp_") {
        name.to_owned()
    } else {
        format!("mcp_custom_{name}")
    };
    let raw = name.strip_prefix("mcp_").unwrap_or(&name);
    let (server, selected) = raw.split_once('_').unwrap_or(("custom", raw));
    let selected = or(selected, "tool");
    let server = or(server, "custom");
    let id = result["tool_call_id"].as_str().unwrap_or_default();
    let (call_id, model_id) = id.split_once("\nmc_").unwrap_or((id, ""));
    let index = result["tool_index"]
        .as_u64()
        .filter(|index| *index != 0)
        .or_else(|| result["index"].as_u64().filter(|index| *index != 0))
        .unwrap_or(1);
    let args = nonempty(&result["raw_args"]).unwrap_or("{}");
    let text = nonempty(&result["result_content"])
        .or_else(|| nonempty(&result["result"]))
        .unwrap_or_default();
    len(out, 1, call_id);
    len(out, 2, &name);
    scalar(out, 3, index);
    if !model_id.is_empty() {
        len(out, 12, model_id);
    }
    len(out, 5, args);
    message(out, 8, |out| {
        scalar(out, 1, 19);
        message(out, 28, |out| {
            len(out, 1, selected);
            len(out, 2, text);
        });
        len(out, 35, call_id);
        if !model_id.is_empty() {
            len(out, 48, model_id);
        }
        scalar(out, 49, index);
    });
    message(out, 11, |out| {
        scalar(out, 1, 19);
        message(out, 27, |out| {
            message(out, 1, |out| {
                len(out, 1, selected);
                len(out, 3, args);
                len(out, 4, server);
            });
        });
        len(out, 3, call_id);
        len(out, 9, &name);
        len(out, 10, args);
        scalar(out, 48, index);
        if !model_id.is_empty() {
            len(out, 49, model_id);
        }
    });
}

fn or<'a>(value: &'a str, default: &'a str) -> &'a str {
    if value.is_empty() { default } else { value }
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value > 0x7f {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub(super) fn scalar(out: &mut Vec<u8>, field: u64, value: u64) {
    varint(out, field << 3);
    varint(out, value);
}

pub(super) fn len(out: &mut Vec<u8>, field: u64, bytes: impl AsRef<[u8]>) {
    let bytes = bytes.as_ref();
    varint(out, field << 3 | 2);
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

pub(super) fn message(out: &mut Vec<u8>, field: u64, encode: impl FnOnce(&mut Vec<u8>)) {
    let mut body = Vec::new();
    encode(&mut body);
    len(out, field, body);
}

#[derive(Default)]
struct Frame {
    text: String,
    thinking: String,
    call: Option<Call>,
}

struct Call {
    id: String,
    name: String,
    arguments: String,
    last: bool,
}

impl Call {
    fn as_json(&self, index: Option<usize>) -> Value {
        let mut value = json!({"id":self.id,"type":"function","function":{"name":self.name,"arguments":self.arguments}});
        if let Some(index) = index {
            value["index"] = json!(index);
        }
        value
    }
}

fn bytes(value: Option<Wire<'_>>) -> Option<&[u8]> {
    match value {
        Some(Wire::Bytes(bytes)) => Some(bytes),
        _ => None,
    }
}

fn wire_text(value: Option<Wire<'_>>) -> Cow<'_, str> {
    bytes(value).map_or(Cow::Borrowed(""), String::from_utf8_lossy)
}

fn tool(bytes: &[u8]) -> Option<Call> {
    let [id, name, arguments, last, mcp] = first_fields(bytes, [3, 9, 10, 11, 27])?;
    let id = wire_text(id);
    let id = id.split('\n').next()?;
    let mut name = wire_text(name).into_owned();
    let mut input = String::new();
    if let Some(details) = self::bytes(mcp)
        && let Some([tool]) = first_fields(details, [1])
        && let Some(tool) = self::bytes(tool)
        && let Some([actual_name, args]) = first_fields(tool, [1, 3])
    {
        if actual_name.is_some() {
            name = wire_text(actual_name).into_owned();
        }
        input = wire_text(args).into_owned();
    }
    if input.is_empty() {
        input = wire_text(arguments).into_owned();
    }
    if id.is_empty() || name.is_empty() {
        return None;
    }
    Some(Call {
        id: id.to_owned(),
        name,
        arguments: if input.is_empty() { "{}".into() } else { input },
        last: matches!(last, Some(Wire::Varint(value)) if value != 0),
    })
}

fn decode(payload: &[u8]) -> Option<Frame> {
    let [call, response] = first_fields(payload, [1, 2])?;
    if let Some(call) = bytes(call).and_then(tool) {
        return Some(Frame {
            call: Some(call),
            ..Frame::default()
        });
    }
    let [text, thinking] = first_fields(bytes(response)?, [1, 25])?;
    let thinking = bytes(thinking)
        .and_then(|thinking| first_fields(thinking, [1]))
        .map_or_else(String::new, |[text]| wire_text(text).into_owned());
    Some(Frame {
        text: wire_text(text).into_owned(),
        thinking,
        call: None,
    })
}

fn payload(flags: u8, bytes: &[u8]) -> Cow<'_, [u8]> {
    fn read(decoder: impl Read) -> Option<Vec<u8>> {
        let mut output = Vec::new();
        decoder
            .take(FRAME_LIMIT as u64 + 1)
            .read_to_end(&mut output)
            .ok()?;
        (output.len() <= FRAME_LIMIT).then_some(output)
    }
    if !matches!(flags, 1..=3) || bytes.starts_with(b"{\"error\"") {
        return Cow::Borrowed(bytes);
    }
    read(GzDecoder::new(bytes))
        .or_else(|| read(ZlibDecoder::new(bytes)))
        .or_else(|| read(DeflateDecoder::new(bytes)))
        .map_or(Cow::Borrowed(bytes), Cow::Owned)
}

pub(super) enum Rendered {
    Json(u16, Value),
    Sse(String),
}

pub(super) fn render(mut buffer: &[u8], model: &str, body: &Value, stream: bool) -> Rendered {
    let now = chrono::Utc::now();
    let id = format!("chatcmpl-cursor-{}", now.timestamp_millis());
    let created = now.timestamp();
    let composer = model
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let composer = composer == "composer" || composer.starts_with("composer-");
    let mut calls = Vec::<Call>::new();
    let mut finalized = Vec::new();
    let mut text = String::new();
    let mut thinking = String::new();
    let mut emitted = 0;
    let mut output = String::new();
    let chunk = |output: &mut String, delta: Value| {
        emit(
            output,
            json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,
            "choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        );
    };
    while let Some(header) = buffer.get(..5) {
        let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let Some((raw, rest)) = buffer[5..].split_at_checked(length) else {
            break;
        };
        buffer = rest;
        let raw = payload(header[0], raw);
        if raw.first() == Some(&b'{')
            && let Ok(error) = serde_json::from_slice::<Value>(&raw)
            && error.get("error").is_some()
        {
            if !text.is_empty() || !calls.is_empty() || stream && !output.is_empty() {
                break;
            }
            let info = &error["error"];
            let debug = &info["details"][0]["debug"];
            let limited = info["code"] == "resource_exhausted";
            let message = [
                &debug["details"]["title"],
                &debug["details"]["detail"],
                &info["message"],
            ]
            .into_iter()
            .find(|value| truthy(value))
            .cloned()
            .unwrap_or(json!("API Error"));
            let code = debug
                .get("error")
                .filter(|value| truthy(value))
                .cloned()
                .unwrap_or(json!("unknown"));
            return Rendered::Json(
                if limited { 429 } else { 400 },
                json!({"error":{
                    "message":message,"type":if limited {"rate_limit_error"} else {"api_error"},"code":code,
                }}),
            );
        }
        let Some(frame) = decode(&raw) else {
            continue;
        };
        if let Some(call) = frame.call {
            if stream && output.is_empty() {
                chunk(&mut output, json!({"role":"assistant","content":""}));
            }
            if let Some(index) = calls.iter().position(|existing| existing.id == call.id) {
                if stream && !call.arguments.is_empty() {
                    chunk(
                        &mut output,
                        json!({"tool_calls":[call.as_json(Some(index))]}),
                    );
                }
                calls[index].arguments.push_str(&call.arguments);
                if !stream && call.last {
                    finalized.push(calls[index].as_json(None));
                }
            } else {
                if stream {
                    chunk(
                        &mut output,
                        json!({"tool_calls":[call.as_json(Some(calls.len()))]}),
                    );
                } else if call.last {
                    finalized.push(call.as_json(None));
                }
                calls.push(call);
            }
        }
        if !frame.text.is_empty() {
            text.push_str(&frame.text);
            if stream {
                let delta = if output.is_empty() {
                    json!({"role":"assistant","content":frame.text})
                } else {
                    json!({"content":frame.text})
                };
                chunk(&mut output, delta);
            }
        }
        if composer && !frame.thinking.is_empty() {
            thinking.push_str(&frame.thinking);
            if stream {
                let visible = visible_thinking(&thinking)
                    .encode_utf16()
                    .collect::<Vec<_>>();
                if let Some(delta) = visible.get(emitted..).filter(|delta| !delta.is_empty()) {
                    emitted = visible.len();
                    let delta = String::from_utf16_lossy(delta);
                    text.push_str(&delta);
                    let outgoing = if output.is_empty() {
                        json!({"role":"assistant","content":delta})
                    } else {
                        json!({"content":delta})
                    };
                    chunk(&mut output, outgoing);
                }
            }
        }
    }
    let prompt = (js_json(body, false)
        .unwrap_or_else(|| body.to_string())
        .encode_utf16()
        .count())
    .div_ceil(4);
    if stream {
        if output.is_empty() {
            chunk(&mut output, json!({"role":"assistant","content":""}));
        }
        let usage = estimated_usage(prompt, text.encode_utf16().count());
        emit(
            &mut output,
            json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,
            "choices":[{"index":0,"delta":{},"finish_reason":if calls.is_empty(){"stop"}else{"tool_calls"}}],"usage":usage}),
        );
        output.push_str("data: [DONE]\n\n");
        return Rendered::Sse(output);
    }
    for call in &calls {
        if !finalized.iter().any(|item| item["id"] == call.id) {
            finalized.push(call.as_json(None));
        }
    }
    let text = if text.is_empty() && composer {
        visible_thinking(&thinking)
    } else {
        &text
    };
    let mut message = json!({"role":"assistant","content":(!text.is_empty()).then_some(text)});
    let reason = if finalized.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    if !finalized.is_empty() {
        message["tool_calls"] = json!(finalized);
    }
    Rendered::Json(
        200,
        json!({"id":id,"object":"chat.completion","created":created,"model":model,
        "choices":[{"index":0,"message":message,"finish_reason":reason}],
        "usage":estimated_usage(prompt,text.encode_utf16().count())}),
    )
}

fn visible_thinking(thinking: &str) -> &str {
    thinking
        .rsplit_once("</think>")
        .map_or("", |(_, text)| text.trim_start_matches(js_space))
}

fn emit(output: &mut String, value: Value) {
    output.push_str("data: ");
    output.push_str(&value.to_string());
    output.push_str("\n\n");
}
