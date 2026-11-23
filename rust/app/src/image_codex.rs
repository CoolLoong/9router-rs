use std::sync::Arc;
use std::time::Duration;

use axum::http::{StatusCode, header};
use axum::response::Response;
use reqwest::Url;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{ImageError, ImageOptions, ImageState, QueuedImageError, image_response};
use crate::chat_accounts;
use crate::chat_output::js_text;
use crate::provider_connections::{by_id, truthy};
use crate::provider_http::Delivery;
use crate::provider_oauth::{
    RefreshMode, refresh_account, refresh_due_for_provider, test_token_url, token_claims,
};
use crate::storage::Storage;

#[path = "image_codex_sse.rs"]
mod sse;

const GENERATE: &str = "https://chatgpt.com/backend-api/codex/images/generations";
const EDIT: &str = "https://chatgpt.com/backend-api/codex/images/edits";
const RESPONSES: &str = "https://chatgpt.com/backend-api/codex/responses";
const FIXTURE: &str = "http://127.0.0.1:52989/codex-image";
const MAIN_MODEL: &str = "gpt-6-sol";
const FALLBACK_MODEL: &str = "gpt-5.6-terra";
const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const IMAGE_TIMEOUT: Duration = Duration::from_secs(600);
const IMAGE_LIMIT: usize = 128 * 1024 * 1024;

pub(super) enum ResultImage {
    Free,
    Ready(Response),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Generate,
    Edit,
    Responses,
}

impl Mode {
    fn endpoint(self) -> &'static str {
        match self {
            Self::Generate => GENERATE,
            Self::Edit => EDIT,
            Self::Responses => RESPONSES,
        }
    }
}

pub(super) async fn generate(
    state: &ImageState,
    settings: &Value,
    model: &str,
    observed: &Value,
    input: &Value,
    options: ImageOptions<'_>,
) -> Result<ResultImage, QueuedImageError> {
    let (mode, mut request) = plan(model, input)?;
    let endpoint = Url::parse(mode.endpoint())
        .map_err(|_| before(StatusCode::BAD_GATEWAY, "Invalid Codex image endpoint"))?;
    let delivery = Delivery::select(&state.storage, settings, observed, endpoint.as_str())
        .await
        .map_err(|_| route_error())?;
    let mut account = current(&state.storage, observed, &endpoint, &delivery).await?;
    let mut secrets = secrets_of(&account);
    let mut renewed = false;
    if renewable(&account) && refresh_due_for_provider("codex", &account) {
        let next = renew(state, &account, &endpoint, &delivery).await?;
        if let Some(next) = next {
            secrets.extend(secrets_of(&next));
            renewed = next["accessToken"] != account["accessToken"];
            account = next;
        }
    }
    if plan_type(&account).as_deref() == Some("free") {
        return Ok(ResultImage::Free);
    }
    let effects = Effects {
        storage: state.storage.clone(),
        connection: account["id"].as_str().ok_or_else(route_error)?.to_owned(),
        model: model.to_owned(),
        api_key: options.client_key.map(str::to_owned),
        endpoint: options.endpoint.to_owned(),
    };
    let mut response = send(state, &endpoint, &delivery, &account, mode, &request).await?;
    if !renewed
        && renewable(&account)
        && matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        )
        && let Some(next) = renew(state, &account, &endpoint, &delivery).await?
        && next["accessToken"] != account["accessToken"]
    {
        if plan_type(&next).as_deref() == Some("free") {
            return Ok(ResultImage::Free);
        }
        secrets.extend(secrets_of(&next));
        drop(response);
        if mode == Mode::Responses {
            request["prompt_cache_key"] = json!(Uuid::new_v4());
        }
        response = send(state, &endpoint, &delivery, &next, mode, &request).await?;
        account = next;
    }
    secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    let secrets: Arc<[String]> = secrets.into();
    if !response.status().is_success() {
        let failure = rejected(response, &secrets).await;
        if mode == Mode::Responses
            && failure.status == StatusCode::BAD_REQUEST
            && request["model"] == MAIN_MODEL
            && truthy(&request["tools"][0]["model"])
            && failure
                .message
                .to_ascii_lowercase()
                .contains("model is not supported when using codex")
        {
            request["model"] = json!(FALLBACK_MODEL);
            response = send(state, &endpoint, &delivery, &account, mode, &request).await?;
            if !response.status().is_success() {
                return Err(rejection(rejected(response, &secrets).await, &delivery));
            }
        } else {
            return Err(rejection(failure, &delivery));
        }
    }

    let result = if mode == Mode::Responses {
        let stream = sse::Images::new(response, secrets);
        if options.stream {
            return Ok(ResultImage::Ready(stream.reply(effects)));
        }
        let result = stream
            .collect()
            .await
            .map_err(|error| accepted(error.message))?;
        effects.success(result.usage).await;
        image_response(result.image, input, options.binary).await
    } else {
        let value: Value = read_json(response, IMAGE_LIMIT)
            .await
            .ok_or_else(|| accepted("Codex Images API returned invalid JSON."))?;
        let data = value["data"].as_array().filter(|data| {
            data.iter()
                .any(|item| truthy(&item["b64_json"]) || truthy(&item["url"]))
        });
        let Some(data) = data else {
            let reason =
                text(&value["error"]["message"]).unwrap_or("Codex Images API returned no image.");
            return Err(accepted(redact(reason, &secrets)));
        };
        let created = value
            .get("created")
            .filter(|value| truthy(value))
            .cloned()
            .unwrap_or_else(|| json!(chrono::Utc::now().timestamp()));
        let output = json!({"created":created,"data":data});
        effects.success(normalize_usage(&value["usage"])).await;
        if options.stream {
            sse::native(output)
        } else {
            image_response(output, input, options.binary).await
        }
    };
    Ok(ResultImage::Ready(result))
}

