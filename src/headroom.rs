use std::time::Duration;

use serde_json::{Value, json};
use url::Url;

use crate::chat_format::ChatFormat;
use crate::chat_input::{js_json, js_space, truthy};
use crate::{chat_protocol_input, responses_request};

#[derive(Clone, Copy)]
pub enum Wire {
    Chat,
    Claude,
    Responses,
    Gemini,
    Kiro,
    CommandCode,
}

pub struct Config {
    endpoint: Url,
    timeout: Duration,
    compress_users: bool,
}

pub fn config(settings: &Value, token_saver: &str) -> Option<Config> {
    if token_saver.eq_ignore_ascii_case("off") || !truthy(&settings["headroomEnabled"]) {
        return None;
    }
    let from_env = std::env::var("HEADROOM_URL").ok();
    let base = settings["headroomUrl"]
        .as_str()
        .filter(|url| !url.is_empty())
        .or(from_env.as_deref())
        .unwrap_or("http://localhost:8787");
    let mut endpoint = Url::parse(base).ok()?;
    if !matches!(endpoint.scheme(), "http" | "https") {
        return None;
    }
    let path = endpoint.path().strip_suffix('/').unwrap_or(endpoint.path());
    endpoint.set_path(&format!("{path}/v1/compress"));
    endpoint.set_fragment(None);
    let millis = settings["headroomTimeoutMs"]
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(3000.0);
    let timeout = Duration::try_from_secs_f64(millis / 1000.0).ok()?;
    Some(Config {
        endpoint,
        timeout,
        compress_users: truthy(&settings["headroomCompressUserMessages"]),
    })
}

pub async fn apply(
    client: &reqwest::Client,
    config: &Config,
    body: &mut Value,
    wire: Wire,
    model: &str,
) {
    match wire {
        Wire::Chat => direct(client, config, body, model).await,
        Wire::Claude => {
            if let Some(projected) = claude(body) {
                compress_projection(client, config, body, model, projected).await;
            }
        }
        Wire::Kiro => {
            compress_projection(client, config, body, model, kiro(body)).await;
        }
        Wire::CommandCode => {
            compress_projection(client, config, body, model, commandcode(body)).await;
        }
        Wire::Gemini => {
            compress_projection(client, config, body, model, extract_tool_output(body, wire)).await;
        }
        Wire::Responses
            if body["input"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["type"]
                        .as_str()
                        .is_some_and(|type_| type_ != "message")
                })
            }) =>
        {
            compress_projection(client, config, body, model, extract_tool_output(body, wire)).await;
        }
        Wire::Responses => {
            let Some(mut chat) =
                chat_protocol_input::translate(ChatFormat::Responses, body.clone(), "")
            else {
                return;
            };
            let Some(messages) = chat["messages"].as_array() else {
                return;
            };
            let Some(compressed) = request(client, config, model, messages).await else {
                return;
            };
            chat["messages"] = Value::Array(compressed);
            if let Some(fields) = chat.as_object_mut() {
                fields.shift_remove("input");
            }
            if let Some(prepared) = responses_request::prepare(ChatFormat::Chat, chat, model, "")
                && prepared.body["input"].is_array()
            {
                body["input"] = prepared.body["input"].clone();
            }
        }
    }
}

async fn request(
    client: &reqwest::Client,
    config: &Config,
    model: &str,
    messages: &[Value],
) -> Option<Vec<Value>> {
    let mut payload = json!({ "messages": messages, "model": model });
    if config.compress_users {
        payload["config"] = json!({ "compress_user_messages": true });
    }
    let result = client
        .post(config.endpoint.clone())
        .timeout(config.timeout)
        .json(&payload)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<Value>()
        .await
        .ok()?;
    result["messages"].as_array().cloned()
}

async fn direct(client: &reqwest::Client, config: &Config, body: &mut Value, model: &str) {
    let key = if body["messages"].is_array() {
        "messages"
    } else {
        "input"
    };
    let Some(messages) = body[key].as_array() else {
        return;
    };
    if let Some(compressed) = request(client, config, model, messages).await {
        body[key] = Value::Array(compressed);
    }
}

struct Projected {
    messages: Vec<Value>,
    paths: Vec<Option<String>>,
    plain_tools_only: bool,
}

impl Projected {
    fn new(plain_tools_only: bool) -> Self {
        Self {
            messages: Vec::new(),
            paths: Vec::new(),
            plain_tools_only,
        }
    }

    fn add(&mut self, role: &Value, value: &Value, path: String, tool: Option<&Value>) {
        let Some(text) = value.as_str() else { return };
        let mut message = json!({ "content": text });
        if !role.is_null() {
            message["role"] = role.clone();
        }
        if let Some(id) = tool {
            message["tool_call_id"] = id.clone();
        }
        self.messages.push(message);
        self.paths.push(Some(path));
    }

