use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use axum::Router;
use axum::body::{Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header, request::Parts};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use crate::chat_format::ChatFormat;
use crate::chat_protocol_input;
use crate::dashboard_session;
use crate::model_resolution;
use crate::observed_response::into_http;
use crate::provider_connections::{read_all, truthy};
use crate::storage::Storage;

#[path = "translator_gemini.rs"]
mod gemini;
#[path = "translator_history.rs"]
mod history;
#[path = "translator_send.rs"]
mod send;
#[path = "translator_vertex.rs"]
mod vertex;

const LIMIT: usize = 128 * 1024 * 1024;
const OPENAI_BASE: &str = "https://api.openai.com/v1";
static FORMATS: OnceLock<Value> = OnceLock::new();

#[derive(Clone)]
struct Translator {
    storage: Storage,
    send: send::Clients,
    sessions: crate::codex_executor::Sessions,
    github_routes: crate::github_responses::Routes,
}

pub fn router(storage: Storage) -> Router {
    let state = Translator {
        storage,
        send: send::Clients::new(),
        sessions: crate::codex_executor::Sessions::default(),
        github_routes: crate::github_responses::Routes::default(),
    };
    Router::new()
        .route("/api/translator/load", get(load))
        .route("/api/translator/save", post(save))
        .route("/api/translator/translate", post(translate))
        .route("/api/translator/send", post(send::route))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<Translator>, request: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, request.headers()).await {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        axum::Json(json!({"error":"Unauthorized"})),
    )
        .into_response()
}

fn reply(status: StatusCode, body: Value) -> Response {
    into_http(Ok(json!({
        "status": status.as_u16(),
        "headers": {"content-type":"application/json"},
        "body": body.to_string(),
    })))
}

fn success(result: Value) -> Response {
    reply(StatusCode::OK, json!({"success":true,"result":result}))
}

fn failure(status: StatusCode, error: &str) -> Response {
    reply(status, json!({"success":false,"error":error}))
}

fn bad(error: &str) -> Response {
    failure(StatusCode::BAD_REQUEST, error)
}

fn history_file(storage: &Storage, name: &str) -> Option<PathBuf> {
    match name {
        "1_req_client.json" | "2_req_source.json" | "3_req_openai.json" | "4_req_target.json"
        | "5_res_provider.txt" | "6_res_openai.txt" | "7_res_client.txt" | "7_res_client.json" => {
            Some(storage.root().join("logs").join("translator").join(name))
        }
        _ => None,
    }
}

async fn load(State(state): State<Translator>, request: Request) -> Response {
    let query = request.uri().query().unwrap_or_default();
    let Some(name) = url::form_urlencoded::parse(query.as_bytes())
        .find_map(|(key, value)| (key == "file").then_some(value))
        .filter(|value| !value.is_empty())
    else {
        return bad("File parameter required");
    };
    let Some(path) = history_file(&state.storage, &name) else {
        return bad("Invalid file name");
    };
    let name = name.into_owned();
    match tokio::task::spawn_blocking(move || match fs::read(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => history::read_legacy(&name, LIMIT),
        result => result,
    })
    .await
    {
        Ok(Ok(bytes)) => reply(
            StatusCode::OK,
            json!({
                "success":true,"content":String::from_utf8_lossy(&bytes),
            }),
        ),
        Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => {
            failure(StatusCode::NOT_FOUND, "File not found")
        }
        _ => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read translator history",
        ),
    }
}

fn write_file(path: &Path, content: &str) -> io::Result<()> {
    let dir = path.parent().expect("translator history directory");
    fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(content.as_bytes())?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

struct Input {
    parts: Parts,
    bytes: Bytes,
}

impl Input {
    async fn read(request: Request) -> Option<Self> {
        let (parts, body) = request.into_parts();
        let bytes = to_bytes(body, LIMIT).await.ok()?;
        Some(Self { parts, bytes })
    }

    fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.bytes).ok()
    }
}

async fn save(State(state): State<Translator>, request: Request) -> Response {
    let Some(input) = Input::read(request).await else {
        return failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read translator request",
        );
    };
    let Some(value) = input.json().filter(|value| !value.is_null()) else {
        return bad("Invalid JSON body");
    };
    if !truthy(&value["file"]) || value.get("content").is_none() {
        return bad("File and content required");
    }
    let Some(path) = value["file"]
        .as_str()
        .and_then(|name| history_file(&state.storage, name))
    else {
        return bad("Invalid file name");
    };
    let Some(content) = value["content"].as_str().map(ToOwned::to_owned) else {
        return bad("Content must be a string");
    };
    match tokio::task::spawn_blocking(move || write_file(&path, &content)).await {
        Ok(Ok(())) => reply(StatusCode::OK, json!({"success":true})),
        _ => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save translator history",
        ),
    }
}

