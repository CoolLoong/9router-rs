use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use futures_util::{StreamExt, stream};
use provider_capabilities_rs::port::model_capabilities;
use serde_json::{Value, json};

use crate::chat_format::ChatFormat;
use crate::chat_input::{PreparedChat, js_space, truthy};
use crate::responses_request;
use crate::thinking_intent::{Thinking, budget_level, model_suffix, thinking_intent};

const SUPPORTED: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "stream",
    "store",
    "reasoning",
    "service_tier",
    "include",
    "prompt_cache_key",
    "client_metadata",
    "text",
    "parallel_tool_calls",
];
const HOSTED: &[&str] = &[
    "image_generation",
    "web_search",
    "web_search_preview",
    "file_search",
    "computer",
    "computer_use_preview",
    "code_interpreter",
    "mcp",
    "local_shell",
    "tool_search",
    "custom",
    "namespace",
];
const OVERLOADED: &[&str] = &["server_is_overloaded", "service_unavailable_error"];
const CAPACITY: &[&str] = &["selected model is at capacity", "model_at_capacity"];
static CONFIG: OnceLock<Value> = OnceLock::new();

fn config() -> &'static Value {
    CONFIG.get_or_init(|| {
        serde_json::from_str(include_str!("codex-config.json")).expect("embedded Codex transport")
    })
}

pub(super) fn cli_version() -> &'static str {
    config()["version"].as_str().unwrap_or_default()
}

pub fn prepare(
    client: ChatFormat,
    mut body: Value,
    model: &str,
    accept: &str,
    native_client: bool,
) -> Option<PreparedChat> {
    if !body.is_object() || contains_remote_image(&body) {
        return None;
    }
    if client == ChatFormat::Responses {
        if let Some(effort) = request_effort(model, (!native_client).then_some(&body)) {
            if !native_client {
                crate::claude_thinking::strip(&mut body);
            }
            if !body["reasoning"].is_object() {
                body["reasoning"] = json!({});
            }
            body["reasoning"]["effort"] = json!(effort);
        }
        body["model"] = json!(model);
        return Some(PreparedChat { body, stream: true });
    }

    let messages_effort = if client == ChatFormat::Messages
        && (body.get("thinking").is_some() || body.get("output_config").is_some())
    {
        let effort = request_effort(model, Some(&body));
        crate::claude_thinking::strip(&mut body);
        effort
    } else {
        None
    };
    let fields = body.as_object_mut()?;
    let effort = fields.shift_remove("reasoning_effort");
    let reasoning = fields.shift_remove("reasoning");
    if fields.contains_key("thinking") || fields.contains_key("enable_thinking") {
        return None;
    }
    let base = split_suffix(model).0;
    let mut converted = responses_request::prepare(client, body, base, accept)?;
    if let Some(reasoning) = reasoning {
        converted.body["reasoning"] = reasoning;
    }
    if let Some(effort) = messages_effort.map(Value::String).or(effort) {
        converted.body["reasoning_effort"] = effort;
    }
    converted.body["model"] = json!(model);
    converted.stream = true;
    Some(converted)
}

fn request_effort(model: &str, body: Option<&Value>) -> Option<String> {
    let (base, suffix) = model_suffix(model);
    if model_capabilities(Some("codex"), base, None)["reasoning"] != true {
        return None;
    }
    let intent = suffix.or_else(|| body.and_then(thinking_intent))?;
    Some(match intent {
        Thinking::None => "none".to_owned(),
        Thinking::Auto => "auto".to_owned(),
        Thinking::Level(level) => level,
        Thinking::Budget(budget) => budget_level(budget).to_owned(),
    })
}

fn contains_remote_image(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(contains_remote_image),
        Value::Object(fields) => {
            if matches!(value["type"].as_str(), Some("input_image" | "image_url")) {
                let url = value["image_url"]
                    .as_str()
                    .or_else(|| value["image_url"]["url"].as_str());
                return url.is_some_and(|url| !url.starts_with("data:"));
            }
            fields.values().any(contains_remote_image)
        }
        _ => false,
    }
}

pub(super) fn normalize_images(body: &mut Value, format: ChatFormat) {
    let field = match format {
        ChatFormat::Chat => "messages",
        ChatFormat::Responses => "input",
        ChatFormat::Messages => return,
    };
    let Some(items) = body.get_mut(field).and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in parts.iter_mut().filter(|part| part["type"] == "image_url") {
            let image = &part["image_url"];
            if format == ChatFormat::Chat {
                if let Some(url) = image.as_str() {
                    part["image_url"] = json!({"url":url});
                }
            } else if let Some(url) = image.as_str().or_else(|| image["url"].as_str()) {
                *part = json!({
                    "type":"input_image",
                    "image_url":url,
                    "detail":image["detail"].as_str().filter(|detail| !detail.is_empty()).unwrap_or("auto")
                });
            }
        }
    }
}

