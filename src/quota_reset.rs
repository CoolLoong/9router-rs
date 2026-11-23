use std::sync::LazyLock;
use std::time::Duration;

use axum::Json;
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use reqwest::{Client, Method, RequestBuilder, header};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    Failure, QuotaState, access, active_refreshable, auth_expired, load, parse, pinned_field,
    proactive, proxy_unavailable, unauthorized,
};
use crate::dashboard_session;
use crate::provider_connections::truthy;
use crate::provider_http::{self, Bound};
use crate::provider_oauth::RefreshMode;

const REQUEST_LIMIT: usize = 32 * 1024;
const REPLY_LIMIT: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

struct Endpoints {
    codex: String,
    claude_profile: String,
    claude_reset: String,
}

static ENDPOINTS: LazyLock<Endpoints> = LazyLock::new(|| Endpoints {
    codex: pinned_field("codex.usage", "resetCreditsConsumeUrl").expect("Codex reset endpoint"),
    claude_profile: pinned_field("claude.usage", "profileUrl").expect("Claude profile endpoint"),
    claude_reset: pinned_field("claude.usage", "resetUrl").expect("Claude reset endpoint"),
});

static CLIENT: LazyLock<Client> = LazyLock::new(|| {
    provider_http::http_client(TIMEOUT)
        .build()
        .expect("quota reset HTTP client")
});

pub(super) async fn codex(
    State(state): State<QuotaState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return unauthorized();
    }
    let account = match load(&state.storage, id).await {
        Ok(account) => account,
        Err(error) => return error.into_response(),
    };
    if account["provider"] != "codex" {
        return Failure::new(
            StatusCode::BAD_REQUEST,
            "Codex reset credits are only available for Codex connections.",
        )
        .into_response();
    }
    let oauth = account["authType"] == "oauth";
    if !oauth && account["authType"] != "access_token" {
        return Failure::new(
            StatusCode::BAD_REQUEST,
            "Codex reset credits require an OAuth or access-token connection.",
        )
        .into_response();
    }
    let route = match bind(
        &state,
        &account,
        &ENDPOINTS.codex,
        "/quota-reset/codex/consume",
    )
    .await
    {
        Ok(route) => route,
        Err(error) => return error.into_response(),
    };
    let original_account = account;
    let account = if oauth {
        match proactive(
            &state,
            &original_account,
            RefreshMode::QuotaRouted { rejected: false },
        )
        .await
        {
            Ok(Some(account)) => account,
            Ok(None) => original_account.clone(),
            Err(error) => return error.into_response(),
        }
    } else {
        original_account.clone()
    };
    let binding = match bind(
        &state,
        &account,
        &ENDPOINTS.codex,
        "/quota-reset/codex/consume",
    )
    .await
    {
        Ok(binding) if binding.route == route.route && binding.protected == route.protected => {
            binding
        }
        _ => return proxy_unavailable().into_response(),
    };
    let redeem_id = Uuid::new_v4().to_string();
    let mut result = match consume_codex(&binding, &account, &redeem_id).await {
        Ok(result) => result,
        Err(error) => return error.into_response(),
    };
    let previous = account;
    let retry = oauth && active_refreshable(&previous) && result.auth_rejected();
    let mut refreshed = Value::Null;
    if retry
        && let Ok(Some(current)) = proactive(
            &state,
            &previous,
            RefreshMode::QuotaRouted { rejected: true },
        )
        .await
        && access(&current).is_some()
        && access(&previous) != access(&current)
        && let Ok(binding) = bind(
            &state,
            &current,
            &ENDPOINTS.codex,
            "/quota-reset/codex/consume",
        )
        .await
        && binding.route == route.route
        && binding.protected == route.protected
    {
        result = match consume_codex(&binding, &current, &redeem_id).await {
            Ok(retried) => retried,
            Err(error) => return error.into_response(),
        };
        refreshed = current;
    }
    let (status, mut body) = result.reply(&redeem_id);
    redact(&mut body, &[&original_account, &previous, &refreshed]);
    (status, Json(body)).into_response()
}

