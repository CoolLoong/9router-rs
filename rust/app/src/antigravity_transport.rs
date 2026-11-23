use std::sync::LazyLock;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode, header};
use reqwest::{Client, Method, Response};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::chat_accounts;
use crate::chat_format::ChatFormat;
use crate::chat_input::{PreparedChat, stream_requested, truthy};
use crate::chat_output::js_text;
use crate::codex_executor::Sessions;
use crate::gemini_request;
use crate::google_code_assist::{self, ProjectError};
use crate::provider_connections::by_id;
use crate::provider_http::{self, Bound, http_client, requires_protection};
use crate::provider_index::js_trim;
use crate::provider_oauth::{
    RefreshMode, refresh_account, refresh_due_for_provider, test_token_url,
};
use crate::storage::Storage;

const GENERATE: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal:generateContent";
const STREAM: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse";
const FIXTURE: &str = "http://127.0.0.1:52989/antigravity/v1internal:";
const REGISTRY: &str = include_str!("../../../open-sse/providers/registry/antigravity.js");
const TIMEOUT: Duration = Duration::from_secs(120);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
const LIMIT: usize = 128 * 1024;
static DIRECT: LazyLock<Option<Client>> = LazyLock::new(|| http_client(TIMEOUT).build().ok());
static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    let package: Value =
        serde_json::from_str(include_str!("../../../package.json")).unwrap_or_default();
    format!("9Router/{}", package["version"].as_str().unwrap_or("0.0.0"))
});

pub(super) struct Failure {
    pub(super) status: StatusCode,
    pub(super) message: String,
    pub(super) headers: Box<HeaderMap>,
}

impl Failure {
    pub(super) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            headers: Box::default(),
        }
    }

    fn route() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Antigravity connection proxy is unavailable or changed",
        )
    }
}

pub(super) fn supports(account: &Value) -> bool {
    account["provider"] == "antigravity" && account["authType"] == "oauth"
}

pub(super) async fn protects_chat(storage: &Storage, settings: &Value) -> bool {
    requires_protection(settings, &Value::Null)
        || chat_accounts::active_connections(storage, "antigravity")
            .await
            .map_or(true, |accounts| {
                accounts
                    .iter()
                    .any(|account| requires_protection(settings, account))
            })
}

