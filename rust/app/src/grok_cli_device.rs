use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use percent_encoding::percent_decode_str;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::{Value, json};
use url::Url;

use super::device_attempts::{Attempts, Lease, Waiting};
use super::flow::{Credentials, Provider, mapped_code_tokens, nonempty, quoted};
use super::{reply, store, token_claims};
use crate::dashboard_session::authorization_owner;
use crate::provider_http::{self, http_client};
use crate::provider_tools::scrub_payload;
use crate::storage::Storage;

const USER: &str = "https://cli-chat-proxy.grok.com/v1/user";
const FIXTURE: &str = "http://127.0.0.1:52989/grok-cli-device";
const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BODY: usize = 128 * 1024;

#[derive(Clone)]
struct Device {
    storage: Storage,
    client: Client,
    config: std::sync::Arc<Config>,
    pending: Attempts,
}

struct Config {
    register: String,
    token: String,
    client_id: String,
    scope: String,
    referrer: String,
    version: String,
    user_agent: String,
}

impl Config {
    fn pinned() -> Option<Self> {
        let oauth = include_str!("../../../open-sse/providers/registry/grok-cli.js")
            .split_once("\n  oauth: {")?
            .1;
        let version = include_str!("../../../open-sse/config/grokCli.js")
            .lines()
            .find_map(|line| line.strip_prefix("export const GROK_CLI_VERSION = \""))?
            .split_once('"')?
            .0
            .to_owned();
        let user_agent = format!("grok-pager/{version} grok-shell/{version} (linux; x86_64)");
        Some(Self {
            register: quoted(oauth, "deviceCodeUrl")?,
            token: quoted(oauth, "tokenUrl")?,
            client_id: quoted(oauth, "clientId")?,
            scope: oauth
                .split_once("\n    scope:")?
                .1
                .trim_start()
                .strip_prefix('"')?
                .split_once('"')?
                .0
                .to_owned(),
            referrer: quoted(oauth, "referrer")?,
            version,
            user_agent,
        })
    }
}

pub(super) fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/oauth/grok-cli/device-code", get(begin))
        .route("/api/oauth/grok-cli/poll", post(poll))
        .with_state(Device {
            storage,
            client: http_client(TIMEOUT)
                .build()
                .expect("Grok CLI device HTTP client"),
            config: std::sync::Arc::new(
                Config::pinned().expect("pinned Grok CLI device configuration"),
            ),
            pending: Attempts::default(),
        })
}

async fn begin(State(state): State<Device>, headers: HeaderMap) -> Response {
    let Some(owner) = authorization_owner(&state.storage, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    match state.begin(owner).await {
        Some(payload) => reply(StatusCode::OK, payload),
        None => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Grok CLI device code request failed",
        ),
    }
}

