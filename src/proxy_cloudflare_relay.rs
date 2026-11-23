use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::RequestBuilder;
use serde_json::{Value, json};

use super::managed_relay::{
    Platform, api_client, api_error, dns_label, fixture, request_json, save, token,
};
use super::{StateData, error};

const API: &str = "https://api.cloudflare.com/client/v4";
const FIXTURE_API: &str = "http://127.0.0.1:52989/cloudflare-management/client/v4";
const WORKER: &str = include_str!("../assets/relay/cloudflare_relay_worker.mjs");

fn identifier(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

async fn response(request: RequestBuilder) -> Result<Value, StatusCode> {
    let value = request_json(request).await?;
    if value["success"] != true || !value["result"].is_object() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    Ok(value["result"].clone())
}

pub(super) async fn deploy(State(state): State<StateData>, request: Request) -> Response {
    let Ok(bytes) = to_bytes(request.into_body(), 16 * 1024).await else {
        return error(
            StatusCode::BAD_REQUEST,
            "Invalid Cloudflare deployment request",
        );
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return error(
            StatusCode::BAD_REQUEST,
            "Invalid Cloudflare deployment request",
        );
    };
    let (Some(account), Some(api_token)) = (
        body["accountId"]
            .as_str()
            .map(str::trim)
            .filter(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())),
        body["apiToken"].as_str().map(str::trim).filter(|token| {
            !token.is_empty()
                && token.len() <= 512
                && token.bytes().all(|byte| byte.is_ascii_graphic())
        }),
    ) else {
        return error(
            StatusCode::BAD_REQUEST,
            "Cloudflare Account ID and API Token are required",
        );
    };
    let name = body["projectName"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("relay-{}", uuid::Uuid::new_v4().simple()));
    if !dns_label(&name) {
        return error(StatusCode::BAD_REQUEST, "Invalid Cloudflare Worker name");
    }
    let Some(secret) = token() else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not generate the relay token",
        );
    };
    let Some(client) = api_client() else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not initialize the Cloudflare API client",
        );
    };
    let base = if fixture() { FIXTURE_API } else { API };
    let workers = format!("{base}/accounts/{account}/workers");
    let account_result = match response(
        client
            .get(format!("{workers}/subdomain"))
            .bearer_auth(api_token),
    )
    .await
    {
        Ok(result) => result,
        Err(StatusCode::NOT_FOUND) => {
            return error(
                StatusCode::BAD_REQUEST,
                "Set up the account's workers.dev subdomain before deploying",
            );
        }
        Err(status) => {
            return api_error(
                status,
                "Could not read the Cloudflare workers.dev subdomain",
            );
        }
    };
    let Some(subdomain) = account_result["subdomain"]
        .as_str()
        .filter(|label| dns_label(label))
    else {
        return error(
            StatusCode::BAD_GATEWAY,
            "Cloudflare did not return a valid workers.dev subdomain",
        );
    };
    let deploy_url = format!("https://{name}.{subdomain}.workers.dev");

    let created = match response(
        client
            .post(format!("{workers}/workers"))
            .bearer_auth(api_token)
            .json(&json!({
                "name":name,"observability":{"enabled":false},"logpush":false,
                "subdomain":{"enabled":false,"previews_enabled":false},
            })),
    )
    .await
    {
        Ok(result) => result,
        Err(StatusCode::CONFLICT) => {
            return error(
                StatusCode::CONFLICT,
                "A Worker with this name already exists",
            );
        }
        Err(status) => return api_error(status, "Could not create the Cloudflare Worker"),
    };
    let Some(worker_id) = identifier(&created["id"]).filter(|_| created["name"] == name) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "Cloudflare did not confirm the new Worker identity",
        );
    };
    let worker = format!("{workers}/workers/{worker_id}");
    let version = response(
        client.post(format!("{worker}/versions?deploy=true")).bearer_auth(api_token)
            .json(&json!({
                "main_module":"index.mjs",
                "compatibility_date":"2024-03-20",
                "compatibility_flags":["global_fetch_strictly_public","cache_option_enabled"],
                "bindings":[
                    {"name":"RELAY_AUTH","type":"secret_text","text":secret},
                    {"name":"RELAY_URL","type":"plain_text","text":deploy_url},
                ],
                "modules":[{"name":"index.mjs","content_type":"application/javascript+module","content_base64":STANDARD.encode(WORKER)}],
            })),
    ).await;
    match version {
        Ok(result)
            if result["id"]
                .as_str()
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .is_some() => {}
        Ok(_) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "Worker created but Cloudflare did not confirm the deployment",
            );
        }
        Err(status) => {
            return api_error(
                status,
                "Worker created but Cloudflare could not deploy the relay",
            );
        }
    }
    let enabled = match response(
        client
            .patch(worker)
            .bearer_auth(api_token)
            .json(&json!({"subdomain":{"enabled":true,"previews_enabled":false}})),
    )
    .await
    {
        Ok(result) => result,
        Err(status) => {
            return api_error(
                status,
                "Worker deployed but Cloudflare could not enable its URL",
            );
        }
    };
    let confirmed_url = enabled["subdomain"]["url"]
        .as_str()
        .and_then(|url| Platform::Cloudflare.url(url));
    if enabled["id"] != worker_id
        || enabled["name"] != name
        || enabled["subdomain"]["enabled"] != true
        || enabled["subdomain"]["previews_enabled"] != false
        || confirmed_url != Platform::Cloudflare.url(&deploy_url)
    {
        return error(
            StatusCode::BAD_GATEWAY,
            "Worker deployed but Cloudflare did not confirm its exact URL",
        );
    }
    save(state, Platform::Cloudflare, name, deploy_url, secret).await
}