pub(super) fn prepare(
    format: ChatFormat,
    mut body: Value,
    model: &str,
    accept: &str,
    signature: impl Fn(&str) -> Option<String>,
) -> Option<PreparedChat> {
    let model = resolve_model(model, &body);

    let mut prepared = if body["request"]["contents"].is_array() || body["contents"].is_array() {
        let source = if body["request"]["contents"].is_array() {
            &body["request"]
        } else {
            &body
        };
        let mut value = json!({"model":crate::thinking_intent::model_suffix(&model).0});
        for field in ["contents", "generationConfig", "systemInstruction", "tools"] {
            if let Some(data) = source.get(field) {
                value[field] = data.clone();
            }
        }
        PreparedChat {
            body: value,
            stream: stream_requested(&body, accept),
        }
    } else {
        let (plain, _) = model_suffix(&model);
        let caps =
            provider_capabilities_rs::port::model_capabilities(Some("antigravity"), plain, None);
        let model = if caps["thinkingFormat"]
            .as_str()
            .is_some_and(|format| format.starts_with("claude-"))
        {
            crate::claude_thinking::strip(&mut body);
            plain
        } else {
            &model
        };
        gemini_request::prepare(format, body, model, accept, signature, "")?
    };

    let image = prepared.body["model"].as_str().is_some_and(image_model);
    let fields = prepared.body.as_object_mut()?;
    fields.shift_remove("safetySettings");
    if image {
        let model = fields.get("model")?.as_str()?;
        let (base, ratio) = image_configuration(model);
        let base = base.to_owned();
        fields.insert("model".into(), json!(base));
        let contents = fields.get_mut("contents")?.as_array_mut()?;
        contents.retain_mut(|turn| {
            let Some(parts) = turn["parts"].as_array_mut() else {
                return false;
            };
            parts.retain_mut(|part| {
                if let Some(text) = part.get("text") {
                    *part = json!({"text":text});
                    true
                } else if truthy(&part["inlineData"]["data"]) {
                    *part = json!({"inlineData":part["inlineData"]});
                    true
                } else {
                    false
                }
            });
            let has_contents = !parts.is_empty();
            if !truthy(&turn["role"]) {
                turn["role"] = json!("user");
            }
            has_contents
        });
        fields.shift_remove("systemInstruction");
        fields.shift_remove("tools");
        fields.insert(
            "generationConfig".into(),
            json!({
                "temperature":1,"topP":0.95,"topK":40,"maxOutputTokens":8192,
                "imageConfig":{"aspectRatio":ratio},
            }),
        );
        prepared.stream = false;
        return Some(prepared);
    }

    if let Some(contents) = fields.get_mut("contents").and_then(Value::as_array_mut) {
        for turn in contents.iter_mut() {
            let Some(parts) = turn["parts"].as_array_mut() else {
                continue;
            };
            parts.retain_mut(|part| {
                if !truthy(&part["functionCall"]) {
                    return !(truthy(&part["thought"])
                        || part.get("thoughtSignature").is_some() && !truthy(&part["text"]));
                }
                if let Some(fields) = part.as_object_mut()
                    && fields
                        .get("thoughtSignature")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                {
                    fields.shift_remove("thoughtSignature");
                }
                true
            });
            if parts.iter().any(|part| truthy(&part["functionResponse"])) {
                turn["role"] = json!("user");
            }
        }
        contents.retain(|turn| {
            turn["parts"]
                .as_array()
                .is_some_and(|parts| !parts.is_empty())
        });
    }
    if fields
        .get("generationConfig")
        .is_none_or(|value| !value.is_object())
    {
        fields.insert("generationConfig".into(), json!({}));
    }
    if let Some(generation) = fields.get_mut("generationConfig")
        && crate::thinking_intent::numeric(&generation["maxOutputTokens"])
            .is_some_and(|value| value > 64_000.0)
    {
        generation["maxOutputTokens"] = json!(64_000);
    }
    Some(prepared)
}

fn model_suffix(model: &str) -> (&str, Option<&str>) {
    model
        .trim_end()
        .strip_suffix(')')
        .and_then(|value| value.rsplit_once('('))
        .filter(|(_, suffix)| !suffix.is_empty() && !suffix.contains(')'))
        .map_or((model, None), |(name, suffix)| {
            (js_trim(name), Some(suffix))
        })
}

fn resolve_model(model: &str, body: &Value) -> String {
    let (base, suffix) = model_suffix(model);
    let effort = suffix.or_else(|| {
        [
            &body["reasoning_effort"],
            &body["output_config"]["effort"],
            &body["reasoning"]["effort"],
        ]
        .into_iter()
        .find(|value| truthy(value))
        .and_then(Value::as_str)
    });
    let effort = effort.map(|effort| js_trim(effort).to_ascii_lowercase());
    let tier = match effort.as_deref() {
        Some("xhigh" | "max") => Some("high"),
        Some("minimal") => Some("low"),
        Some(value) if !value.is_empty() => Some(value),
        _ => crate::thinking_intent::numeric(&body["thinking"]["budget_tokens"])
            .filter(|budget| *budget > 0.0)
            .map(|budget| {
                if budget > 16_384.0 {
                    "high"
                } else if budget <= 4_096.0 {
                    "low"
                } else {
                    "medium"
                }
            }),
    };
    let resolved = tier
        .and_then(|tier| registry_model(&format!("{base}-{tier}")))
        .or_else(|| registry_model(base));
    let Some(resolved) = resolved else {
        return model.to_owned();
    };
    let (model, preset) = model_suffix(resolved);
    let applied = suffix.or_else(|| preset.and(tier)).or(preset);
    if let Some(level) = applied {
        format!("{model}({level})")
    } else {
        model.to_owned()
    }
}

