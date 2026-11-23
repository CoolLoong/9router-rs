use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use futures_util::{StreamExt, stream};
use rusqlite::Connection;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::capacity_adapter::Routing;
use crate::chat_input::{js_space, truthy};
use crate::model_resolution;

const PEEK_LIMIT: usize = 4 * 1024 * 1024;
const JSON_LIMIT: usize = 128 * 1024 * 1024;

static ROTATION: OnceLock<Mutex<HashMap<String, (usize, usize)>>> = OnceLock::new();

#[derive(Clone)]
pub enum Route {
    Single(String, String),
    Unavailable {
        provider: Option<String>,
        status: StatusCode,
        message: Cow<'static, str>,
    },
    Combo(Combo<Route>),
    Fusion(crate::chat::Fusion<crate::combo_chat::Route>),
    Smart(crate::chat::Smart<crate::combo_chat::Route>),
    Adapter(Box<Route>, f64),
}

#[derive(Clone)]
pub(super) struct StopFallback;

#[derive(Clone)]
pub struct Combo<T> {
    pub name: String,
    pub members: Vec<T>,
    pub sticky: Option<usize>,
}

pub(crate) fn strategy<'a>(requested: &str, settings: &'a Value) -> &'a str {
    settings["comboStrategies"][requested]["fallbackStrategy"]
        .as_str()
        .filter(|strategy| !strategy.is_empty())
        .or_else(|| {
            settings["comboStrategy"]
                .as_str()
                .filter(|strategy| !strategy.is_empty())
        })
        .unwrap_or("fallback")
}