async fn translate(State(state): State<Translator>, request: Request) -> Response {
    let Some(input) = Input::read(request).await else {
        return failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read translator request",
        );
    };
    let Some(value) = input.json().filter(|value| !value.is_null()) else {
        return bad("Invalid JSON body");
    };
    let body = &value["body"];
    let step = &value["step"];
    if !truthy(step) || !truthy(body) {
        return bad("Step and body required");
    }
    match step.as_f64() {
        Some(1.0 | 2.0) => {
            let request = enveloped(body);
            let (provider, model) =
                if let Some(model) = request["model"].as_str().filter(|name| !name.is_empty()) {
                    let model = model.to_owned();
                    match state
                        .storage
                        .run(false, move |db| {
                            match model_resolution::get_model_info(db, &model)? {
                                Some((provider, model)) => Ok((Some(provider), Some(model))),
                                None => Ok((None, Some(model))),
                            }
                        })
                        .await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            return failure(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "Failed to resolve model",
                            );
                        }
                    }
                } else {
                    if truthy(&request["model"]) {
                        return bad("Invalid model format");
                    }
                    (None, None)
                };
            let provider = provider.as_deref();
            let mut source = source_format(request);
            if source == "openai"
                && provider == Some("chatgpt-web")
                && request["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| {
                        let content = &message["content"];
                        content["type"] == "document"
                            || content.as_array().is_some_and(|parts| {
                                parts.iter().any(|part| part["type"] == "document")
                            })
                    })
                })
            {
                source = "claude";
            }
            if step.as_f64() == Some(1.0) {
                return success(json!({
                    "provider":provider,"model":model,"sourceFormat":source,
                    "targetFormat":target_format(provider.unwrap_or_default()),
                }));
            }
            match intermediate(
                request,
                source,
                model.as_deref().unwrap_or_default(),
                provider.unwrap_or_default(),
            ) {
                Some(body) => success(json!({"body":body})),
                None => bad("Invalid source request body"),
            }
        }
        Some(3.0) => {
            let (Some(provider), Some(model)) = (
                body["provider"].as_str().filter(|value| !value.is_empty()),
                body["model"].as_str().filter(|value| !value.is_empty()),
            ) else {
                return bad("provider and model required");
            };
            if matches!(provider, "mimo-free" | "mmf") {
                return failure(StatusCode::GONE, crate::chat::MIMO_FREE_RETIRED);
            }
            if crate::opencode_chat::requires_official_client(provider, model) {
                return failure(
                    StatusCode::FORBIDDEN,
                    crate::opencode_chat::FREE_CLIENT_REQUIRED,
                );
            }
            let key = provider.to_owned();
            let selected = state
                .storage
                .run(false, move |db| {
                    Ok(read_all(db, Some(&key))?
                        .into_iter()
                        .find(|connection| connection["isActive"] != false))
                })
                .await;
            match selected {
                Ok(Some(account)) => {
                    let request_body = enveloped(body);
                    if provider == "commandcode" {
                        return match send::commandcode_preview(
                            &state.send,
                            request_body,
                            model,
                            &account,
                            &input.parts.headers,
                        ) {
                            Some(result) => success(result),
                            None => {
                                bad("Invalid CommandCode preview request or missing credentials")
                            }
                        };
                    }
                    if send::supports_chat_preview(provider) {
                        return match send::chat_preview(
                            &state,
                            provider,
                            model,
                            request_body,
                            &account,
                        ) {
                            Some(result) => success(result),
                            None => bad(&format!(
                                "Invalid {provider} preview request or missing credentials"
                            )),
                        };
                    }
                    if provider == "codex" {
                        return match send::codex_preview(
                            &state,
                            request_body,
                            model,
                            &account,
                            &input.parts.headers,
                        ) {
                            Some(result) => success(result),
                            None => bad("Invalid Codex preview request or missing credentials"),
                        };
                    }
                    if provider == "antigravity" {
                        return match crate::antigravity_transport::preview(
                            request_body,
                            model,
                            &account,
                            &input.parts.headers,
                            &state.sessions,
                        ) {
                            Ok(result) => success(result),
                            Err(error) => failure(error.status, &error.message),
                        };
                    }
                    if provider == "gemini-cli" {
                        return match send::gemini_cli_preview(
                            &state,
                            request_body,
                            model,
                            &account,
                            &input.parts.headers,
                        ) {
                            Ok(result) => success(result),
                            Err(message) => bad(message),
                        };
                    }
                    if matches!(provider, "vertex" | "vertex-partner") {
                        return vertex::preview(request_body, provider, model, &account);
                    }
                    if provider == "kiro" {
                        return match send::kiro_preview(
                            &state,
                            request_body,
                            model,
                            &account,
                            &input.parts.headers,
                        )
                        .await
                        {
                            Ok(result) => success(result),
                            Err(message) => bad(message),
                        };
                    }
                    if provider == "xiaomi-mimo" {
                        return match send::mimo_preview(&state.send, request_body, model, &account)
                        {
                            Some(result) => success(result),
                            None => bad("Invalid MiMo preview request or missing credentials"),
                        };
                    }
                    if provider == "grok-cli" {
                        return match send::grok_cli_preview(
                            &state.send,
                            request_body,
                            model,
                            &account,
                        ) {
                            Some(result) => success(result),
                            None => bad("Invalid Grok CLI preview request or missing credentials"),
                        };
                    }
                    if provider == "github" {
                        return match send::github_preview(
                            &state.send,
                            request_body,
                            model,
                            &account,
                        ) {
                            Some(result) => success(result),
                            None => {
                                bad("Invalid GitHub Copilot preview request or missing credentials")
                            }
                        };
                    }
                    let result = if provider == "devin-cli" {
                        Some(json!({"url":"devin://acp/stdio","headers":{},"body":null}))
                    } else if provider == "gemini" {
                        gemini::preview(request_body, model, &account)
                    } else if provider == "anthropic"
                        || provider.starts_with("anthropic-compatible-")
                    {
                        send::anthropic_preview(request_body, provider, model, &account)
                    } else if provider == "muse" {
                        send::muse_preview(request_body, model, &account)
                    } else if provider == "kimi" {
                        send::kimi_preview(&state.send, request_body, model, &account)
                    } else if provider.starts_with("openai-compatible-") {
                        send::compatible_preview(
                            &state.send,
                            request_body,
                            provider,
                            model,
                            &account,
                        )
                    } else {
                        send::static_preview(&state.send, request_body, provider, model, &account)
                    };
                    match result {
                        Some(result) => success(result),
                        None => bad(&format!(
                            "Invalid {provider} preview request or missing credentials"
                        )),
                    }
                }
                Ok(None) => bad(&format!("No active connection for provider: {provider}")),
                Err(_) => failure(
                    StatusCode::BAD_GATEWAY,
                    &format!("Could not load the saved {provider} account"),
                ),
            }
        }
        _ => bad("Invalid step (1-3)"),
    }
}

