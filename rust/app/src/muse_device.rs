use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use percent_encoding::percent_decode_str;
use reqwest::{Client, ClientBuilder};
use serde_json::{Value, json};
use url::Url;

use crate::catalog_guard::valid_cli_token;
use crate::dashboard_session::{authorization_owner, has_session};
use crate::provider_connections::{object, truthy};
use crate::provider_index::js_trim;
use crate::provider_oauth::device_attempts::{Attempts, Lease, Waiting};
use crate::provider_oauth::save_imported;
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

const REGISTER: &str = "https://auth.meta.com/oidc/device/authorization/";
const TOKEN: &str = "https://auth.meta.com/oidc/device/token/";
const MINT: &str = "https://api.meta.ai/muse-code/key";
const CLIENT_ID: &str = "1031625952748946";
const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const FIXTURE: &str = "http://127.0.0.1:52989/muse-device";
const RESPONSE_LIMIT: usize = 128 * 1024;

type Key = [u8; 32];

#[derive(Clone)]
struct Device {
    storage: Storage,
    client: Client,
    pending: Attempts,
    fixture: bool,
}

enum Endpoint<'a> {
    Register,
    Poll(&'a str),
    Mint(&'a str),
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/oauth/muse/device-code", get(begin))
        .route("/api/oauth/muse/poll", post(poll))
        .with_state(Device {
            storage,
            client: client_builder()
                .build()
                .expect("Muse Code OAuth HTTP client"),
            pending: Attempts::default(),
            fixture: std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"),
        })
}

async fn begin(State(state): State<Device>, headers: HeaderMap) -> Response {
    let Some(owner) = owner(&state.storage, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    match state.begin(owner).await {
        Some(payload) => reply(StatusCode::OK, payload),
        None => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Muse Code device code request failed",
        ),
    }
}

async fn poll(State(state): State<Device>, request: Request) -> Response {
    let Some(owner) = owner(&state.storage, request.headers()).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let Some(body) = to_bytes(request.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .filter(Value::is_object)
    else {
        return error(StatusCode::BAD_REQUEST, "Invalid or empty request body");
    };
    let Some(handle) = text(&body["deviceCode"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing device code");
    };
    let Some(proof) = text(&body["codeVerifier"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing code verifier");
    };
    let (device_code, lease) = match state.pending.claim(owner, handle, proof) {
        Ok(claimed) => claimed,
        Err(code) => return reply(StatusCode::OK, poll_error(code)),
    };
    let result = tokio::spawn(async move { state.poll(&device_code, lease).await })
        .await
        .unwrap_or_else(|_| fatal("Could not complete Muse Code authorization", None));
    reply(StatusCode::OK, result)
}

async fn owner(storage: &Storage, headers: &HeaderMap) -> Option<Key> {
    if !has_session(storage, headers) && !valid_cli_token(storage, headers) {
        return None;
    }
    authorization_owner(storage, headers).await
}

impl Device {
    async fn begin(&self, owner: Key) -> Option<Value> {
        let (status, data) = self.send(Endpoint::Register).await.ok()?;
        let data = data.filter(|_| status.is_success())?;
        let device_code = text(&data["device_code"])?;
        let user_code = text(&data["user_code"]).filter(|value| !value.contains(device_code))?;
        let verify = verification(&data["verification_uri"], device_code)?;
        let complete = verification(&data["verification_uri_complete"], device_code);
        let seconds = data["expires_in"].as_u64().filter(|seconds| *seconds > 0)?;
        let interval = data["interval"].as_u64().filter(|seconds| *seconds > 0);
        let (handle, proof) = self.pending.issue(
            owner,
            device_code,
            Duration::from_secs(seconds),
            Duration::from_secs(interval.unwrap_or(5)),
        )?;
        let mut payload = json!({
            "device_code":handle, "user_code":user_code, "verification_uri":verify.as_str(),
            "expires_in":seconds, "codeVerifier":proof,
        });
        if let Some(complete) = complete {
            payload["verification_uri_complete"] = json!(complete.as_str());
        }
        if let Some(interval) = interval {
            payload["interval"] = json!(interval);
        }
        Some(payload)
    }

    async fn poll(&self, device_code: &str, mut lease: Lease) -> Value {
        let Ok((status, data)) = self.send(Endpoint::Poll(device_code)).await else {
            return fatal("Could not poll for a Muse Code device token", None);
        };
        let Some(data) = data else {
            return poll_error("invalid_response");
        };
        if status.is_success()
            && data["error"].is_null()
            && let Some(token) = text(&data["access_token"])
        {
            return self.mint(token).await;
        }
        let code = match data["error"].as_str() {
            Some(
                code @ ("authorization_pending"
                | "slow_down"
                | "access_denied"
                | "expired_token"
                | "invalid_grant"
                | "invalid_client"
                | "invalid_request"
                | "invalid_scope"
                | "unsupported_grant_type"
                | "unauthorized_client"
                | "server_error"
                | "temporarily_unavailable"
                | "invalid_response"),
            ) => code,
            None if status.is_success() => "no_access_token",
            _ => "device_authorization_failed",
        };
        match code {
            "authorization_pending" => lease.wait(Waiting::Pending),
            "slow_down" => lease.wait(Waiting::SlowDown),
            _ => (),
        }
        let mut result = poll_error(code);
        if code == "no_access_token" {
            result["errorDescription"] = json!("No access token received");
        }
        result
    }

    async fn mint(&self, token: &str) -> Value {
        let Ok((status, payload)) = self.send(Endpoint::Mint(token)).await else {
            return fatal("Could not confirm Muse Code key minting", None);
        };
        let action = payload
            .as_ref()
            .and_then(|payload| action_url(payload, token));
        if !status.is_success() {
            return fatal(
                &format!("Muse Code key mint failed: HTTP {}", status.as_u16()),
                action,
            );
        }
        let Some(payload) = payload else {
            return fatal("Muse Code key mint returned invalid JSON", None);
        };
        if payload["is_subs_active"] == false {
            return fatal(
                "Muse Code subscription is inactive — activate it on muse.ai first",
                action,
            );
        }
        let Some(key) = text(&payload["api_key"]).filter(|key| *key != token) else {
            let description = if truthy(&payload["require_payment"])
                || truthy(&payload["action_url"])
                || truthy(&payload["require_payment_action_url"])
            {
                "Muse Code subscription required"
            } else {
                "Muse Code key response is missing api_key"
            };
            return fatal(description, action);
        };
        let safe = |field: &str| {
            text(&payload[field]).filter(|value| !value.contains(token) && !value.contains(key))
        };
        let email = safe("user_email")
            .map(js_trim)
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase);
        let subscription = safe("subs_tier_name");
        let fields = object(json!({
            "accessToken":key,"refreshToken":null,"email":email,
            "providerSpecificData":{"authMethod":"device_code","oauthAccessToken":token,"subscriptionTier":subscription},
        }));
        match save_imported(&self.storage, "muse", fields).await {
            Ok(saved) => json!({"success":true,"connection":{"id":saved["id"],"provider":"muse"}}),
            Err(_) => fatal("Could not save the provider connection", None),
        }
    }

    async fn send(&self, step: Endpoint<'_>) -> Result<(StatusCode, Option<Value>), ()> {
        let (target, path) = match step {
            Endpoint::Register => (REGISTER, "authorization"),
            Endpoint::Poll(_) => (TOKEN, "token"),
            Endpoint::Mint(_) => (MINT, "key"),
        };
        let (client, address) = if self.fixture {
            (self.client.clone(), format!("{FIXTURE}/{path}"))
        } else {
            let settings = self.storage.read_settings().await.ok_or(())?;
            let route =
                proxy_transport::select(&self.storage, &settings, &Value::Null, target).await?;
            let client = match route {
                Route::Direct => self.client.clone(),
                Route::Proxy { url, .. } => client_builder()
                    .proxy(reqwest::Proxy::all(url).map_err(|_| ())?)
                    .build()
                    .map_err(|_| ())?,
                Route::Relay { .. } | Route::MissingStrict => return Err(()),
            };
            (client, target.to_owned())
        };
        let request = client
            .post(address)
            .header(header::ACCEPT, "application/json")
            .header(header::USER_AGENT, "9Router")
            .header("x-api-version", "1.0.0");
        let request = match step {
            Endpoint::Register => request.form(&[("client_id", CLIENT_ID)]),
            Endpoint::Poll(code) => request.form(&[
                ("grant_type", GRANT),
                ("device_code", code),
                ("client_id", CLIENT_ID),
            ]),
            Endpoint::Mint(token) => request.bearer_auth(token).json(&json!({"onboard":true})),
        };
        let mut response = request.send().await.map_err(|_| ())?;
        let status = response.status();
        if status.is_redirection() {
            return Ok((status, None));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if chunk.len() > RESPONSE_LIMIT - bytes.len() {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((status, serde_json::from_slice(&bytes).ok()))
    }
}

fn trusted(value: &Value) -> Option<Url> {
    let url = Url::parse(text(value)?).ok()?;
    (url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "auth.meta.com"
                    | "meta.com"
                    | "www.meta.com"
                    | "accountscenter.meta.com"
                    | "meta.ai"
                    | "www.meta.ai"
                    | "api.meta.ai"
                    | "dev.meta.ai"
                    | "muse.ai"
                    | "www.muse.ai"
                    | "facebook.com"
                    | "www.facebook.com"
            )
        ))
    .then_some(url)
}

fn verification(value: &Value, code: &str) -> Option<Url> {
    let url = trusted(value)?;
    (!percent_decode_str(url.as_str())
        .decode_utf8_lossy()
        .contains(code))
    .then_some(url)
}

fn action_url(payload: &Value, token: &str) -> Option<Url> {
    ["action_url", "require_payment_action_url"]
        .into_iter()
        .find_map(|name| {
            let mut url = trusted(&payload[name])?;
            url.set_query(None);
            url.set_fragment(None);
            let decoded = percent_decode_str(url.as_str()).decode_utf8_lossy();
            (!decoded.contains(token)
                && text(&payload["api_key"]).is_none_or(|key| !decoded.contains(key)))
            .then_some(url)
        })
}

fn client_builder() -> ClientBuilder {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn poll_error(code: &str) -> Value {
    json!({"success":false,"error":code,"pending":matches!(code,"authorization_pending"|"slow_down")})
}

fn fatal(description: &str, action: Option<Url>) -> Value {
    let mut payload = json!({"success":false,"error":"exchange_failed","errorDescription":description,"pending":false,"fatal":true});
    if let Some(url) = action {
        payload["actionUrl"] = json!(url.as_str());
    }
    payload
}

fn error(status: StatusCode, text: &str) -> Response {
    reply(status, json!({"error":text}))
}

fn reply(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}