fn registry_model(model: &str) -> Option<&'static str> {
    REGISTRY.lines().find_map(|line| {
        let row = line.trim().strip_prefix("{ id: \"")?;
        let (id, fields) = row.split_once('"')?;
        (id == model).then(|| {
            fields
                .split_once("upstreamModelId: \"")
                .and_then(|(_, value)| value.split_once('"'))
                .map_or(id, |(value, _)| value)
        })
    })
}

pub(super) async fn complete(
    storage: &Storage,
    settings: &Value,
    observed: &Value,
    source_body: &Value,
    prepared: &PreparedChat,
    headers: &HeaderMap,
    sessions: &Sessions,
) -> Result<(Response, Vec<String>), Failure> {
    execute(
        storage,
        settings,
        observed,
        Operation::chat(prepared.stream),
        |account, project| {
            chat_envelope(
                source_body,
                &prepared.body,
                headers,
                sessions,
                account,
                project,
            )
        },
    )
    .await
}

pub(super) fn preview(
    body: &Value,
    model: &str,
    account: &Value,
    headers: &HeaderMap,
    sessions: &Sessions,
) -> Result<Value, Failure> {
    if !supports(account) {
        return Err(Failure::new(
            StatusCode::UNAUTHORIZED,
            "Missing Antigravity OAuth account credentials",
        ));
    }
    let project = project(account).ok_or_else(|| {
        Failure::new(
            StatusCode::BAD_REQUEST,
            "An authorized Antigravity project is required",
        )
    })?;
    let prepared = prepare(ChatFormat::Chat, body.clone(), model, "", |_| None)
        .ok_or_else(|| Failure::new(StatusCode::BAD_REQUEST, "Invalid Antigravity request body"))?;
    let mut envelope = chat_envelope(body, &prepared.body, headers, sessions, account, project)?;
    if !prepared.stream {
        envelope["stream"] = json!(false);
    }
    Ok(
        json!({"url":Operation::chat(prepared.stream).endpoint(),"headers":{
        "Content-Type":"application/json", "Authorization":"[redacted]", "User-Agent":USER_AGENT.as_str(),
    },"body":envelope}),
    )
}

fn chat_envelope(
    source_body: &Value,
    body: &Value,
    headers: &HeaderMap,
    sessions: &Sessions,
    account: &Value,
    project: &str,
) -> Result<Value, Failure> {
    let requested = body["model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .ok_or_else(|| Failure::new(StatusCode::BAD_REQUEST, "Antigravity requires a model"))?;
    let supplied = source_body["request"]["sessionId"]
        .as_str()
        .map(js_trim)
        .filter(|id| !id.is_empty() && id.encode_utf16().count() <= 256);
    let raw_session = supplied
        .map(str::to_owned)
        .unwrap_or_else(|| sessions.pick(source_body, account, headers, None));
    let image = image_model(requested);
    let session = if image {
        raw_session
    } else {
        numeric_session(raw_session)
    };
    let mut request = body.clone();
    let fields = request
        .as_object_mut()
        .ok_or_else(|| Failure::new(StatusCode::BAD_REQUEST, "Invalid Antigravity request"))?;
    fields.shift_remove("model");
    if request["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty())
    {
        request["toolConfig"] = json!({"functionCallingConfig":{"mode":"VALIDATED"}});
    }
    request["sessionId"] = json!(session);
    let kind = if image { "image_gen" } else { "agent" };
    let mut envelope = json!({
        "project":project,"model":requested,"userAgent":"antigravity",
        "requestId":request_id(&session, requested, kind, &request),"request":request,
    });
    if image {
        envelope["requestType"] = json!("image_gen");
    }
    Ok(envelope)
}

pub(super) async fn search(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    query: &str,
    model: &str,
) -> Result<(Response, Vec<String>), Failure> {
    execute(
        storage,
        settings,
        account,
        Operation::Search,
        |_, project| {
            Ok(json!({
                "project":project,"model":model,"userAgent":"antigravity","requestType":"search",
                "request":{
                    "contents":[{"role":"user","parts":[{"text":query}]}],
                    "tools":[{"googleSearch":{}}],
                    "generationConfig":{"temperature":1,"maxOutputTokens":8192},
                },
            }))
        },
    )
    .await
}

pub(super) fn search_model() -> Option<&'static str> {
    REGISTRY
        .split_once("searchViaChat: {")?
        .1
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("defaultModel: \"")?
                .split_once('"')
                .map(|(model, _)| model)
        })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Generate,
    Stream,
    Search,
}