fn enveloped(body: &Value) -> &Value {
    body.get("body")
        .filter(|value| truthy(value))
        .unwrap_or(body)
}

fn target_format(provider: &str) -> &str {
    if provider.starts_with("openai-compatible-") && provider.contains("responses") {
        "openai-responses"
    } else if provider.starts_with("anthropic-compatible-") {
        "claude"
    } else {
        FORMATS.get_or_init(|| {
            serde_json::from_str(include_str!("translator-formats.json"))
                .expect("embedded translator formats")
        })[provider]
            .as_str()
            .unwrap_or("openai")
    }
}

fn source_format(body: &Value) -> &'static str {
    if truthy(&body["input"])
        && (body["input"].is_array() || body["input"].is_string())
        && !truthy(&body["messages"])
    {
        return "openai-responses";
    }
    if truthy(&body["request"]["contents"]) && body["userAgent"] == "antigravity" {
        return "antigravity";
    }
    if body["contents"].is_array() {
        return "gemini";
    }
    if ["stream_options", "response_format", "logit_bias", "user"]
        .iter()
        .any(|key| truthy(&body[key]))
        || [
            "logprobs",
            "top_logprobs",
            "n",
            "presence_penalty",
            "frequency_penalty",
        ]
        .iter()
        .any(|key| body.get(*key).is_some())
    {
        return "openai";
    }
    let Some(messages) = body["messages"].as_array() else {
        return "openai";
    };
    if let Some(blocks) = messages
        .first()
        .and_then(|message| message["content"].as_array())
        && blocks.first().is_some_and(|entry| entry["type"] == "text")
        && !body["model"]
            .as_str()
            .is_some_and(|model| model.contains('/'))
    {
        if truthy(&body["system"])
            || truthy(&body["anthropic_version"])
            || blocks
                .iter()
                .any(|entry| entry["type"] == "image" && entry["source"]["type"] == "base64")
        {
            return "claude";
        }
        if blocks
            .iter()
            .any(|entry| entry["type"] == "image_url" && truthy(&entry["image_url"]["url"]))
        {
            return "openai";
        }
        if blocks
            .iter()
            .any(|entry| matches!(entry["type"].as_str(), Some("tool_use" | "tool_result")))
        {
            return "claude";
        }
    }
    if body.get("system").is_some() || truthy(&body["anthropic_version"]) {
        "claude"
    } else {
        "openai"
    }
}

