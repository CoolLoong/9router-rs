use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::Duration;

use reqwest::{Client, RequestBuilder, Response, StatusCode, header};
use serde_json::{Value, json};

use crate::chat_input::PreparedChat;
use crate::provider_http::{self, Delivery};
use crate::provider_oauth::{
    RefreshMode, refresh_account, refresh_due_for_provider, test_token_url,
};
use crate::proxy_transport::{Clients, Route, Selection};
use crate::storage::Storage;

pub(crate) const ENDPOINT: &str = "https://api.anthropic.com/v1/messages?beta=true";
const FIXTURE: &str = "http://127.0.0.1:52989/anthropic/v1/messages?beta=true";
const TIMEOUT: Duration = Duration::from_secs(60);
static CLIENT: LazyLock<Option<Client>> =
    LazyLock::new(|| provider_http::http_client(TIMEOUT).build().ok());
static PROXIES: LazyLock<Clients> = LazyLock::new(Clients::default);

pub(crate) struct Failure {
    pub status: StatusCode,
    pub message: &'static str,
}

impl Failure {
    fn unavailable() -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: "Claude outbound connection is unavailable",
        }
    }

    fn credentials() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "Claude account credentials are unavailable or expired",
        }
    }
}

pub(crate) fn supports(account: &Value) -> bool {
    account["provider"] == "claude"
        && match account["authType"].as_str() {
            Some("oauth") => {
                token(account, "accessToken").is_some() || token(account, "refreshToken").is_some()
            }
            Some("apikey" | "api_key") => token(account, "apiKey").is_some(),
            _ => false,
        }
}

fn token<'a>(account: &'a Value, field: &str) -> Option<&'a str> {
    account[field]
        .as_str()
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic()))
}

fn beta(body: &Value, model: &str, incoming: &str, oauth: bool) -> String {
    let model_beta = crate::chat::anthropic_beta(model, body, incoming, true);
    match model_beta {
        Some(beta) if oauth => beta,
        Some(beta) => beta
            .split(',')
            .filter(|flag| *flag != "oauth-2025-04-20")
            .collect::<Vec<_>>()
            .join(","),
        None if oauth => "oauth-2025-04-20".to_owned(),
        None => incoming.to_owned(),
    }
}

pub(crate) fn preview(account: &Value, model: &str, prepared: PreparedChat) -> Option<Value> {
    if !supports(account) {
        return None;
    }
    let oauth = account["authType"] == "oauth";
    let mut headers = json!({
        "Content-Type":"application/json",
        "Anthropic-Version":"2023-06-01",
        "Anthropic-Beta":beta(&prepared.body,model,"",oauth),
    });
    headers[if oauth { "Authorization" } else { "x-api-key" }] = json!("[redacted]");
    if prepared.stream {
        headers["Accept"] = json!("text/event-stream");
    }
    Some(json!({"url":ENDPOINT,"headers":headers,"body":prepared.body}))
}

pub(crate) async fn complete(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    prepared: &PreparedChat,
    incoming: &str,
) -> Result<(Response, Vec<Vec<u8>>), Failure> {
    if !supports(account) {
        return Err(Failure::credentials());
    }
    let selection = provider_http::select(storage, settings, account, ENDPOINT)
        .await
        .map_err(|_| Failure::unavailable())?;
    if matches!(selection, Selection::Routed(Route::MissingStrict)) {
        return Err(Failure::unavailable());
    }
    let renewable = account["authType"] == "oauth" && token(account, "refreshToken").is_some();
    let mut current = Cow::Borrowed(account);
    let mut secrets = secrets(account);
    let mut renewed = false;
    if renewable
        && (token(account, "accessToken").is_none() || refresh_due_for_provider("claude", account))
    {
        let updated = renew(storage, &current, &selection).await?;
        secrets.extend(self::secrets(&updated));
        current = Cow::Owned(updated);
        renewed = true;
    }
    let mut response = send(&selection, &current, model, prepared, incoming).await?;
    if renewable
        && !renewed
        && matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        )
        && let Ok(updated) = renew(storage, &current, &selection).await
        && updated["accessToken"] != current["accessToken"]
    {
        secrets.extend(self::secrets(&updated));
        drop(response);
        response = send(&selection, &updated, model, prepared, incoming).await?;
    }
    Ok((response, secrets))
}

async fn renew(storage: &Storage, account: &Value, expected: &Selection) -> Result<Value, Failure> {
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(Failure::unavailable)?;
    let endpoint = test_token_url("claude").ok_or_else(Failure::unavailable)?;
    let delivery = Delivery::select(storage, &settings, account, endpoint)
        .await
        .map_err(|_| Failure::unavailable())?;
    let updated = refresh_account(storage, account, RefreshMode::Routed(delivery.protected()))
        .await
        .map_err(|_| Failure::credentials())?;
    if updated["isActive"] == false || token(&updated, "accessToken").is_none() {
        return Err(Failure::credentials());
    }
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(Failure::unavailable)?;
    let current = provider_http::select(storage, &settings, &updated, ENDPOINT)
        .await
        .map_err(|_| Failure::unavailable())?;
    if &current != expected {
        return Err(Failure::unavailable());
    }
    Ok(updated)
}

async fn send(
    selection: &Selection,
    account: &Value,
    model: &str,
    prepared: &PreparedChat,
    incoming: &str,
) -> Result<Response, Failure> {
    let client = CLIENT.as_ref().ok_or_else(Failure::unavailable)?;
    let destination = if provider_http::fixture()
        && matches!(
            selection,
            Selection::ExplicitBypass | Selection::Routed(Route::Direct)
        ) {
        FIXTURE
    } else {
        ENDPOINT
    };
    let request =
        headers(client.post(destination), account, model, prepared, incoming)?.json(&prepared.body);
    let (client, request) = request.build_split();
    provider_http::send(
        &PROXIES,
        client,
        request.map_err(|_| Failure::unavailable())?,
        selection.clone(),
        TIMEOUT,
    )
    .await
    .map_err(|_| Failure::unavailable())
}

fn headers(
    request: RequestBuilder,
    account: &Value,
    model: &str,
    prepared: &PreparedChat,
    incoming: &str,
) -> Result<RequestBuilder, Failure> {
    let oauth = account["authType"] == "oauth";
    let mut request = request.header("anthropic-version", "2023-06-01");
    let beta = beta(&prepared.body, model, incoming, oauth);
    if !beta.is_empty() {
        request = request.header("anthropic-beta", beta);
    }
    if prepared.stream {
        request = request.header(header::ACCEPT, "text/event-stream");
    }
    if oauth {
        Ok(request.bearer_auth(token(account, "accessToken").ok_or_else(Failure::credentials)?))
    } else {
        let mut key = header::HeaderValue::from_str(
            token(account, "apiKey").ok_or_else(Failure::credentials)?,
        )
        .map_err(|_| Failure::credentials())?;
        key.set_sensitive(true);
        Ok(request.header("x-api-key", key))
    }
}

fn secrets(account: &Value) -> Vec<Vec<u8>> {
    ["apiKey", "accessToken", "refreshToken", "idToken"]
        .into_iter()
        .filter_map(|field| token(account, field))
        .map(|secret| secret.as_bytes().to_vec())
        .collect()
}