impl Operation {
    fn chat(stream: bool) -> Self {
        if stream { Self::Stream } else { Self::Generate }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::Stream => STREAM,
            Self::Generate | Self::Search => GENERATE,
        }
    }

    fn timeout(self) -> Duration {
        if self == Self::Search {
            SEARCH_TIMEOUT
        } else {
            TIMEOUT
        }
    }
}

async fn execute(
    storage: &Storage,
    settings: &Value,
    observed: &Value,
    operation: Operation,
    render: impl FnOnce(&Value, &str) -> Result<Value, Failure>,
) -> Result<(Response, Vec<String>), Failure> {
    let delivery = bind_operation(storage, settings, observed, operation)
        .await
        .ok_or_else(Failure::route)?;
    let relay = delivery.route.relay_identity();
    let mut account = current(storage, observed, operation, &delivery).await?;
    let mut secrets = secrets_of(&account);
    secrets.extend(delivery.relay_secret().map(str::to_owned));
    if operation == Operation::Search && project(&account).is_none() {
        return Err(Failure::new(
            StatusCode::UNAUTHORIZED,
            "Antigravity account has no projectId — reconnect the account",
        ));
    }
    let renewable = token(&account, "refreshToken").is_some();
    let mut refreshed = renewable
        && (token(&account, "accessToken").is_none()
            || refresh_due_for_provider("antigravity", &account));
    if refreshed {
        let updated = renew(storage, &account, operation, &delivery, project(&account)).await?;
        secrets.extend(secrets_of(&updated));
        account = updated;
    }
    if project(&account).is_none() {
        let mut discovered = google_code_assist::discover_project(storage, &account, relay).await;
        if matches!(discovered, Err(ProjectError::Rejected)) && renewable && !refreshed {
            account = renew(storage, &account, operation, &delivery, None).await?;
            secrets.extend(secrets_of(&account));
            refreshed = true;
            discovered = google_code_assist::discover_project(storage, &account, relay).await;
        }
        let discovered = discovered.map_err(|error| match error {
            ProjectError::AccountChanged | ProjectError::Route => Failure::route(),
            ProjectError::Rejected => Failure::new(
                StatusCode::UNAUTHORIZED,
                "Antigravity project discovery rejected the OAuth credentials",
            ),
            ProjectError::Missing => missing_project(),
        })?;
        account = current(storage, &discovered, operation, &delivery).await?;
        secrets.extend(secrets_of(&account));
    }
    let project = project(&account).ok_or_else(missing_project)?;
    let envelope = render(&account, project)?;
    let mut response = send(&delivery, operation, &account, &envelope).await?;
    if response.status() == StatusCode::UNAUTHORIZED && renewable && !refreshed {
        let updated = renew(storage, &account, operation, &delivery, Some(project)).await?;
        if token(&updated, "accessToken") != token(&account, "accessToken") {
            secrets.extend(secrets_of(&updated));
            drop(response);
            response = send(&delivery, operation, &updated, &envelope).await?;
        }
    }
    if response.status().is_success() {
        return Ok((response, secrets));
    }
    Err(read_error(response, &secrets, operation).await)
}

async fn bind_operation(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    operation: Operation,
) -> Option<Bound> {
    let endpoint = operation.endpoint();
    let selection = provider_http::select(storage, settings, account, endpoint)
        .await
        .ok()?;
    let (_, action) = endpoint.rsplit_once(':')?;
    let fixture = format!("{FIXTURE}{action}");
    provider_http::bind_selection(
        selection,
        endpoint,
        &fixture,
        DIRECT.as_ref()?,
        operation.timeout(),
        None,
    )
}