fn split_suffix(model: &str) -> (&str, Option<&str>) {
    model
        .trim_end_matches(js_space)
        .strip_suffix(')')
        .and_then(|value| value.rsplit_once('('))
        .filter(|(_, effort)| !effort.is_empty() && !effort.contains(['(', ')']))
        .map_or((model, None), |(model, effort)| {
            (
                model.trim_matches(js_space),
                Some(effort.trim_matches(js_space)),
            )
        })
}

#[derive(Default, Clone)]
pub struct Sessions {
    by_account: Arc<Mutex<HashMap<String, String>>>,
}

impl Sessions {
    pub(crate) fn pick(
        &self,
        body: &Value,
        account: &Value,
        headers: &HeaderMap,
        caller: Option<&str>,
    ) -> String {
        use crate::gemini_signatures::{assistant_prefix, implicit_session, session_id};
        if let Some(session) = session_id(headers, body) {
            return session;
        }
        let id = account["id"].as_str().unwrap_or("default");
        let provider = account["provider"].as_str().unwrap_or("codex");
        let history = assistant_prefix(body);
        if !history.is_empty() {
            return implicit_session(provider, id, caller, &history);
        }
        if let Some(workspace) = account["providerSpecificData"]["workspaceId"]
            .as_str()
            .map(|id| id.trim_matches(js_space))
            .filter(|id| !id.is_empty() && id.encode_utf16().count() <= 256)
        {
            return workspace.to_owned();
        }
        if caller.is_some() {
            return implicit_session(provider, id, caller, &history);
        }
        let mut slots = self
            .by_account
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        slots
            .entry(id.to_owned())
            .or_insert_with(|| {
                format!(
                    "{}{}",
                    uuid::Uuid::new_v4(),
                    chrono::Utc::now().timestamp_millis()
                )
            })
            .clone()
    }
}

pub struct Exchange {
    pub compact: bool,
    lite_header: bool,
    session: String,
    account_id: Option<String>,
}

impl Exchange {
    pub fn refresh_binding(&mut self, account: &Value) {
        self.account_id = account_binding(account);
    }
    pub fn url(&self) -> String {
        endpoint(self.compact)
    }

    pub fn fixture_url(&self) -> Option<String> {
        (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")).then(|| {
            let base = "http://127.0.0.1:52989/codex/backend-api/codex/responses";
            if self.compact {
                format!("{base}/compact")
            } else {
                base.to_owned()
            }
        })
    }

    pub fn headers(
        &self,
        mut request: reqwest::RequestBuilder,
        token: &str,
    ) -> reqwest::RequestBuilder {
        let version = cli_version();
        request = request
            .bearer_auth(token)
            .header("session_id", &self.session)
            .header("originator", "codex_cli_rs")
            .header("version", version)
            .header("user-agent", format!("codex_cli_rs/{version}"));
        if self.lite_header {
            request = request.header("x-openai-internal-codex-responses-lite", "true");
        }
        if let Some(account) = &self.account_id {
            request = request.header("ChatGPT-Account-ID", account);
        }
        request
    }
}

pub(super) fn endpoint(compact: bool) -> String {
    let base = config()["endpoint"].as_str().unwrap_or_default();
    if compact {
        format!("{base}/compact")
    } else {
        base.to_owned()
    }
}