    fn add_content(&mut self, role: &Value, content: &Value, path: String, tool: Option<&Value>) {
        if let Some(parts) = content.as_array() {
            for (index, part) in parts.iter().enumerate() {
                if part["type"] == "text" {
                    self.add(role, &part["text"], format!("{path}/{index}/text"), tool);
                }
            }
        } else {
            self.add(role, content, path, tool);
        }
    }

    fn visit_tool_output(&mut self, value: &Value, path: String, depth: usize) {
        if depth > 8 {
            return;
        }
        match value {
            Value::String(text)
                if !text.trim_matches(js_space).is_empty()
                    && serde_json::from_str::<Value>(text).is_err() =>
            {
                let id = json!(format!("headroom-tool-{}", self.messages.len()));
                self.add(&json!("tool"), value, path, Some(&id));
            }
            Value::Array(parts) => {
                for (index, part) in parts.iter().enumerate() {
                    self.visit_tool_output(part, format!("{path}/{index}"), depth + 1);
                }
            }
            Value::Object(fields)
                if !truthy(&value["type"])
                    || matches!(value["type"].as_str(), Some("text" | "input_text")) =>
            {
                for field in ["result", "output", "content", "text"] {
                    if let Some(value) = fields.get(field) {
                        self.visit_tool_output(value, format!("{path}/{field}"), depth + 1);
                    }
                }
            }
            _ => (),
        }
    }
}

fn claude(body: &Value) -> Option<Projected> {
    let messages = body["messages"].as_array()?;
    let mut output = Projected::new(false);
    output.add_content(&json!("system"), &body["system"], "/system".into(), None);
    for (index, message) in messages.iter().enumerate() {
        let paths = format!("/messages/{index}/content");
        let blocks = message["content"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        if message["role"] == "assistant" {
            let calls = blocks.iter().filter(|block| block["type"] == "tool_use").map(|block| {
                let args = if truthy(&block["input"]) { block["input"].clone() } else { json!({}) };
                let mut call = json!({
                    "type":"function",
                    "function":{"arguments":js_json(&args,false).unwrap_or_else(||args.to_string())},
                });
                if let Some(id) = block.get("id") { call["id"] = id.clone(); }
                if let Some(name) = block.get("name") { call["function"]["name"] = name.clone(); }
                call
            }).collect::<Vec<_>>();
            if !calls.is_empty() {
                output
                    .messages
                    .push(json!({"role":"assistant","content":"","tool_calls":calls}));
                output.paths.push(None);
            }
        }
        output.add_content(&message["role"], &message["content"], paths.clone(), None);
        for (block, item) in blocks
            .iter()
            .enumerate()
            .filter(|(_, item)| item["type"] == "tool_result")
        {
            output.add_content(
                &json!("tool"),
                &item["content"],
                format!("{paths}/{block}/content"),
                item.get("tool_use_id"),
            );
        }
    }
    output.paths.iter().any(Option::is_some).then_some(output)
}

fn kiro(body: &Value) -> Projected {
    let state = &body["conversationState"];
    let mut output = Projected::new(false);
    for (index, item) in state["history"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        project_kiro_turn(
            &mut output,
            item,
            &format!("/conversationState/history/{index}"),
        );
    }
    project_kiro_turn(
        &mut output,
        &state["currentMessage"],
        "/conversationState/currentMessage",
    );
    output
}

fn project_kiro_turn(output: &mut Projected, turn: &Value, path: &str) {
    if let Some(user) = turn.get("userInputMessage").filter(|user| !user.is_null()) {
        let path = format!("{path}/userInputMessage");
        output.add(
            &json!("system"),
            &user["systemInstruction"],
            format!("{path}/systemInstruction"),
            None,
        );
        output.add(
            &json!("user"),
            &user["content"],
            format!("{path}/content"),
            None,
        );
        for (index, result) in user["userInputMessageContext"]["toolResults"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let id = result.get("toolUseId").filter(|value| truthy(value));
            for (part, content) in result["content"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                output.add(
                    &json!("tool"),
                    &content["text"],
                    format!(
                        "{path}/userInputMessageContext/toolResults/{index}/content/{part}/text"
                    ),
                    id,
                );
            }
        }
        return;
    }
    let assistant = &turn["assistantResponseMessage"];
    if !assistant["content"].is_string() {
        return;
    }
    output.add(
        &json!("assistant"),
        &assistant["content"],
        format!("{path}/assistantResponseMessage/content"),
        None,
    );
    let calls: Vec<_> = assistant["toolUses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|call| truthy(&call["toolUseId"]) || truthy(&call["name"]))
        .map(|call| {
            let input = if truthy(&call["input"]) {
                call["input"].clone()
            } else {
                json!({})
            };
            let mut result = json!({
                "type":"function",
                "function":{
                    "name":call["name"].as_str().unwrap_or_default(),
                    "arguments":js_json(&input, false).unwrap_or_else(|| input.to_string()),
                },
            });
            if let Some(id) = call.get("toolUseId") {
                result["id"] = id.clone();
            }
            result
        })
        .collect();
    if !calls.is_empty()
        && let Some(message) = output.messages.last_mut()
    {
        message["tool_calls"] = Value::Array(calls);
    }
}

fn commandcode(body: &Value) -> Projected {
    let params = &body["params"];
    let mut output = Projected::new(false);
    output.add(
        &json!("system"),
        &params["system"],
        "/params/system".into(),
        None,
    );
    for (index, message) in params["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(parts) = message["content"].as_array() else {
            continue;
        };
        if message["role"] == "assistant" {
            let calls: Vec<_> = parts
                .iter()
                .filter(|part| part["type"] == "tool-call")
                .map(|part| {
                    let empty = json!({});
                    let input = part
                        .get("input")
                        .filter(|value| !value.is_null())
                        .unwrap_or(&empty);
                    json!({
                        "id":part["toolCallId"],
                        "type":"function",
                        "function":{
                            "name":part["toolName"],
                            "arguments":js_json(input, false).unwrap_or_else(|| input.to_string()),
                        },
                    })
                })
                .collect();
            if !calls.is_empty() {
                output
                    .messages
                    .push(json!({"role":"assistant","content":"","tool_calls":calls}));
                output.paths.push(None);
            }
        }
        for (part_index, part) in parts.iter().enumerate() {
            let path = format!("/params/messages/{index}/content/{part_index}");
            if part["type"] == "text" {
                output.add(
                    &message["role"],
                    &part["text"],
                    format!("{path}/text"),
                    None,
                );
            } else if part["type"] == "tool-result"
                && part["output"]["type"] == "text"
                && part["output"]["value"].is_string()
            {
                output.add(
                    &json!("tool"),
                    &part["output"]["value"],
                    format!("{path}/output/value"),
                    part.get("toolCallId"),
                );
                if let (Some(name), Some(last)) = (part.get("toolName"), output.messages.last_mut())
                {
                    last["name"] = name.clone();
                }
            }
        }
    }
    output
}

fn extract_tool_output(body: &Value, wire: Wire) -> Projected {
    let mut output = Projected::new(true);
    match wire {
        Wire::Responses => {
            for (index, item) in body["input"].as_array().into_iter().flatten().enumerate() {
                if item["type"] == "function_call_output" {
                    output.visit_tool_output(&item["output"], format!("/input/{index}/output"), 0);
                }
            }
        }
        Wire::Gemini => {
            let (contents, path) = body["request"]["contents"]
                .as_array()
                .map(|contents| (Some(contents), "/request/contents"))
                .unwrap_or_else(|| (body["contents"].as_array(), "/contents"));
            for (index, message) in contents.into_iter().flatten().enumerate() {
                for (part, item) in message["parts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    output.visit_tool_output(
                        &item["functionResponse"]["response"],
                        format!("{path}/{index}/parts/{part}/functionResponse/response"),
                        0,
                    );
                }
            }
        }
        _ => (),
    }
    output
}