pub(crate) fn sticky_for(strategy: &str, settings: &Value) -> Option<usize> {
    (strategy == "round-robin").then(|| {
        let limit = &settings["comboStickyRoundRobinLimit"];
        let parsed = limit.as_u64().or_else(|| {
            crate::chat_output::js_text(limit)
                .trim_start_matches(js_space)
                .split(|ch: char| !ch.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        });
        parsed
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(1)
            .max(1)
    })
}

pub fn resolve(
    db: &Connection,
    requested: &str,
    settings: &Value,
    adapters: &Routing<'_>,
) -> rusqlite::Result<Option<Route>> {
    let mut parents = HashSet::new();
    let Some(original) = resolve_inner(db, requested, settings, adapters, &mut parents)? else {
        return Ok(None);
    };
    if model_resolution::get_combo_models(db, requested)?.is_some() {
        return Ok(Some(original));
    }
    let additional = adapters.candidates(db, settings, &[requested])?;
    if additional.is_empty() {
        return Ok(Some(original));
    }
    let Some(sticky) = adapters.solo_limit(settings) else {
        return Ok(None);
    };
    let mut members = Vec::with_capacity(additional.len() + 1);
    parents.insert(requested.to_owned());
    for (name, window) in additional {
        let Some(route) = resolve_inner(db, name, settings, adapters, &mut parents)? else {
            return Ok(None);
        };
        members.push(Route::Adapter(Box::new(route), window));
    }
    members.push(original);
    Ok(Some(Route::Combo(Combo {
        name: requested.to_owned(),
        members,
        sticky,
    })))
}

fn resolve_inner(
    db: &Connection,
    requested: &str,
    settings: &Value,
    adapters: &Routing<'_>,
    parents: &mut HashSet<String>,
) -> rusqlite::Result<Option<Route>> {
    if parents.len() == 20 || parents.contains(requested) {
        return Ok(Some(Route::Unavailable {
            provider: None,
            status: StatusCode::BAD_REQUEST,
            message: "Invalid nested combo configuration".into(),
        }));
    }
    if let Some(models) = model_resolution::get_combo_models(db, requested)? {
        let Some(models) = models.as_array() else {
            return Ok(None);
        };
        let strategy = strategy(requested, settings);
        let sticky = sticky_for(strategy, settings);
        parents.insert(requested.to_owned());
        let mut members = Vec::new();
        let mut names = Vec::new();
        let mut first = None;
        if strategy != "fusion" {
            let original: Vec<_> = models.iter().filter_map(Value::as_str).collect();
            for (name, window) in adapters.candidates(db, settings, &original)? {
                let Some(route) = resolve_inner(db, name, settings, adapters, parents)? else {
                    return Ok(None);
                };
                members.push(Route::Adapter(Box::new(route), window));
                names.push(name.to_owned());
            }
        }
        for model in models {
            if strategy == "fusion" && !truthy(model) {
                continue;
            }
            let Some(model) = model.as_str() else {
                return Ok(None);
            };
            first.get_or_insert(model);
            names.push(model.to_owned());
            let Some(route) = resolve_inner(db, model, settings, adapters, parents)? else {
                return Ok(None);
            };
            members.push(route);
        }
        if strategy == "fusion" {
            if members.len() == 1 {
                parents.remove(requested);
                return Ok(members.pop());
            }
            let config = &settings["comboStrategies"][requested];
            let judge = config["judgeModel"]
                .as_str()
                .map(|model| model.trim_matches(js_space))
                .filter(|model| !model.is_empty())
                .or(first);
            let judge = if let Some(model) = judge {
                let Some(judge) = resolve_inner(db, model, settings, adapters, parents)? else {
                    return Ok(None);
                };
                Some(Box::new(judge))
            } else {
                None
            };
            parents.remove(requested);
            let tuning = crate::fusion::Tuning::new(members.len(), &config["fusionTuning"]);
            return Ok(Some(Route::Fusion(crate::chat::Fusion {
                members,
                judge,
                tuning,
            })));
        }
        if strategy == "smart" {
            if members.len() == 1 {
                parents.remove(requested);
                return Ok(members.pop());
            }
            let config = crate::smart_router::Config::new(&settings["comboStrategies"][requested]);
            let router = model_resolution::get_model_info(db, &config.router)?;
            parents.remove(requested);
            return Ok(Some(Route::Smart(crate::chat::Smart {
                name: requested.to_owned(),
                members: names.into_iter().zip(members).collect(),
                router,
                config,
            })));
        }
        parents.remove(requested);
        return Ok(Some(Route::Combo(Combo {
            name: requested.to_owned(),
            members,
            sticky,
        })));
    }
    let Some((provider, model)) = model_resolution::get_model_info(db, requested)?
        .filter(|(provider, _)| !provider.is_empty())
    else {
        return Ok(Some(Route::Unavailable {
            provider: None,
            status: StatusCode::BAD_REQUEST,
            message: "Invalid model format".into(),
        }));
    };
    if crate::chat::native_provider(&provider) {
        return Ok(Some(Route::Single(provider, model)));
    }
    let without_account = crate::provider_index::find(&provider)
        .is_some_and(|entry| entry["noAuth"] == true || entry["noAuthFree"] == true);
    if without_account || model_resolution::has_active_connection(db, &provider)? {
        return Ok(Some(Route::Unavailable {
            status: StatusCode::NOT_ACCEPTABLE,
            message: format!("Provider '{provider}' does not support this chat request").into(),
            provider: Some(provider),
        }));
    }
    Ok(Some(Route::Unavailable {
        status: StatusCode::NOT_FOUND,
        message: format!("No active credentials for provider: {provider}").into(),
        provider: Some(provider),
    }))
}

pub fn reset(name: Option<&str>) {
    crate::smart_router::clear(name, None);
    if let Some(state) = ROTATION.get()
        && let Ok(mut state) = state.lock()
    {
        if let Some(name) = name {
            state.remove(name);
        } else {
            state.clear();
        }
    }
}

impl<T> Combo<T> {
    fn rotate(&mut self) {
        let Some(limit) = self.sticky.filter(|_| self.members.len() > 1) else {
            return;
        };
        let Ok(mut rotation) = ROTATION.get_or_init(Mutex::default).lock() else {
            return;
        };
        let state = rotation.entry(self.name.clone()).or_default();
        let index = state.0 % self.members.len();
        self.members.rotate_left(index);
        if state.1 >= limit - 1 {
            *state = ((index + 1) % self.members.len(), 0);
        } else {
            *state = (index, state.1 + 1);
        }
    }

    pub async fn execute<F, Fut>(
        mut self,
        capabilities: &HashSet<&str>,
        prepare: impl Fn(&T) -> (Option<String>, bool, Option<String>),
        mut run: F,
        on_failure: impl Fn(&str),
    ) -> Response
    where
        F: FnMut(T) -> Fut,
        Fut: Future<Output = Response>,
    {
        self.rotate();
        if !capabilities.is_empty() {
            self.members.sort_by_key(|member| !prepare(member).1);
        }
        let mut first_status = None;
        let mut first_request_error = None;
        let mut last_error = None;
        let mut retry_after = None;
        let len = self.members.len();
        for (index, member) in self.members.into_iter().enumerate() {
            let (provider, _, label) = prepare(&member);
            let mut response = run(member).await;
            if response.extensions().get::<StopFallback>().is_some() {
                return response;
            }
            let status = response.status();
            let last = index + 1 == len;
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|header| header.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            if status.is_success() {
                if last {
                    return response;
                }
                if content_type.contains("text/event-stream") {
                    let (restored, empty) = peek_sse(response).await;
                    if let Some(reason) = empty {
                        if let Some(label) = &label {
                            on_failure(label);
                        }
                        last_error = Some(format!("empty generation ({reason})"));
                        first_status.get_or_insert(StatusCode::BAD_GATEWAY);
                        continue;
                    }
                    return restored;
                }
                if content_type.contains("application/json") {
                    let (restored, data) = peek_json(response).await;
                    if data.as_ref().is_some_and(is_zero_content) {
                        if let Some(label) = &label {
                            on_failure(label);
                        }
                        last_error = Some("Zero usable content".to_owned());
                        first_status = Some(StatusCode::OK);
                        continue;
                    }
                    response = restored;
                }
                return response;
            }

            if let Some(label) = &label {
                on_failure(label);
            }
            let (restored, data) = peek_json(response).await;
            if let Some(value) = data.as_ref().and_then(|data| data["retryAfter"].as_str())
                && let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(value)
                && retry_after.as_ref().is_none_or(|old| &parsed < old)
            {
                retry_after = Some(parsed);
            }
            let error = data
                .as_ref()
                .and_then(|data| {
                    [&data["error"]["message"], &data["error"], &data["message"]]
                        .into_iter()
                        .find(|value| truthy(value))
                })
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string())
                })
                .unwrap_or_else(|| status.canonical_reason().unwrap_or("").to_owned());
            let rule = chat_account_rules_rs::port::check_fallback_error(
                json!(status.as_u16()),
                json!(error),
                None,
                provider.map(|provider| json!(provider)),
            )
            .unwrap_or(Value::Null);
            if rule["shouldFallback"] != true && rule["requestScoped"] != true {
                return restored;
            }
            if rule["requestScoped"] == true {
                first_request_error.get_or_insert(restored);
            } else if let Some(millis) = rule["cooldownMs"]
                .as_u64()
                .filter(|millis| (1..=5000).contains(millis))
                && matches!(status.as_u16(), 502..=504)
            {
                tokio::time::sleep(Duration::from_millis(millis)).await;
            }
            first_status.get_or_insert(status);
            last_error = Some(if error.is_empty() {
                status.as_u16().to_string()
            } else {
                error
            });
        }

        if let Some(error) = first_request_error {
            return error;
        }
        let message = last_error.unwrap_or_else(|| "All combo models unavailable".to_owned());
        let status = if message.to_ascii_lowercase().contains("no credentials") {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            first_status.unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
        };
        combo_error(status, &message, retry_after)
    }
}