async fn current(
    storage: &Storage,
    observed: &Value,
    operation: Operation,
    expected: &Bound,
) -> Result<Value, Failure> {
    let id = observed["id"]
        .as_str()
        .ok_or_else(Failure::route)?
        .to_owned();
    let account = storage
        .run(false, move |db| by_id(db, &id))
        .await
        .ok()
        .flatten()
        .filter(|record| supports(record) && record["isActive"] == true)
        .ok_or_else(Failure::route)?;
    let settings = storage.read_settings().await.ok_or_else(Failure::route)?;
    if !bind_operation(storage, &settings, &account, operation)
        .await
        .is_some_and(|actual| expected.same_route(&actual))
    {
        return Err(Failure::route());
    }
    Ok(account)
}

async fn renew(
    storage: &Storage,
    account: &Value,
    operation: Operation,
    expected: &Bound,
    project_id: Option<&str>,
) -> Result<Value, Failure> {
    let latest = current(storage, account, operation, expected).await?;
    if project_id.is_some_and(|id| project(&latest) != Some(id)) {
        return Err(Failure::route());
    }
    if token(&latest, "accessToken") != token(account, "accessToken") {
        return Ok(latest);
    }
    let mode = if let Some(identity) = expected.route.relay_identity() {
        RefreshMode::RelayRouted(identity)
    } else {
        let token_endpoint = test_token_url("antigravity").ok_or_else(Failure::route)?;
        let token_route = provider_http::bind(
            storage,
            &latest,
            token_endpoint,
            token_endpoint,
            DIRECT.as_ref().ok_or_else(Failure::route)?,
            TIMEOUT,
            None,
        )
        .await
        .ok_or_else(Failure::route)?;
        token_route.route.relay_identity().map_or(
            RefreshMode::Routed(token_route.protected),
            RefreshMode::RelayRouted,
        )
    };
    let updated = refresh_account(storage, &latest, mode).await.map_err(|_| {
        Failure::new(
            StatusCode::UNAUTHORIZED,
            "Antigravity credential refresh failed",
        )
    })?;
    let latest = current(storage, &updated, operation, expected).await?;
    if project_id.is_some_and(|id| project(&latest) != Some(id)) {
        return Err(Failure::route());
    }
    Ok(latest)
}

async fn send(
    delivery: &Bound,
    operation: Operation,
    account: &Value,
    body: &Value,
) -> Result<Response, Failure> {
    let token = token(account, "accessToken").ok_or_else(|| {
        Failure::new(
            StatusCode::UNAUTHORIZED,
            "Valid Antigravity OAuth credentials are required",
        )
    })?;
    let response = delivery
        .request(Method::POST)
        .bearer_auth(token)
        .header(header::USER_AGENT, USER_AGENT.as_str())
        .json(body)
        .send()
        .await
        .map_err(|error| {
            Failure::new(
                if error.is_timeout() {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                },
                if operation == Operation::Search && error.is_timeout() {
                    "Upstream timeout"
                } else {
                    "Antigravity upstream request failed"
                },
            )
        })?;
    if response.status().is_redirection() {
        return Err(Failure::new(
            StatusCode::BAD_GATEWAY,
            "Antigravity upstream returned an unexpected redirect",
        ));
    }
    Ok(response)
}

fn project(account: &Value) -> Option<&str> {
    google_code_assist::saved_project(account).filter(|project| {
        project
            .bytes()
            .all(|character| character.is_ascii_graphic())
    })
}

fn missing_project() -> Failure {
    Failure::new(
        StatusCode::BAD_REQUEST,
        "An authorized Antigravity project is required",
    )
}

fn token<'a>(account: &'a Value, field: &str) -> Option<&'a str> {
    account[field]
        .as_str()
        .filter(|value| !value.is_empty() && value.bytes().all(|value| value.is_ascii_graphic()))
}

fn secrets_of(account: &Value) -> Vec<String> {
    ["accessToken", "refreshToken", "apiKey", "idToken"]
        .into_iter()
        .filter_map(|field| token(account, field).map(str::to_owned))
        .collect()
}