pub fn finish(
    body: &mut Value,
    model: &str,
    account: &Value,
    session: String,
    client: ChatFormat,
    compact_path: bool,
) -> Option<Exchange> {
    let mut compact = compact_path;
    if let Some(value) = body.as_object_mut()?.shift_remove("_compact") {
        compact |= truthy(&value);
    }
    let account_id = account_binding(account);
    let (base, suffix) = split_suffix(model);
    let models = &config()["models"];
    let upstream = models[base]["upstream"]
        .as_str()
        .or_else(|| {
            (base != "codex-auto-review")
                .then(|| base.strip_suffix("-review"))
                .flatten()
        })
        .unwrap_or(base);
    let lite_model = models[upstream]["lite"] == true;

    if let Some(value) = body["input"].as_str() {
        let value = if value.trim_matches(js_space).is_empty() {
            "..."
        } else {
            value
        };
        body["input"] = json!([user_turn(value)]);
    }
    let auto_search =
        body.as_object_mut()?.shift_remove("_autoCodexWebSearch") == Some(Value::Bool(true));
    if auto_search && !has_search(&body["tools"]) {
        if !body["tools"].is_array() {
            body["tools"] = json!([])
        }
        body["tools"]
            .as_array_mut()?
            .push(json!({"type":"web_search"}));
    }

    let mut converted_prefix = false;
    if lite_model
        && (has_search(&body["tools"])
            || body["input"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["type"] == "additional_tools" && has_search(&item["tools"]))
            }))
    {
        let mut tools = body["tools"].as_array().cloned().unwrap_or_default();
        let mut seen = tools.iter().map(tool_name).collect::<HashSet<_>>();
        if let Some(items) = body["input"].as_array_mut() {
            items.retain(|item| {
                if item["type"] != "additional_tools" {
                    return true;
                }
                converted_prefix = true;
                for tool in item["tools"].as_array().into_iter().flatten() {
                    if seen.insert(tool_name(tool)) {
                        tools.push(tool.clone());
                    }
                }
                false
            });
        }
        body["tools"] = Value::Array(tools);
    }
    let lite = lite_model && !has_search(&body["tools"]);
    if !truthy(&body["input"]) || body["input"].as_array().is_some_and(Vec::is_empty) {
        body["input"] = json!([user_turn("...")]);
    }
    if let Some(items) = body["input"].as_array_mut() {
        items.retain_mut(|item| {
            if item.as_str().is_some_and(server_id) || item["type"] == "item_reference" {
                return false;
            }
            if item["role"] == "system" && (!truthy(&item["type"]) || item["type"] == "message") {
                item["role"] = json!("developer");
            }
            if item["id"].as_str().is_some_and(|id| {
                server_id(id) && !(lite && item["role"] == "developer" && id.starts_with("msg_"))
            }) && let Some(object) = item.as_object_mut()
            {
                object.shift_remove("id");
            }
            true
        });
    }
    normalize_images(body, ChatFormat::Responses);
    normalize_tools(body);
    body["stream"] = json!(true);
    body["store"] = json!(false);
    if !truthy(&body["prompt_cache_key"]) {
        body["prompt_cache_key"] = json!(session);
    }
    let instruction = config()["instructions"].as_str().unwrap_or_default();
    if !lite
        && !converted_prefix
        && body["instructions"]
            .as_str()
            .is_none_or(|value| value.trim_matches(js_space).is_empty())
    {
        body["instructions"] = json!(instruction);
    }
    if lite {
        let instruction_prefix = body["instructions"]
            .as_str()
            .filter(|value| !value.trim_matches(js_space).is_empty())
            .unwrap_or(instruction)
            .to_owned();
        let tools = body["tools"].as_array().cloned().unwrap_or_default();
        if !body["input"].is_array() {
            body["input"] = json!([std::mem::take(&mut body["input"])]);
        }
        let items = body["input"].as_array_mut()?;
        if !items.iter().any(|item| item["type"] == "additional_tools") {
            items.insert(
                0,
                json!({"type":"additional_tools","role":"developer","tools":tools}),
            );
            if !instruction_prefix.is_empty() {
                items.insert(1, json!({"type":"message","role":"developer","content":[{"type":"input_text","text":instruction_prefix}]}));
            }
        }
        body["instructions"] = json!("");
        body["tools"] = Value::Null;
        if !truthy(&body["tool_choice"]) {
            body["tool_choice"] = json!("auto");
        }
        body["parallel_tool_calls"] = json!(false);
    } else {
        body.as_object_mut()?.shift_remove("parallel_tool_calls");
    }

    let effort = ["none", "minimal", "low", "medium", "high", "xhigh"]
        .into_iter()
        .find(|level| upstream.ends_with(&format!("-{level}")));
    let requested = body["reasoning"]["effort"]
        .as_str()
        .or(body["reasoning_effort"].as_str())
        .or(suffix)
        .or(effort)
        .unwrap_or(if lite { "medium" } else { "low" });
    let upstream = if let Some(level) = effort {
        upstream.replacen(&format!("-{level}"), "", 1)
    } else {
        upstream.to_owned()
    };
    let supported = models[&upstream]["levels"].as_array();
    let normalized = if supported
        .is_some_and(|levels| levels.iter().any(|entry| entry == requested))
    {
        requested
    } else if lite && client != ChatFormat::Messages && matches!(requested, "none" | "minimal") {
        "low"
    } else if requested == "ultra"
        && supported.is_some_and(|levels| levels.iter().any(|entry| entry == "max"))
    {
        "max"
    } else if matches!(requested, "max" | "ultra") {
        "xhigh"
    } else {
        requested
    }
    .to_owned();
    body["model"] = json!(upstream);
    if !body["reasoning"].is_object() {
        body["reasoning"] = json!({});
    }
    body["reasoning"]["effort"] = json!(normalized);
    if lite {
        body["reasoning"]["context"] = json!("all_turns");
    } else if !truthy(&body["reasoning"]["summary"]) {
        body["reasoning"]["summary"] = json!("auto");
    }
    if normalized != "none" && !normalized.is_empty() {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if body["service_tier"] == "fast" {
        body["service_tier"] = json!("priority");
    } else if truthy(&body["service_tier"]) && body["service_tier"] != "priority" {
        body.as_object_mut()?.shift_remove("service_tier");
    }
    body.as_object_mut()?
        .retain(|key, _| SUPPORTED.contains(&key.as_str()));
    Some(Exchange {
        compact,
        lite_header: lite,
        session,
        account_id,
    })
}