pub(crate) fn combo_error(
    status: StatusCode,
    message: &str,
    retry: Option<chrono::DateTime<chrono::FixedOffset>>,
) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response.headers_mut().insert(header::VARY, HeaderValue::from_static(
        "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding",
    ));
    let message = if let Some(retry) = retry {
        let millis =
            (retry.timestamp_millis() - chrono::Utc::now().timestamp_millis()).max(0) as u64;
        let seconds = millis.div_ceil(1000);
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        format!(
            "{message} ({})",
            crate::chat_accounts::format_delay(seconds)
        )
    } else {
        message.to_owned()
    };
    *response.body_mut() = Body::from(json!({"error": {"message": message}}).to_string());
    response
}

async fn peek_json(response: Response) -> (Response, Option<Value>) {
    let (parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, JSON_LIMIT).await else {
        return (Response::from_parts(parts, Body::empty()), None);
    };
    let data = serde_json::from_slice(&bytes).ok();
    (Response::from_parts(parts, Body::from(bytes)), data)
}

fn is_zero_content(data: &Value) -> bool {
    if let Some(choices) = data["choices"].as_array() {
        let Some(choice) = choices.first().filter(|value| truthy(value)) else {
            return true;
        };
        let message = if truthy(&choice["message"]) {
            &choice["message"]
        } else {
            &choice["delta"]
        };
        let content = message["content"]
            .as_str()
            .is_some_and(|text| !text.trim_matches(js_space).is_empty())
            || message["content"]
                .as_array()
                .is_some_and(|content| !content.is_empty());
        let tool = message["tool_calls"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
            || truthy(&message["function_call"]);
        let reasoning = message["reasoning_content"]
            .as_str()
            .is_some_and(|text| !text.trim_matches(js_space).is_empty());
        let normal = matches!(
            choice["finish_reason"].as_str(),
            Some("stop" | "length" | "tool_calls" | "function_call")
        );
        if !content && !tool && !reasoning && !normal {
            return true;
        }
    }
    data["content"].as_array().is_some_and(|content| {
        !content.iter().any(|block| {
            block["type"] == "tool_use"
                || block["type"] == "text"
                    && block["text"]
                        .as_str()
                        .is_some_and(|text| !text.trim_matches(js_space).is_empty())
        }) && (!truthy(&data["stop_reason"])
            || matches!(
                data["stop_reason"].as_str(),
                Some("in_progress" | "incomplete")
            ))
    })
}

async fn peek_sse(response: Response) -> (Response, Option<String>) {
    let deadline = std::env::var("EMPTY_PEEK_DEADLINE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .unwrap_or(45_000);
    let deadline = Instant::now() + Duration::from_millis(deadline);
    let (parts, body) = response.into_parts();
    let mut downstream = body.into_data_stream().fuse();
    let mut head = Vec::new();
    let mut pending = Vec::new();
    let mut total = 0;
    let mut finish = None;
    let mut content = false;
    loop {
        match tokio::time::timeout_at(deadline, downstream.next()).await {
            Ok(Some(Ok(bytes))) => {
                total += bytes.len();
                pending.extend_from_slice(&bytes);
                if let Some(end) = pending.iter().rposition(|&byte| byte == b'\n') {
                    for line in pending[..end].split(|&byte| byte == b'\n') {
                        content |= observe_line(line, &mut finish);
                    }
                    pending.drain(..=end);
                }
                head.push(Ok(bytes));
                if content || total >= PEEK_LIMIT {
                    content = true;
                    break;
                }
            }
            Ok(Some(Err(error))) => {
                head.push(Err(error));
                break;
            }
            Ok(None) => {
                content |= observe_line(&pending, &mut finish);
                break;
            }
            Err(_) => {
                content = true;
                break;
            }
        }
    }
    let empty = (!content && finish.as_deref() != Some("content_filter"))
        .then(|| finish.unwrap_or_else(|| "no-content".into()));
    (
        Response::from_parts(
            parts,
            Body::from_stream(stream::iter(head).chain(downstream)),
        ),
        empty,
    )
}

fn observe_line(line: &[u8], finish: &mut Option<String>) -> bool {
    let Some(data) = std::str::from_utf8(line).ok().and_then(|text| {
        text.trim_start_matches(js_space)
            .strip_prefix("data:")
            .map(|data| data.trim_matches(js_space))
    }) else {
        return false;
    };
    let Ok(event) = serde_json::from_str::<Value>(data) else {
        return false;
    };
    let non_empty = |value: &Value| value.as_str().is_some_and(|text| !text.is_empty());
    let mut content = false;
    for choice in event["choices"].as_array().into_iter().flatten() {
        let delta = if truthy(&choice["delta"]) {
            &choice["delta"]
        } else {
            &choice["message"]
        };
        content |= non_empty(&delta["content"])
            || delta["content"]
                .as_array()
                .is_some_and(|parts| !parts.is_empty())
            || delta["tool_calls"]
                .as_array()
                .is_some_and(|parts| !parts.is_empty())
            || non_empty(&delta["output_text"]);
        if let Some(reason) = choice["finish_reason"]
            .as_str()
            .filter(|reason| !reason.is_empty())
        {
            *finish = Some(reason.to_ascii_lowercase());
        }
    }
    match event["type"].as_str() {
        Some("content_block_delta") => {
            content |= match event["delta"]["type"].as_str() {
                Some("text_delta") => non_empty(&event["delta"]["text"]),
                Some("input_json_delta") => non_empty(&event["delta"]["partial_json"]),
                _ => false,
            };
        }
        Some("content_block_start") => {
            content |= event["content_block"]["type"] == "tool_use"
                || event["content_block"]["type"] == "text"
                    && non_empty(&event["content_block"]["text"]);
        }
        Some("message_delta") => {
            if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                *finish = Some(
                    match reason {
                        "refusal" => "content_filter",
                        "max_tokens" => "length",
                        "tool_use" => "tool_calls",
                        _ => "stop",
                    }
                    .into(),
                );
            }
        }
        Some(
            "response.output_text.delta"
            | "response.function_call_arguments.delta"
            | "response.custom_tool_call_input.delta",
        ) => content |= non_empty(&event["delta"]),
        Some("response.output_text.done") => content |= non_empty(&event["text"]),
        Some("response.output_item.added" | "response.output_item.done") => {
            content |= matches!(
                event["item"]["type"].as_str(),
                Some("function_call" | "custom_tool_call")
            );
        }
        Some("response.completed") => *finish = Some("stop".into()),
        Some("response.incomplete") => {
            let reason = event["response"]["incomplete_details"]["reason"].as_str();
            *finish = Some(
                match reason {
                    Some("content_filter") => "content_filter",
                    Some("max_output_tokens") => "length",
                    _ => "incomplete",
                }
                .into(),
            );
        }
        Some("response.failed") => *finish = Some("failed".into()),
        _ => (),
    }
    content
}

pub fn capabilities(body: &Value) -> HashSet<&'static str> {
    let mut found = HashSet::new();
    for (items, field) in [
        (&body["messages"], "content"),
        (&body["input"], "content"),
        (&body["contents"], "parts"),
        (&body["request"]["contents"], "parts"),
    ] {
        let Some(items) = items.as_array() else {
            continue;
        };
        let trailing = items
            .iter()
            .rev()
            .take_while(|item| !matches!(item["role"].as_str(), Some("model" | "assistant")));
        for item in trailing {
            if truthy(&item["image_url"])
                || truthy(&item["image"])
                || item["images"]
                    .as_array()
                    .is_some_and(|images| !images.is_empty())
            {
                found.insert("vision");
            }
            if truthy(&item["audio_url"]) || truthy(&item["audio"]) {
                found.insert("audioInput");
            }
            if let Some(text) = item[field].as_str() {
                for (expected, feature) in [
                    ("data:image/", "vision"),
                    ("data:audio/", "audioInput"),
                    ("data:application/pdf", "pdf"),
                ] {
                    if text.contains(expected) {
                        found.insert(feature);
                        break;
                    }
                }
            }
            for block in item[field].as_array().into_iter().flatten() {
                if let Some(capability) = block_capability(block) {
                    found.insert(capability);
                }
            }
            let attachments = if truthy(&item["experimental_attachments"]) {
                &item["experimental_attachments"]
            } else {
                &item["attachments"]
            };
            for attachment in attachments.as_array().into_iter().flatten() {
                let mime = attachment["contentType"]
                    .as_str()
                    .or_else(|| attachment["mediaType"].as_str())
                    .or_else(|| data_mime(&attachment["url"]));
                if let Some(mime) = mime {
                    found.extend(mime_capability(mime));
                } else if truthy(&attachment["url"]) || truthy(&attachment["data"]) {
                    found.insert("vision");
                }
            }
        }
    }
    found
}