async fn read_error(mut response: Response, secrets: &[String], operation: Operation) -> Failure {
    let status = response.status();
    let mut headers = HeaderMap::new();
    if let Some(retry) = response.headers().get(header::RETRY_AFTER) {
        headers.insert(header::RETRY_AFTER, retry.clone());
    }
    let mut output = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if chunk.len() > LIMIT - output.len() {
            break;
        }
        output.extend_from_slice(&chunk);
    }
    let parsed = match serde_json::from_slice::<Value>(&output) {
        Ok(parsed) => parsed,
        Err(_) if operation == Operation::Search => {
            return Failure::new(
                StatusCode::BAD_GATEWAY,
                format!("Invalid upstream response (status {})", status.as_u16()),
            );
        }
        Err(_) => Value::Null,
    };
    let fields = if operation == Operation::Search {
        [
            &parsed["error"]["message"],
            &parsed["error"],
            &parsed["message"],
        ]
    } else {
        [
            &parsed["error"]["message"],
            &parsed["message"],
            &parsed["error"],
        ]
    };
    let raw = fields
        .into_iter()
        .find(|value| truthy(value))
        .map(|value| {
            if operation == Operation::Search && !value.is_string() {
                value.to_string()
            } else {
                js_text(value)
            }
        })
        .or_else(|| {
            (operation == Operation::Search).then(|| format!("Upstream HTTP {}", status.as_u16()))
        })
        .or_else(|| (!output.is_empty()).then(|| String::from_utf8_lossy(&output).into_owned()))
        .unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("Antigravity rejected the request")
                .to_owned()
        });
    let message = secrets
        .iter()
        .fold(raw, |message, secret| message.replace(secret, "[redacted]"));
    let message = crate::cli_video::redact_bearer(&message);
    let message = if operation == Operation::Search {
        message
    } else {
        format!("[{}]: {message}", status.as_u16())
    };
    Failure {
        status,
        message,
        headers: headers.into(),
    }
}

fn numeric_session(session: String) -> String {
    let digits = session.strip_prefix('-').unwrap_or(&session);
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return session;
    }
    let bytes = Sha256::digest(session.as_bytes());
    let head = u64::from_be_bytes(std::array::from_fn(|index| bytes[index]));
    format!("-{}", head & 0x7fff_ffff_ffff_ffff)
}

fn request_id(session: &str, model: &str, kind: &str, request: &Value) -> String {
    let conversation = scoped_uuid(&format!("antigravity:conversation:{session}"));
    let trajectory = scoped_uuid(&format!("antigravity:trajectory:{session}:{model}:{kind}"));
    let steps = request["contents"]
        .as_array()
        .map_or(1, |entries| entries.len());
    let steps = steps.saturating_mul(2).saturating_sub(1).max(1);
    format!(
        "agent/{conversation}/{}/{trajectory}/{steps}",
        chrono::Utc::now().timestamp_millis()
    )
}

fn scoped_uuid(seed: &str) -> Uuid {
    let digest = Sha256::digest(seed.as_bytes());
    let mut bytes = std::array::from_fn(|index| digest[index]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

pub(super) fn image_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("image")
}

fn image_configuration(model: &str) -> (&str, String) {
    let dimensions = model.rsplit_once('-').and_then(|(base, suffix)| {
        let (width, height) = suffix.split_once('x')?;
        if !width.bytes().all(|byte| byte.is_ascii_digit())
            || !height.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let width = width.parse::<u64>().ok().filter(|value| *value > 0)?;
        let height = height.parse::<u64>().ok().filter(|value| *value > 0)?;
        Some((base, width, height))
    });
    let Some((base, width, height)) = dimensions else {
        return (model, "1:1".into());
    };
    if width <= 16 && height <= 16 {
        return (base, format!("{width}:{height}"));
    }
    let (mut left, mut right) = (width, height);
    while right != 0 {
        (left, right) = (right, left % right);
    }
    (base, format!("{}:{}", width / left, height / left))
}
