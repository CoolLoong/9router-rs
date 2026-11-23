use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use reqwest::{Method, Response as Upstream};
use serde_json::{Value, json};

use super::{body, error, metadata_bytes, metadata_client, nonempty, reply};
use crate::provider_connections::{object, truthy};
use crate::provider_index::js_trim;
use crate::provider_oauth::save_imported;
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

const ORIGIN: &str = "https://platform.iflow.cn";
const API: &str = "https://platform.iflow.cn/api/openapi/apikey";
const FAILED: &str = "iFlow cookie authentication failed";

pub(super) async fn import(State(storage): State<Storage>, request: Request) -> Response {
    let input = match body(&storage, request).await {
        Ok(input) => input,
        Err((status, message)) => return error(status, message),
    };
    let Some(cookie) = nonempty(&input["cookie"]).map(js_trim) else {
        return error(StatusCode::BAD_REQUEST, "Cookie is required");
    };
    let bx = cookie
        .split(';')
        .map(js_trim)
        .find_map(|token| token.strip_prefix("BXAuth="))
        .filter(|token| !token.is_empty());
    let Some(bx) = bx else {
        return error(StatusCode::BAD_REQUEST, "Cookie must contain BXAuth field");
    };
    let cookie_value = if cookie.ends_with(';') {
        cookie.to_owned()
    } else {
        format!("{cookie};")
    };
    let Ok(mut cookie_header) = HeaderValue::from_str(&cookie_value) else {
        return error(StatusCode::BAD_REQUEST, "Invalid Cookie header");
    };
    cookie_header.set_sensitive(true);
    let Some(settings) = storage.read_settings().await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, FAILED);
    };
    let Ok(route) = proxy_transport::select(&storage, &settings, &Value::Null, API).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, FAILED);
    };
    let Some(client) = metadata_client(&route, Duration::from_secs(15)) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, FAILED);
    };
    let url = match &route {
        Route::Relay { url, .. } => url,
        Route::Direct if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") => {
            "http://127.0.0.1:52989/iflow-fixture/api/openapi/apikey"
        }
        _ => API,
    };
    let request = |method| {
        client
            .request(method, url)
            .header("cookie", cookie_header.clone())
            .header("accept", "application/json, text/plain, */*")
            .header("user-agent", "9Router")
    };
    let read = route.relay(request(Method::GET)).send().await;
    let initial = match decode(
        read,
        "Failed to fetch API key info",
        "API key fetch failed",
        &cookie_value,
        bx,
    )
    .await
    {
        Ok(result) => result,
        Err((status, message)) => return error(status, message),
    };
    let Some(name) = nonempty(&initial["data"]["name"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing name in API key info");
    };
    let refresh = route
        .relay(
            request(Method::POST)
                .header("origin", ORIGIN)
                .header("referer", "https://platform.iflow.cn/")
                .json(&json!({"name":name})),
        )
        .send()
        .await;
    let result = match decode(
        refresh,
        "Failed to refresh API key",
        "API key refresh failed",
        &cookie_value,
        bx,
    )
    .await
    {
        Ok(result) => result,
        Err((status, message)) => return error(status, message),
    };
    let Some(key) = nonempty(&result["data"]["apiKey"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing API key in response");
    };
    let account_name = nonempty(&result["data"]["name"]).unwrap_or(name);
    let mut details = object(json!({"cookie":format!("BXAuth={bx};")}));
    let expiry = result["data"].get("expireTime");
    if let Some(expiry) = expiry {
        details.insert("expireTime".into(), expiry.clone());
    }
    let fields = object(json!({
        "authType":"cookie","name":account_name,"email":account_name,"apiKey":key,"providerSpecificData":details,
    }));
    let Ok(saved) = save_imported(&storage, "iflow", fields).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, FAILED);
    };
    let masked = format!("{}...", key.chars().take(10).collect::<String>());
    let mut connection =
        json!({"id":saved["id"],"provider":"iflow","email":saved["email"],"apiKey":masked});
    if let Some(expiry) = expiry {
        connection["expireTime"] = expiry.clone();
    }
    reply(
        StatusCode::OK,
        json!({"success":true,"connection":connection}),
        true,
    )
}

async fn decode(
    response: Result<Upstream, reqwest::Error>,
    http_error: &str,
    service_error: &str,
    cookie: &str,
    bx: &str,
) -> Result<Value, (StatusCode, String)> {
    let mut response =
        response.map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, FAILED.to_owned()))?;
    let status = response.status();
    let bytes = metadata_bytes(&mut response)
        .await
        .ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, FAILED.to_owned()))?;
    let redact = |value: &str| {
        value
            .replace(cookie, "[redacted]")
            .replace(bx, "[redacted]")
            .chars()
            .filter(|ch| !ch.is_control())
            .take(256)
            .collect::<String>()
    };
    if !status.is_success() {
        return Err((
            status,
            format!("{http_error}: {}", redact(&String::from_utf8_lossy(&bytes))),
        ));
    }
    let result: Value = serde_json::from_slice(&bytes)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, FAILED.to_owned()))?;
    if !truthy(&result["success"]) {
        let detail = result["message"].as_str().unwrap_or("undefined");
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{service_error}: {}", redact(detail)),
        ));
    }
    Ok(result)
}