fn account_binding(account: &Value) -> Option<String> {
    ["workspaceId", "chatgptAccountId", "accountId"]
        .iter()
        .find_map(|name| {
            account["providerSpecificData"][*name]
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
}

fn user_turn(value: &str) -> Value {
    json!({"type":"message","role":"user","content":[{"type":"input_text","text":value}]})
}

fn tool_name(tool: &Value) -> String {
    format!(
        "{}:{}",
        tool["type"].as_str().unwrap_or("undefined"),
        tool["name"]
            .as_str()
            .or_else(|| tool["function"]["name"].as_str())
            .unwrap_or_default()
    )
}

fn has_search(tools: &Value) -> bool {
    tools
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["type"] == "web_search"))
}

fn server_id(value: &str) -> bool {
    ["rs_", "fc_", "resp_", "msg_"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

fn normalize_tools(body: &mut Value) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    let mut names = HashSet::new();
    tools.retain_mut(|tool| {
        if !tool.is_object() {
            return false;
        }
        let kind = tool["type"].as_str().unwrap_or_default();
        if kind == "namespace" {
            if let Some(children) = tool["tools"].as_array_mut() {
                for child in children {
                    let name = child["name"].as_str().map(tool_label).unwrap_or_default();
                    if !name.is_empty() {
                        names.insert(name);
                    }
                    if let Some(schema) = child.get_mut("parameters") {
                        strip_schema_pattern(schema);
                    }
                }
            }
            return true;
        }
        if kind != "function" {
            return HOSTED.contains(&kind)
                && (kind == "custom" || !truthy(&tool["function"]) && !tool["name"].is_string());
        }
        let function = &tool["function"];
        let name = tool["name"]
            .as_str()
            .or(function["name"].as_str())
            .unwrap_or_default()
            .trim_matches(js_space);
        if name.is_empty() {
            return false;
        }
        names.insert(name.to_owned());
        let description = tool["description"]
            .as_str()
            .or(function["description"].as_str())
            .unwrap_or_default();
        let mut parameters = tool
            .get("parameters")
            .filter(|value| value.is_object())
            .or_else(|| function.get("parameters").filter(|value| value.is_object()))
            .cloned()
            .unwrap_or(json!({"type":"object","properties":{}}));
        strip_schema_pattern(&mut parameters);
        let strict = tool["strict"]
            .as_bool()
            .or_else(|| function["strict"].as_bool());
        let mut value = json!({"type":"function","name":tool_label(name)});
        if !description.is_empty() {
            value["description"] = json!(description);
        }
        value["parameters"] = parameters;
        if let Some(strict) = strict {
            value["strict"] = json!(strict);
        }
        *tool = value;
        true
    });
    if body["tool_choice"]["type"] == "function"
        && body["tool_choice"]["name"]
            .as_str()
            .is_none_or(|name| !names.contains(name.trim_matches(js_space)))
        && let Some(fields) = body.as_object_mut()
    {
        fields.shift_remove("tool_choice");
    }
}

fn tool_label(name: &str) -> String {
    String::from_utf16_lossy(
        &name
            .trim_matches(js_space)
            .encode_utf16()
            .take(128)
            .collect::<Vec<_>>(),
    )
}

fn strip_schema_pattern(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(strip_schema_pattern),
        Value::Object(fields) => {
            if fields
                .get("pattern")
                .and_then(Value::as_str)
                .is_some_and(property_escape)
            {
                fields.shift_remove("pattern");
            }
            for (key, value) in fields {
                if key == "properties"
                    && let Some(properties) = value.as_object_mut()
                {
                    properties.values_mut().for_each(strip_schema_pattern);
                } else {
                    strip_schema_pattern(value);
                }
            }
        }
        _ => (),
    }
}