fn plan(model: &str, input: &Value) -> Result<(Mode, Value), QueuedImageError> {
    let hosted = matches!(
        model,
        "gpt-image-1.5"
            | "gpt-image-2"
            | "gpt-image-2.5"
            | "gpt-image-2.5-flare"
            | "gpt-image-2.5-sunburst"
            | "gpt-image-2.6-flare"
            | "gpt-image-2.6-sunburst"
    );
    let edit = truthy(&input["image"])
        || input["images"]
            .as_array()
            .is_some_and(|images| !images.is_empty());
    let format = if truthy(&input["output_format"]) {
        input["output_format"]
            .as_str()
            .ok_or_else(|| before(StatusCode::BAD_REQUEST, "Invalid Codex image output format"))?
            .to_lowercase()
    } else {
        "png".into()
    };
    let native = hosted
        && (input["n"].is_null() || input["n"].as_f64() == Some(1.0))
        && (!truthy(&input["size"]) || input["size"] == "auto")
        && format == "png"
        && (!truthy(&input["background"])
            || matches!(input["background"].as_str(), Some("auto" | "opaque")))
        && (!truthy(&input["quality"])
            || matches!(
                input["quality"].as_str(),
                Some("auto" | "low" | "medium" | "high")
            ))
        && input["output_compression"].is_null();
    let refs = input["images"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(std::iter::once(&input["image"]))
        .filter_map(text)
        .map(|entry| {
            if ["data:image/", "http://", "https://"]
                .into_iter()
                .any(|prefix| {
                    entry
                        .get(..prefix.len())
                        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
                })
            {
                entry.to_owned()
            } else {
                format!("data:image/png;base64,{entry}")
            }
        });
    if native {
        let fallback = json!("auto");
        let fallback_field = |key| {
            input
                .get(key)
                .filter(|value| truthy(value))
                .unwrap_or(&fallback)
        };
        return if edit {
            Ok((
                Mode::Edit,
                json!({
                    "model":model,"prompt":input["prompt"],"images":refs.map(|url| json!({"image_url":url})).collect::<Vec<_>>(),
                    "n":1,"size":"auto","quality":fallback_field("quality"),"background":fallback_field("background"),
                }),
            ))
        } else {
            Ok((
                Mode::Generate,
                json!({
                    "model":model,"prompt":input["prompt"],"n":1,"size":"auto",
                    "quality":fallback_field("quality"),"background":fallback_field("background"),"output_format":"png",
                }),
            ))
        };
    }
    let mut tool = json!({"type":"image_generation","output_format":format});
    let fallback = json!("high");
    let detail = input
        .get("image_detail")
        .filter(|value| truthy(value))
        .unwrap_or(&fallback);
    let mut content = Vec::new();
    for (index, url) in refs.enumerate() {
        content.extend([
            json!({"type":"input_text","text":format!("<image name=image{}>",index+1)}),
            json!({"type":"input_image","image_url":url,"detail":detail}),
            json!({"type":"input_text","text":"</image>"}),
        ]);
    }
    if hosted {
        tool["action"] = json!(if content.is_empty() {
            "generate"
        } else {
            "edit"
        });
        tool["model"] = json!(model);
    }
    for field in ["size", "quality", "background"] {
        if truthy(&input[field]) {
            tool[field] = input[field].clone();
        }
    }
    content.push(json!({"type":"input_text","text":input["prompt"]}));
    Ok((
        Mode::Responses,
        json!({
            "model":if hosted {MAIN_MODEL} else {model.strip_suffix("-image").unwrap_or(model)},
            "instructions":"",
            "input":[{"type":"message","role":"user","content":content}],
            "tools":[tool],
            "tool_choice":if hosted {json!({"type":"image_generation"})} else {json!("auto")},
            "parallel_tool_calls":false,
            "prompt_cache_key":Uuid::new_v4(),
            "stream":true,
            "store":false,
            "reasoning":null,
        }),
    ))
}

async fn current(
    storage: &Storage,
    observed: &Value,
    endpoint: &Url,
    expected: &Delivery,
) -> Result<Value, QueuedImageError> {
    let id = text(&observed["id"]).ok_or_else(route_error)?.to_owned();
    let row = storage
        .run(false, move |db| by_id(db, &id))
        .await
        .ok()
        .flatten()
        .filter(|row| row["isActive"] == true && row["provider"] == "codex")
        .ok_or_else(route_error)?;
    let settings = storage.read_settings().await.ok_or_else(route_error)?;
    if !matches!(Delivery::select(storage, &settings, &row, endpoint.as_str()).await, Ok(delivery) if &delivery == expected)
    {
        return Err(route_error());
    }
    Ok(row)
}

fn renewable(account: &Value) -> bool {
    account["authType"] == "oauth" && text(&account["refreshToken"]).is_some()
}

async fn renew(
    state: &ImageState,
    observed: &Value,
    endpoint: &Url,
    expected: &Delivery,
) -> Result<Option<Value>, QueuedImageError> {
    let row = current(&state.storage, observed, endpoint, expected).await?;
    if row["accessToken"] != observed["accessToken"]
        || row["refreshToken"] != observed["refreshToken"]
    {
        return Ok(Some(row));
    }
    let settings = match state.storage.read_settings().await {
        Some(settings) => settings,
        None => return Err(route_error()),
    };
    let token_url = test_token_url("codex").ok_or_else(route_error)?;
    let delivery = Delivery::select(&state.storage, &settings, &row, token_url)
        .await
        .map_err(|_| route_error())?;
    match refresh_account(
        &state.storage,
        &row,
        RefreshMode::Routed(delivery.protected()),
    )
    .await
    {
        Ok(updated) => current(&state.storage, &updated, endpoint, expected)
            .await
            .map(Some),
        Err(_) => {
            let verified = current(&state.storage, &row, endpoint, expected).await?;
            if verified["accessToken"] != observed["accessToken"] {
                return Ok(Some(verified));
            }
            if expected.protected() || delivery.protected() {
                Err(QueuedImageError::Protected(ImageError::bad_gateway(
                    "Codex image credential refresh failed",
                )))
            } else {
                Ok(None)
            }
        }
    }
}

async fn send(
    state: &ImageState,
    endpoint: &Url,
    delivery: &Delivery,
    account: &Value,
    mode: Mode,
    body: &Value,
) -> Result<reqwest::Response, QueuedImageError> {
    let selected = current(&state.storage, account, endpoint, delivery).await?;
    if !matches!(
        selected["authType"].as_str(),
        Some("oauth" | "access_token")
    ) || selected["accessToken"] != account["accessToken"]
    {
        return Err(route_error());
    }
    let token = text(&selected["accessToken"])
        .filter(|token| token.bytes().all(|c| c.is_ascii_graphic()))
        .ok_or_else(|| {
            before(
                StatusCode::UNAUTHORIZED,
                "A valid Codex access token is required",
            )
        })?;
    let workspace = account_id(&selected);
    let version = crate::codex_executor::cli_version();
    let destination = if delivery.use_fixture() {
        format!("{FIXTURE}{}", endpoint.path())
    } else {
        endpoint.to_string()
    };
    let client = delivery
        .client(&state.protected, IMAGE_TIMEOUT)
        .ok_or_else(route_error)?;
    let request = client
        .post(destination)
        .timeout(IMAGE_TIMEOUT)
        .bearer_auth(token)
        .header(
            header::ACCEPT,
            if mode == Mode::Responses {
                "text/event-stream, application/json"
            } else {
                "application/json"
            },
        )
        .header("ChatGPT-Account-ID", workspace)
        .header("originator", "codex_cli_rs")
        .header("session_id", Uuid::new_v4().to_string())
        .header(header::USER_AGENT, format!("codex_cli_rs/{version}"))
        .header("version", version)
        .header("x-client-request-id", Uuid::new_v4().to_string())
        .json(body);
    let response = tokio::time::timeout(CONNECT_TIMEOUT, request.send())
        .await
        .map_err(|_| accepted("[502]: Codex image upstream timed out"))?
        .map_err(|_| accepted("[502]: Codex image upstream fetch failed"))?;
    if response.status().is_redirection() {
        return Err(accepted(
            "Codex image upstream returned an unexpected redirect",
        ));
    }
    Ok(response)
}

fn plan_type(account: &Value) -> Option<String> {
    for token in ["accessToken", "idToken"]
        .into_iter()
        .filter_map(|key| text(&account[key]))
    {
        if let Some(plan) = text(&token_claims(token)[AUTH_CLAIM]["chatgpt_plan_type"]) {
            return Some(plan.to_owned());
        }
    }
    text(&account["providerSpecificData"]["chatgptPlanType"]).map(str::to_owned)
}

fn account_id(account: &Value) -> String {
    text(&account["providerSpecificData"]["chatgptAccountId"])
        .map(str::to_owned)
        .or_else(|| {
            text(&account["idToken"]).and_then(|token| {
                text(&token_claims(token)[AUTH_CLAIM]["chatgpt_account_id"]).map(str::to_owned)
            })
        })
        .unwrap_or_default()
}

fn secrets_of(account: &Value) -> Vec<String> {
    ["accessToken", "apiKey", "refreshToken", "idToken"]
        .into_iter()
        .filter_map(|key| text(&account[key]).map(str::to_owned))
        .collect()
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn redact(message: &str, secrets: &[String]) -> String {
    let message = secrets.iter().fold(message.to_owned(), |message, secret| {
        message.replace(secret, "[redacted]")
    });
    crate::cli_video::redact_bearer(&message)
}

async fn rejected(response: reqwest::Response, secrets: &[String]) -> ImageError {
    let status = response.status();
    let raw = bounded(response, 256 * 1024).await.unwrap_or_default();
    let parsed = serde_json::from_slice::<Value>(&raw).unwrap_or_default();
    let message = [
        &parsed["error"]["message"],
        &parsed["message"],
        &parsed["error"],
    ]
    .into_iter()
    .find(|value| truthy(value))
    .map(js_text)
    .filter(|message| !message.is_empty())
    .unwrap_or_else(|| {
        if raw.is_empty() {
            status
                .canonical_reason()
                .unwrap_or("Codex image request failed")
                .to_owned()
        } else {
            String::from_utf8_lossy(&raw).into_owned()
        }
    });
    ImageError {
        status,
        message: format!("[{}]: {}", status.as_u16(), redact(&message, secrets)),
    }
}

fn before(status: StatusCode, message: impl Into<String>) -> QueuedImageError {
    QueuedImageError::BeforeSubmit(ImageError {
        status,
        message: message.into(),
    })
}

fn accepted(message: impl Into<String>) -> QueuedImageError {
    QueuedImageError::PossiblyAccepted(ImageError::bad_gateway(message))
}

fn route_error() -> QueuedImageError {
    QueuedImageError::Protected(ImageError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "Codex image connection proxy or account changed".into(),
    })
}

fn rejection(error: ImageError, delivery: &Delivery) -> QueuedImageError {
    if delivery.protected() {
        QueuedImageError::Protected(error)
    } else {
        QueuedImageError::BeforeSubmit(error)
    }
}

async fn read_json(response: reqwest::Response, limit: usize) -> Option<Value> {
    serde_json::from_slice(&bounded(response, limit).await?).ok()
}

async fn bounded(mut response: reqwest::Response, limit: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > limit - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(bytes)
}

struct Effects {
    storage: Storage,
    connection: String,
    model: String,
    api_key: Option<String>,
    endpoint: String,
}

impl Effects {
    async fn success(self, usage: Option<Value>) {
        let _ = chat_accounts::mark_success(&self.storage, &self.connection, &self.model).await;
        if let Some(usage) = usage {
            let callback = crate::chat_usage::exact_callback(
                self.storage,
                "codex".to_owned(),
                self.model,
                self.connection,
                self.api_key,
                self.endpoint,
            );
            callback(usage);
        }
    }
}

fn normalize_usage(value: &Value) -> Option<Value> {
    let read = |field| value.get(field).filter(|value| !value.is_null());
    let safe = |field: &Value| {
        field
            .as_f64()
            .filter(|value| value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0)
            .map(|value| value as i64)
    };
    let count = |field| safe(field).filter(|count| *count >= 0);
    let input = count(read("input_tokens").or_else(|| read("prompt_tokens"))?)?;
    let output = count(read("output_tokens").or_else(|| read("completion_tokens"))?)?;
    let total = safe(&value["total_tokens"]).unwrap_or(input + output);
    let mut result = json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":total});
    for (name, left, right) in [
        (
            "cached_tokens",
            &value["input_tokens_details"]["cached_tokens"],
            &value["prompt_tokens_details"]["cached_tokens"],
        ),
        (
            "reasoning_tokens",
            &value["output_tokens_details"]["reasoning_tokens"],
            &value["completion_tokens_details"]["reasoning_tokens"],
        ),
    ] {
        if let Some(tokens) = count(if left.is_null() { right } else { left }) {
            result[name] = json!(tokens);
        }
    }
    Some(result)
}