fn clean_openai(mut body: Value, provider: &str) -> Option<Value> {
    let preserve_cache = crate::static_chat::default_endpoint(provider)
        .is_some_and(|endpoint| endpoint.preserve_cache_control());
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        let mut normalized = Vec::with_capacity(messages.len());
        for message in std::mem::take(messages) {
            if let Some(message) = crate::chat_input::filter_message(message, preserve_cache, true)?
            {
                normalized.push(message);
            }
        }
        *messages = normalized;
    }
    body.as_object_mut()?.shift_remove("_toolNameMap");
    crate::chat_input::normalize_tools(&mut body)?;
    Some(body)
}

fn intermediate(body: &Value, source: &str, model: &str, provider: &str) -> Option<Value> {
    if source == "gemini" {
        return gemini::to_openai(body, model, provider);
    }
    if source == "antigravity" {
        let mut request = crate::mitm_antigravity::request(body, model)?;
        request["stream"] = json!(body["stream"] != false);
        return Some(request);
    }
    if !body.is_object() {
        return None;
    }
    let mut input = body.clone();
    let thinking = if chat_protocol_input::unsupported_thinking(body)
        || crate::thinking_intent::model_suffix(model).1.is_some()
    {
        crate::openai_thinking::capture(
            &mut input,
            provider,
            model,
            crate::static_chat::openai_thinking_format(provider),
            false,
        )
        .1
    } else {
        None
    };
    if source == "openai-responses" {
        input.as_object_mut()?.shift_remove("reasoning");
    }
    if chat_protocol_input::unsupported_thinking(&input) {
        return None;
    }
    let inline_files = provider == "chatgpt-web";
    let mut result = match source {
        "openai" => {
            if let Some(messages) = input.get_mut("messages").and_then(Value::as_array_mut) {
                crate::chat_input::preprocess_tool_history(messages)?;
            }
            input
        }
        "claude" => {
            let mut result = if inline_files {
                chat_protocol_input::chatgpt_web(ChatFormat::Messages, input, "", true)
            } else {
                chat_protocol_input::translate(ChatFormat::Messages, input, "")
            }?;
            result["model"] = json!(model);
            result
        }
        "openai-responses" if inline_files => {
            chat_protocol_input::chatgpt_web(ChatFormat::Responses, input, "", true)?
        }
        "openai-responses" => chat_protocol_input::responses_preview(input)?,
        _ => return None,
    };
    if source != "openai" && result.get("messages").is_none() {
        return None;
    }
    if let Some(thinking) = thinking {
        thinking.write(&mut result);
    }
    clean_openai(result, provider)
}

fn redact_url(raw: &str) -> String {
    let Ok(mut address) = url::Url::parse(raw) else {
        return raw.to_owned();
    };
    let mut changed = false;
    if !address.username().is_empty() || address.password().is_some() {
        let _ = address.set_username("");
        let _ = address.set_password(None);
        changed = true;
    }
    let query: Vec<_> = address
        .query_pairs()
        .map(|(key, value)| {
            let value = if matches!(
                key.to_ascii_lowercase().as_str(),
                "key" | "api_key" | "apikey" | "token" | "access_token" | "secret"
            ) {
                changed = true;
                "[redacted]".into()
            } else {
                value
            };
            (key.into_owned(), value.into_owned())
        })
        .collect();
    if changed {
        if !query.is_empty() {
            address.query_pairs_mut().clear().extend_pairs(query);
        }
        address.to_string()
    } else {
        raw.to_owned()
    }
}