fn property_escape(text: &str) -> bool {
    text.as_bytes()
        .windows(3)
        .enumerate()
        .any(|(index, bytes)| {
            matches!(bytes, [b'\\', b'p' | b'P', b'{'])
                && text.as_bytes()[..index]
                    .iter()
                    .rev()
                    .take_while(|&&byte| byte == b'\\')
                    .count()
                    % 2
                    == 0
        })
}

pub enum Probe {
    Response(reqwest::Response),
    Transient {
        message: String,
        account_capacity: bool,
    },
}

pub async fn probe(mut response: reqwest::Response) -> Probe {
    if !response.status().is_success()
        || !response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"))
    {
        return Probe::Response(response);
    }
    let mut data = Vec::new();
    let mut first: Vec<Result<Bytes, reqwest::Error>> = Vec::new();
    while data.len() < 256 * 1024 {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                first.push(Err(error));
                break;
            }
        };
        data.extend_from_slice(&chunk);
        first.push(Ok(chunk));
        let text = String::from_utf8_lossy(&data);
        let lower = text.to_ascii_lowercase();
        let pattern = CAPACITY
            .iter()
            .find(|part| lower.contains(**part))
            .map(|part| (*part, true))
            .or_else(|| {
                OVERLOADED
                    .iter()
                    .find(|part| lower.contains(**part))
                    .map(|part| (*part, false))
            });
        if let Some((pattern, account_capacity)) = pattern {
            let message = text
                .split('\n')
                .filter_map(|line| {
                    let data = line.strip_prefix("data:")?.trim_matches(js_space);
                    let value = serde_json::from_str::<Value>(data).ok()?;
                    nested_message(&value, 0).map(str::to_owned)
                })
                .next()
                .unwrap_or_else(|| pattern.to_owned());
            return Probe::Transient {
                message,
                account_capacity,
            };
        }
        if [
            "event: response.output_text.delta",
            "event: response.function_call_arguments.delta",
            "\"type\":\"response.output_text.delta\"",
            "\"type\":\"response.function_call_arguments.delta\"",
        ]
        .iter()
        .any(|part| lower.contains(part))
        {
            break;
        }
    }
    let status = response.status();
    let headers = response.headers().clone();
    let version = response.version();
    let body = reqwest::Body::wrap_stream(stream::iter(first).chain(response.bytes_stream()));
    let mut replay = axum::http::Response::new(body);
    *replay.status_mut() = status;
    *replay.headers_mut() = headers;
    *replay.version_mut() = version;
    Probe::Response(reqwest::Response::from(replay))
}

fn nested_message(value: &Value, depth: usize) -> Option<&str> {
    if depth > 6 {
        return None;
    }
    for entry in [
        value.get("message"),
        value["error"].get("message"),
        value["response"]["error"].get("message"),
    ] {
        if let Some(text) = entry
            .and_then(Value::as_str)
            .filter(|text| !text.trim_matches(js_space).is_empty())
        {
            return Some(text);
        }
    }
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .find_map(|item| nested_message(item, depth + 1));
    }
    value
        .as_object()?
        .values()
        .find_map(|item| nested_message(item, depth + 1))
}

pub fn sse_failure(message: &str) -> String {
    let payload =
        json!({"error":{"message":message,"type":"server_error","code":"service_unavailable"}});
    format!("[{}]: {payload}", StatusCode::SERVICE_UNAVAILABLE.as_u16())
}

pub fn quota_reset(status: StatusCode, message: &mut String) -> Option<i64> {
    if status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let raw = message.strip_prefix("[429]: ")?;
    let data = serde_json::from_str::<Value>(raw).ok()?;
    let error = &data["error"];
    if error["type"] != "usage_limit_reached" {
        return None;
    }
    let now = chrono::Utc::now().timestamp_millis();
    let reset = error["resets_at"]
        .as_f64()
        .map(|value| (value * 1000.0) as i64)
        .filter(|&value| value > now)
        .or_else(|| {
            error["resets_in_seconds"]
                .as_f64()
                .filter(|value| *value > 0.0)
                .map(|value| now.saturating_add((value * 1000.0) as i64))
        })?;
    if let Some(detail) = error["message"]
        .as_str()
        .filter(|detail| !detail.is_empty())
    {
        *message = format!("[429]: {detail}");
    }
    Some(reset)
}