pub(super) async fn claude(
    State(state): State<QuotaState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return unauthorized();
    }
    let account = match load(&state.storage, id).await {
        Ok(account) => account,
        Err(error) => return error.into_response(),
    };
    if account["provider"] != "claude" || account["authType"] != "oauth" {
        return Failure::new(
            StatusCode::BAD_REQUEST,
            "Limit reset is only available for Claude OAuth connections.",
        )
        .into_response();
    }
    let route = match bind(
        &state,
        &account,
        &ENDPOINTS.claude_profile,
        "/quota-reset/claude/profile",
    )
    .await
    {
        Ok(route) => route,
        Err(error) => return error.into_response(),
    };
    let observed = account;
    let account = match proactive(
        &state,
        &observed,
        RefreshMode::QuotaRouted { rejected: false },
    )
    .await
    {
        Ok(Some(account)) => account,
        Ok(None) => observed.clone(),
        Err(error) => {
            return Failure::new(StatusCode::INTERNAL_SERVER_ERROR, error.reason).into_response();
        }
    };
    let profile = match bind(
        &state,
        &account,
        &ENDPOINTS.claude_profile,
        "/quota-reset/claude/profile",
    )
    .await
    {
        Ok(binding) if binding.route == route.route && binding.protected == route.protected => {
            binding
        }
        _ => return proxy_unavailable().into_response(),
    };
    let Some(token) = access(&account) else {
        return internal("No Claude access token available. Please re-authorize the connection.")
            .into_response();
    };
    let body = to_bytes(request.into_body(), REQUEST_LIMIT)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let Some(grant) = body
        .as_ref()
        .and_then(|value| value["grantId"].as_str())
        .filter(|id| valid_id(id, 40) && !id.bytes().any(|byte| byte.is_ascii_uppercase()))
    else {
        return internal("Invalid reset grant id.").into_response();
    };
    match consume_claude(&state, &profile, &account, token, grant).await {
        Ok((status, mut body)) => {
            redact(&mut body, &[&observed, &account]);
            (status, Json(body)).into_response()
        }
        Err(error) => error.into_response(),
    }
}

pub(super) async fn bind(
    state: &QuotaState,
    account: &Value,
    endpoint: &str,
    fixture_path: &str,
) -> Result<Bound, Failure> {
    provider_http::bind(
        &state.storage,
        account,
        endpoint,
        &format!("http://127.0.0.1:52989{fixture_path}"),
        &CLIENT,
        TIMEOUT,
        None,
    )
    .await
    .ok_or_else(proxy_unavailable)
}

struct CodexResult {
    status: StatusCode,
    data: Value,
}

impl CodexResult {
    fn accepted(&self) -> bool {
        self.status.is_success()
            && (self.data["code"] == "reset" || parse::finite(&self.data["windows_reset"]) > 0.0)
    }

    fn auth_rejected(&self) -> bool {
        self.status == StatusCode::UNAUTHORIZED
            || matches!(self.status, StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN)
                && ["code", "message", "detail", "error"]
                    .iter()
                    .any(|key| self.data[*key].as_str().is_some_and(auth_expired))
    }

    fn reply(self, redeem_id: &str) -> (StatusCode, Value) {
        let code = &self.data["code"];
        let windows = parse::number(parse::finite(&self.data["windows_reset"]));
        if self.accepted() {
            return (
                StatusCode::OK,
                json!({
                    "code": truthy(code).then_some(code),
                    "reset": true,
                    "windows_reset": windows,
                    "redeemRequestId": redeem_id,
                    "credit": truthy(&self.data["credit"]).then_some(&self.data["credit"]),
                }),
            );
        }
        if self.status.is_success() && code == "no_credit" {
            return (
                StatusCode::CONFLICT,
                json!({
                    "code": "no_credit",
                    "reset": false,
                    "windows_reset": windows,
                    "message": "No Codex reset credits available.",
                }),
            );
        }
        let status = if self.status.is_client_error() {
            self.status
        } else {
            StatusCode::BAD_GATEWAY
        };
        (
            status,
            json!({
                "code": parse::value_or(code, json!("unknown_response")),
                "reset": false,
                "windows_reset": windows,
                "message": parse::value_or(
                    &self.data["message"], json!("Codex reset credit consume returned an unexpected response.")
                ),
            }),
        )
    }
}

async fn consume_codex(
    binding: &Bound,
    account: &Value,
    redeem_id: &str,
) -> Result<CodexResult, Failure> {
    let token = access(account).ok_or_else(|| {
        internal("No Codex access token available. Please re-authorize the connection.")
    })?;
    let request = binding
        .request(Method::POST)
        .bearer_auth(token)
        .header(header::ACCEPT, "application/json")
        .json(&json!({"redeem_request_id": redeem_id}));
    let response = request
        .send()
        .await
        .map_err(|_| internal("Failed to consume Codex reset credit: fetch failed"))?;
    let status = response.status();
    let data = bounded_json(response)
        .await
        .map_err(|_| internal("Failed to consume Codex reset credit: invalid provider response"))?;
    Ok(CodexResult { status, data })
}