fn data_mime(value: &Value) -> Option<&str> {
    value
        .as_str()?
        .strip_prefix("data:")?
        .split([';', ','])
        .next()
        .filter(|mime| !mime.is_empty())
}

fn block_capability(block: &Value) -> Option<&'static str> {
    match block["type"].as_str() {
        Some("image_url" | "image" | "input_image") => Some("vision"),
        Some("input_audio" | "audio_url" | "audio") => Some("audioInput"),
        Some("input_video" | "video_url" | "video") => Some("videoInput"),
        Some("file" | "document" | "input_file") => {
            if truthy(&block["input_audio"]["format"]) {
                return Some("audioInput");
            }
            data_mime(&block["file"]["file_data"])
                .or_else(|| block["source"]["media_type"].as_str())
                .or_else(|| data_mime(&block["source"]["data"]))
                .map_or(Some("pdf"), mime_capability)
        }
        _ => block["inlineData"]["mimeType"]
            .as_str()
            .or_else(|| block["fileData"]["mimeType"].as_str())
            .and_then(mime_capability),
    }
}

fn mime_capability(mime: &str) -> Option<&'static str> {
    if mime.starts_with("image/") {
        Some("vision")
    } else if mime == "application/pdf" {
        Some("pdf")
    } else if mime.starts_with("audio/") {
        Some("audioInput")
    } else if mime.starts_with("video/") {
        Some("videoInput")
    } else {
        None
    }
}