async fn poll(State(state): State<Device>, request: Request) -> Response {
    let Some(owner) = authorization_owner(&state.storage, request.headers()).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let Some(body) = to_bytes(request.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
    else {
        return error(StatusCode::BAD_REQUEST, "Invalid or empty request body");
    };
    let Some(handle) = nonempty(&body["deviceCode"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing device code");
    };
    let Some(proof) = nonempty(&body["codeVerifier"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing code verifier");
    };
    let (device_code, lease) = match state.pending.claim(owner, handle, proof) {
        Ok(claim) => claim,
        Err(code) => return reply(StatusCode::OK, pending(code)),
    };
    let outcome = tokio::spawn(async move { state.exchange(&device_code, lease).await })
        .await
        .unwrap_or_else(|_| failed("Could not complete Grok CLI authorization"));
    reply(StatusCode::OK, outcome)
}

impl Device {
    async fn begin(&self, owner: [u8; 32]) -> Option<Value> {
        let request = self
            .request(Method::POST, &self.config.register, "code")
            .await?
            .form(&[
                ("client_id", self.config.client_id.as_str()),
                ("scope", self.config.scope.as_str()),
                ("referrer", self.config.referrer.as_str()),
            ]);
        let (status, data) = response(request).await?;
        if !status.is_success() {
            return None;
        }
        let code = nonempty(&data["device_code"])?;
        let user = nonempty(&data["user_code"]).filter(|user| !user.contains(code))?;
        let uri = verification(&data["verification_uri"], code)?;
        let complete = verification(&data["verification_uri_complete"], code);
        let expires = seconds(&data["expires_in"]).unwrap_or(900).min(3600);
        let interval = seconds(&data["interval"]).unwrap_or(5);
        let (handle, proof) = self.pending.issue(
            owner,
            code,
            Duration::from_secs(expires),
            Duration::from_secs(interval),
        )?;
        let mut payload = json!({
            "device_code":handle,"user_code":user,"verification_uri":uri,
            "expires_in":expires,"interval":interval,"codeVerifier":proof,
        });
        if let Some(complete) = complete {
            payload["verification_uri_complete"] = json!(complete);
        }
        Some(payload)
    }

    async fn exchange(&self, code: &str, mut lease: Lease) -> Value {
        let Some(request) = self
            .request(Method::POST, &self.config.token, "token")
            .await
        else {
            return failed("Grok CLI authorization proxy is unavailable");
        };
        let Some((status, data)) = response(request.form(&[
            ("grant_type", GRANT),
            ("device_code", code),
            ("client_id", self.config.client_id.as_str()),
        ]))
        .await
        else {
            return failed("Could not poll for a Grok CLI device token");
        };
        if status.is_success() && data["error"].is_null() {
            if let Some(access) = nonempty(&data["access_token"])
                .filter(|value| value.bytes().all(|byte| byte.is_ascii_graphic()))
            {
                return self.save(&data, access).await;
            }
            return json!({"success":false,"error":"no_access_token","errorDescription":"No access token received","pending":false});
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
            _ if status == StatusCode::TOO_MANY_REQUESTS => "slow_down",
            _ => "device_authorization_failed",
        };
        match code {
            "authorization_pending" => lease.wait(Waiting::Pending),
            "slow_down" => lease.wait(Waiting::SlowDown),
            _ => (),
        }
        pending(code)
    }

    async fn save(&self, data: &Value, access: &str) -> Value {
        let mut profile = if let Some(request) = self.request(Method::GET, USER, "user").await {
            response(
                request
                    .bearer_auth(access)
                    .header("x-xai-token-auth", "xai-grok-cli")
                    .header("x-grok-client-version", &self.config.version),
            )
            .await
            .filter(|(status, _)| status.is_success())
            .map_or(Value::Null, |(_, profile)| profile)
        } else {
            Value::Null
        };
        let private = ["access_token", "refresh_token", "id_token"]
            .into_iter()
            .filter_map(|key| nonempty(&data[key]).map(str::to_owned))
            .collect::<Vec<_>>();
        scrub_payload(&mut profile, &private);
        let email = nonempty(&data["id_token"])
            .and_then(email_claim)
            .or_else(|| email_claim(access))
            .or_else(|| nonempty(&profile["email"]).map(str::to_owned))
            .filter(|email| !private.iter().any(|secret| email.contains(secret)));
        let user = nonempty(&profile["userId"]).or_else(|| nonempty(&profile["principalId"]));
        let display = ["firstName", "lastName"]
            .into_iter()
            .filter_map(|key| nonempty(&profile[key]))
            .collect::<Vec<_>>()
            .join(" ");
        let mut fields = mapped_code_tokens(data);
        fields.entry("refreshToken").or_insert(Value::Null);
        if let Some(value) = &email {
            fields.insert("email".into(), json!(value));
        }
        if !display.is_empty() {
            fields.insert("displayName".into(), json!(display));
        }
        let mut details = json!({
            "authMethod":"device_code","idToken":nonempty(&data["id_token"]),"email":email,"userId":user,
            "hasGrokCodeAccess":profile["hasGrokCodeAccess"],"subscriptionTier":profile["subscriptionTier"],
        });
        if let Some(details) = details.as_object_mut() {
            details.retain(|_, value| !value.is_null());
        }
        fields.insert("providerSpecificData".into(), details);
        match store::save(&self.storage, Provider::GrokCli, Credentials(fields)).await {
            Ok(saved) => {
                json!({"success":true,"connection":{"id":saved["id"],"provider":"grok-cli"}})
            }
            Err(_) => failed("Could not save the Grok CLI connection"),
        }
    }

    async fn request(&self, method: Method, endpoint: &str, path: &str) -> Option<RequestBuilder> {
        let local = format!("{FIXTURE}/{path}");
        let binding = provider_http::bind(
            &self.storage,
            &Value::Null,
            endpoint,
            &local,
            &self.client,
            TIMEOUT,
            None,
        )
        .await?;
        Some(
            binding
                .request(method)
                .header("Accept", "application/json")
                .header("User-Agent", &self.config.user_agent),
        )
    }
}

async fn response(request: RequestBuilder) -> Option<(StatusCode, Value)> {
    let mut response = request.send().await.ok()?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MAX_BODY - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let data = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"error":"invalid_response"}));
    Some((status, data))
}

fn verification(value: &Value, code: &str) -> Option<String> {
    let url = Url::parse(nonempty(value)?).ok()?;
    let host = url.host_str()?;
    let trusted = ["x.ai", "grok.com", "x.com"].iter().any(|domain| {
        host == *domain
            || host
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.'))
    });
    let valid = trusted
        && url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && !percent_decode_str(url.as_str())
            .decode_utf8_lossy()
            .contains(code);
    valid.then(|| url.to_string())
}

fn seconds(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|seconds| *seconds > 0)
}

fn email_claim(token: &str) -> Option<String> {
    let claims = token_claims(token);
    ["email", "preferred_username", "sub"]
        .into_iter()
        .find_map(|key| nonempty(&claims[key]).map(str::to_owned))
}

fn pending(code: &str) -> Value {
    json!({"success":false,"error":code,"pending":matches!(code,"authorization_pending"|"slow_down")})
}

fn failed(description: &str) -> Value {
    json!({"success":false,"error":"exchange_failed","errorDescription":description,"pending":false,"fatal":true})
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}))
}