async fn consume_claude(
    state: &QuotaState,
    profile: &Bound,
    account: &Value,
    token: &str,
    grant: &str,
) -> Result<(StatusCode, Value), Failure> {
    let protected = profile.protected;
    let headers = |request: RequestBuilder| {
        request
            .bearer_auth(token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("anthropic-version", &state.reader.config.anthropic_version)
            .header(header::USER_AGENT, &state.reader.config.claude_agent)
            .header(header::CONTENT_TYPE, "application/json")
    };
    let profile = headers(profile.request(Method::GET))
        .send()
        .await
        .map_err(|_| internal("fetch failed"))?;
    let profile_status = profile.status();
    let data = bounded_json(profile).await.unwrap_or_default();
    let org = data["organization"]["uuid"]
        .as_str()
        .filter(|id| profile_status.is_success() && valid_id(id, 128))
        .ok_or_else(|| {
            internal(format!(
                "Cannot resolve Claude organization ({}).",
                profile_status.as_u16()
            ))
        })?;
    let endpoint = ENDPOINTS.claude_reset.replace("{org_id}", org);
    let reset = bind(
        state,
        account,
        &endpoint,
        &format!("/quota-reset/claude/organizations/{org}/reset_rate_limits"),
    )
    .await?;
    if reset.protected != protected {
        return Err(proxy_unavailable());
    }
    let response = headers(reset.request(Method::POST))
        .json(&json!({
            "program": "cedar_ember",
            "grant_id": grant,
            "request_id": Uuid::new_v4().to_string(),
        }))
        .send()
        .await;
    let result = match response {
        Ok(response) => {
            let status = response.status();
            let data = bounded_json(response).await.unwrap_or_default();
            Ok(claude_reply(status, &data))
        }
        Err(_) => Err(internal("fetch failed")),
    };
    state.reader.invalidate_claude_usage(token);
    result
}

fn claude_reply(upstream: StatusCode, provider: &Value) -> (StatusCode, Value) {
    let accepted = upstream.is_success() && provider["result"] == "reset";
    let status = if accepted {
        StatusCode::OK
    } else if upstream.is_client_error() {
        upstream
    } else {
        StatusCode::CONFLICT
    };
    let result = &provider["result"];
    let reason = &provider["reason"];
    let message = &provider["error"]["message"];
    let message = if accepted || truthy(message) {
        truthy(message).then_some(message.clone())
    } else {
        let description = [reason, result]
            .into_iter()
            .find(|value| truthy(value))
            .map_or_else(|| "unknown".into(), parse::string);
        Some(json!(format!("Reset not applied: {description}")))
    };
    (
        status,
        json!({
            "ok": accepted,
            "status": upstream.as_u16(),
            "result": truthy(result).then_some(result),
            "reason": truthy(reason).then_some(reason),
            "resetsLeft": provider["resets_left"],
            "message": message,
        }),
    )
}

fn valid_id(id: &str, limit: usize) -> bool {
    !id.is_empty()
        && id.len() <= limit
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

async fn bounded_json(mut response: reqwest::Response) -> Result<Value, ()> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if chunk.len() > REPLY_LIMIT - body.len() {
            return Err(());
        }
        body.extend_from_slice(&chunk);
    }
    if body.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&body).map_err(|_| ())
}

fn internal(message: impl Into<String>) -> Failure {
    Failure::new(StatusCode::INTERNAL_SERVER_ERROR, message)
}

fn redact(value: &mut Value, accounts: &[&Value]) {
    let mut secrets = accounts
        .iter()
        .flat_map(|account| {
            ["accessToken", "refreshToken"]
                .into_iter()
                .filter_map(|field| account[field].as_str().filter(|value| !value.is_empty()))
        })
        .collect::<Vec<_>>();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    fn visit(value: &mut Value, secrets: &[&str]) {
        match value {
            Value::String(text) => {
                for secret in secrets {
                    if text.contains(secret) {
                        *text = text.replace(secret, "[redacted]");
                    }
                }
            }
            Value::Array(entries) => entries.iter_mut().for_each(|entry| visit(entry, secrets)),
            Value::Object(fields) => {
                for (mut name, mut content) in std::mem::take(fields) {
                    for secret in secrets {
                        if name.contains(secret) {
                            name = name.replace(secret, "[redacted]");
                        }
                    }
                    visit(&mut content, secrets);
                    fields.insert(name, content);
                }
            }
            _ => (),
        }
    }
    visit(value, &secrets);
}
