use std::time::Duration;

use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};

use crate::provider_http::{self, Bound, http_client};
use crate::provider_oauth::{
    RefreshMode, nous_inference_token, nous_refresh_due, nous_refresh_supported, refresh_account,
};
use crate::storage::Storage;

const BASE: &str = "https://inference-api.nousresearch.com/v1";
const LIMIT: usize = 16 * 1024 * 1024;

pub(crate) async fn models(
    storage: &Storage,
    token: &str,
) -> Result<Vec<Value>, Option<StatusCode>> {
    let mut response = route(storage, &Value::Null, "/models", 15)
        .await
        .ok_or(None)?
        .request(Method::GET)
        .header("Content-Type", "application/json")
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| None)?;
    if !response.status().is_success() {
        return Err(Some(response.status()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| None)? {
        if chunk.len() > LIMIT - bytes.len() {
            return Err(None);
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| None)?;
    let mut models = crate::providers::normalize_nous_models(&value);
    for model in &mut models {
        let id = model["id"].as_str().expect("normalized model ID");
        let routed = format!("nous/{id}");
        model["upstreamModelId"] = json!(id);
        model["id"] = json!(routed);
    }
    Ok(models)
}

pub(crate) async fn probe(storage: &Storage, account: &Value, token: &str) -> Option<String> {
    let Some(route) = route(storage, account, "/chat/completions", 8).await else {
        return Some("fetch failed".into());
    };
    match probe_status(&route, token).await {
        Ok(status) if status.is_success() => None,
        Ok(StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => {
            Some("Invalid or inactive API key".into())
        }
        Ok(status) => Some(format!(
            "Unable to verify API key (Nous returned {})",
            status.as_u16()
        )),
        Err(error) => Some(error.into()),
    }
}

pub(crate) struct OAuthTest {
    pub connection: Value,
    pub error: Option<String>,
    pub refreshed: bool,
}

pub(crate) async fn test_oauth(storage: &Storage, connection: Value) -> OAuthTest {
    let mut test = OAuthTest {
        connection,
        error: None,
        refreshed: false,
    };
    if test.connection["accessToken"]
        .as_str()
        .is_none_or(str::is_empty)
    {
        test.error = Some("No access token".into());
        return test;
    }
    let Some(route) = route(storage, &test.connection, "/chat/completions", 15).await else {
        test.error = Some("fetch failed".into());
        return test;
    };
    let mode = RefreshMode::Routed(route.protected);
    let refreshable = nous_refresh_supported(&test.connection);
    let proactive = refreshable && nous_refresh_due(&test.connection);
    if proactive {
        match refresh_account(storage, &test.connection, mode).await {
            Ok(connection) if nous_inference_token(&connection).is_some() => {
                test.refreshed = rotated(&test.connection, &connection);
                test.connection = connection;
            }
            _ => {
                test.error = Some("Token expired and refresh failed".into());
                return test;
            }
        }
    }
    let Some(token) = nous_inference_token(&test.connection) else {
        test.error = Some("Token invalid or revoked".into());
        return test;
    };
    let status = probe_status(&route, token).await;
    if status == Ok(StatusCode::UNAUTHORIZED)
        && refreshable
        && !proactive
        && let Ok(connection) = refresh_account(storage, &test.connection, mode).await
    {
        let refreshed = rotated(&test.connection, &connection);
        test.connection = connection;
        if let Some(token) = nous_inference_token(&test.connection) {
            match probe_status(&route, token).await {
                Ok(status) if status.is_success() => {
                    test.refreshed = refreshed;
                    return test;
                }
                Err(error) => {
                    test.error = Some(error.into());
                    return test;
                }
                _ => (),
            }
        }
    }
    test.error = match status {
        Ok(status) if status.is_success() => None,
        Ok(StatusCode::UNAUTHORIZED) => Some("Token invalid or revoked".into()),
        Ok(StatusCode::FORBIDDEN) => Some("Access denied".into()),
        Ok(status) => Some(format!("API returned {}", status.as_u16())),
        Err(error) => Some(error.into()),
    };
    test
}

fn rotated(first: &Value, second: &Value) -> bool {
    ["accessToken", "refreshToken", "apiKey", "lastRefreshAt"]
        .into_iter()
        .any(|field| first[field] != second[field])
}

async fn probe_status(route: &Bound, token: &str) -> Result<StatusCode, &'static str> {
    let response = route
        .request(Method::POST)
        .bearer_auth(token)
        .json(&json!({
            "model":"poolside/laguna-xs-2.1:free", "messages":[{"role":"user","content":"ping"}],
            "max_tokens":1, "stream":false,
        }))
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                "The operation was aborted due to timeout"
            } else {
                "fetch failed"
            }
        })?;
    if response.status() == StatusCode::PROXY_AUTHENTICATION_REQUIRED {
        return Err("Proxy authentication required");
    }
    Ok(response.status())
}

async fn route(storage: &Storage, account: &Value, path: &str, seconds: u64) -> Option<Bound> {
    let timeout = Duration::from_secs(seconds);
    let client = http_client(timeout).build().ok()?;
    provider_http::bind(
        storage,
        account,
        &format!("{BASE}{path}"),
        &format!("http://127.0.0.1:52989/nous-management{path}"),
        &client,
        timeout,
        None,
    )
    .await
}