async fn compress_projection(
    client: &reqwest::Client,
    config: &Config,
    body: &mut Value,
    model: &str,
    projection: Projected,
) {
    if projection.messages.is_empty() {
        return;
    }
    let Some(messages) = request(client, config, model, &projection.messages).await else {
        return;
    };
    if messages.len() != projection.messages.len() {
        return;
    }
    let mut update = Vec::new();
    for ((reply, expected), path) in messages
        .into_iter()
        .zip(projection.messages)
        .zip(projection.paths)
    {
        if !reply.is_object()
            || reply.get("role") != expected.get("role")
            || reply.get("tool_call_id") != expected.get("tool_call_id")
        {
            return;
        }
        let Some(text) = content(&reply["content"], projection.plain_tools_only) else {
            return;
        };
        if projection.plain_tools_only {
            let Some(original) = expected["content"].as_str() else {
                return;
            };
            if text.trim_matches(js_space).is_empty()
                || text.encode_utf16().count() >= original.encode_utf16().count()
            {
                continue;
            }
        }
        if let Some(path) = path {
            update.push((path, text));
        }
    }
    for (path, text) in update {
        if let Some(target) = body.pointer_mut(&path) {
            *target = Value::String(text);
        }
    }
}

fn content(value: &Value, plain_only: bool) -> Option<String> {
    if let Some(value) = value.as_str() {
        return Some(value.to_owned());
    }
    if plain_only {
        return None;
    }
    let parts = value
        .as_array()?
        .iter()
        .filter_map(|item| item.as_str().or(item["text"].as_str()))
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("\n"))
}
