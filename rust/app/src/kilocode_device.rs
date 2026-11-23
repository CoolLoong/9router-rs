use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Method, RequestBuilder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use super::{body, error, metadata_bytes, metadata_client, nonempty, reply};
use crate::dashboard_session;
use crate::provider_connections::object;
use crate::provider_oauth::save_imported;
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

const PREFIX: &str = "https://api.kilo.ai";
pub(crate) const CHAT_ENDPOINT: &str = "https://api.kilo.ai/api/openrouter/chat/completions";

pub(crate) fn chat_headers(
    request: reqwest::RequestBuilder,
    token: &str,
    account: &Value,
) -> reqwest::RequestBuilder {
    let request = request.bearer_auth(token);
    let organization = &account["providerSpecificData"]["orgId"];
    if crate::provider_connections::truthy(organization) {
        request.header(
            "X-Kilocode-OrganizationID",
            crate::chat_output::js_text(organization),
        )
    } else {
        request
    }
}
const INTERVAL: Duration = Duration::from_secs(3);
static DEVICE: LazyLock<Mutex<HashMap<[u8; 32], Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct Pending {
    owner: [u8; 32],
    code: String,
    expires: Instant,
    due: Instant,
    busy: bool,
}

pub(super) async fn authorize(State(storage): State<Storage>, request: Request) -> Response {
    let Some(owner) = dashboard_session::authorization_owner(&storage, request.headers()).await
    else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let target = format!("{PREFIX}/api/device-auth/codes");
    let Some(init) = build(&storage, &target, Method::POST).await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Device auth initiation failed",
        );
    };
    let Ok(mut result) = init.header("content-type", "application/json").send().await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Device auth initiation failed",
        );
    };
    if result.status() == StatusCode::TOO_MANY_REQUESTS {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Too many pending authorization requests. Please try again later.",
        );
    }
    if !result.status().is_success() {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Device auth initiation failed",
        );
    }
    let payload = decode(&mut result).await.unwrap_or_default();
    let Some((code, verification)) =
        nonempty(&payload["code"]).zip(nonempty(&payload["verificationUrl"]).filter(|url| {
            Url::parse(url).is_ok_and(|url| {
                url.scheme() == "https"
                    && url.port().is_none()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.host_str().is_some_and(|host| {
                        matches!(host, "kilo.ai" | "kilocode.ai")
                            || host.ends_with(".kilo.ai")
                            || host.ends_with(".kilocode.ai")
                    })
            })
        }))
    else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Invalid Kilo Code device authorization response",
        );
    };
    let expires = payload["expiresIn"]
        .as_u64()
        .filter(|seconds| *seconds > 0)
        .unwrap_or(300)
        .min(3600);
    let mut raw = [0u8; 32];
    if getrandom::fill(&mut raw).is_err() {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not create device authorization",
        );
    }
    let handle = URL_SAFE_NO_PAD.encode(raw);
    let Ok(mut pending) = DEVICE.lock() else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not record device authorization",
        );
    };
    let now = Instant::now();
    pending.retain(|_, entry| entry.busy || entry.expires > now);
    if pending.len() >= 512 {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many pending authorization requests. Please try again later.",
        );
    }
    pending.insert(
        digest(&handle),
        Pending {
            owner,
            code: code.to_owned(),
            expires: now + Duration::from_secs(expires),
            due: now,
            busy: false,
        },
    );
    reply(
        StatusCode::OK,
        json!({
            "device_code":handle,"user_code":code,"verification_uri":verification,
            "verification_uri_complete":verification,"expires_in":expires,"interval":3,
        }),
        true,
    )
}

pub(super) async fn poll(State(storage): State<Storage>, request: Request) -> Response {
    let Some(owner) = dashboard_session::authorization_owner(&storage, request.headers()).await
    else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let input = match body(&storage, request).await {
        Ok(input) => input,
        Err((status, message)) => return error(status, message),
    };
    let Some(handle) = nonempty(&input["deviceCode"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing device code");
    };
    let key = digest(handle);
    let code = match claim(key, owner) {
        Ok(code) => code,
        Err(pending) => return reply(StatusCode::OK, pending, true),
    };
    let outcome = tokio::spawn(async move { exchange(&storage, key, &code).await })
        .await
        .unwrap_or_else(|_| {
            rejected(
                "poll_failed",
                Some("Could not complete Kilo Code sign-in"),
                false,
            )
        });
    reply(StatusCode::OK, outcome, true)
}

fn claim(key: [u8; 32], owner: [u8; 32]) -> Result<String, Value> {
    let Ok(mut pending) = DEVICE.lock() else {
        return Err(rejected("poll_failed", None, false));
    };
    let now = Instant::now();
    pending.retain(|_, entry| entry.busy || entry.expires > now);
    let Some(entry) = pending
        .get_mut(&key)
        .filter(|entry| entry.owner == owner && entry.expires > now)
    else {
        return Err(rejected(
            "expired_token",
            Some("Authorization code expired"),
            false,
        ));
    };
    if entry.busy || now < entry.due {
        return Err(rejected("slow_down", None, true));
    }
    entry.busy = true;
    Ok(entry.code.clone())
}

async fn exchange(storage: &Storage, key: [u8; 32], code: &str) -> Value {
    let mut target =
        Url::parse(&format!("{PREFIX}/api/device-auth/codes")).expect("fixed Kilo device endpoint");
    target.path_segments_mut().expect("device path").push(code);
    let result = if let Some(request) = build(storage, target.as_str(), Method::GET).await {
        request.send().await.ok()
    } else {
        None
    };
    let Some(mut result) = result else {
        return transition(
            key,
            rejected(
                "poll_failed",
                Some("Could not fetch Kilo Code authorization status"),
                false,
            ),
            false,
        );
    };
    match result.status() {
        StatusCode::ACCEPTED => {
            return transition(key, rejected("authorization_pending", None, true), true);
        }
        StatusCode::FORBIDDEN => {
            return transition(
                key,
                rejected("access_denied", Some("Authorization denied by user"), false),
                false,
            );
        }
        StatusCode::GONE => {
            return transition(
                key,
                rejected("expired_token", Some("Authorization code expired"), false),
                false,
            );
        }
        code if !code.is_success() => {
            return transition(
                key,
                rejected(
                    "poll_failed",
                    Some(&format!("Poll failed: {}", code.as_u16())),
                    false,
                ),
                false,
            );
        }
        _ => (),
    }
    let Some(result) = decode(&mut result).await else {
        return transition(
            key,
            rejected("poll_failed", Some("Invalid authorization response"), false),
            false,
        );
    };
    let Some(token) = nonempty(&result["token"]).filter(|_| result["status"] == "approved") else {
        return transition(key, rejected("authorization_pending", None, true), true);
    };
    transition(key, Value::Null, false);
    let mut fields = object(json!({"accessToken":token,"refreshToken":null,"expiresAt":null}));
    if let Some(email) = result.get("userEmail") {
        fields.insert("email".into(), email.clone());
    }
    if let Some(request) = build(storage, &format!("{PREFIX}/api/profile"), Method::GET).await
        && let Ok(mut profile) = request.bearer_auth(token).send().await
        && profile.status().is_success()
        && let Some(profile) = decode(&mut profile).await
        && let Some(org) = profile["organizations"]
            .get(0)
            .and_then(|row| row.get("id"))
            .filter(|id| crate::provider_connections::truthy(id))
    {
        fields.insert("providerSpecificData".into(), json!({"orgId":org}));
    }
    match save_imported(storage, "kilocode", fields).await {
        Ok(saved) => json!({"success":true,"connection":{"id":saved["id"],"provider":"kilocode"}}),
        Err(_) => rejected(
            "poll_failed",
            Some("Could not save the provider connection"),
            false,
        ),
    }
}

fn transition(key: [u8; 32], response: Value, retry: bool) -> Value {
    if let Ok(mut pending) = DEVICE.lock() {
        if retry {
            if let Some(entry) = pending.get_mut(&key) {
                entry.busy = false;
                entry.due = Instant::now() + INTERVAL;
            }
        } else {
            pending.remove(&key);
        }
    }
    response
}

async fn build(storage: &Storage, target: &str, method: Method) -> Option<RequestBuilder> {
    let settings = storage.read_settings().await?;
    let route = proxy_transport::select(storage, &settings, &Value::Null, target)
        .await
        .ok()?;
    let client = metadata_client(&route, Duration::from_secs(15))?;
    let fixture;
    let address = match &route {
        Route::Relay { url, .. } => url,
        Route::Direct if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") => {
            fixture = format!(
                "http://127.0.0.1:52989/kilocode-device{}",
                target.strip_prefix(PREFIX)?
            );
            &fixture
        }
        _ => target,
    };
    Some(route.relay(client.request(method, address)))
}

async fn decode(result: &mut reqwest::Response) -> Option<Value> {
    serde_json::from_slice(&metadata_bytes(result).await?).ok()
}

fn rejected(code: &str, message: Option<&str>, pending: bool) -> Value {
    let mut body = json!({"success":false,"error":code});
    if let Some(message) = message {
        body["errorDescription"] = json!(message);
    }
    body["pending"] = json!(pending);
    body
}

fn digest(handle: &str) -> [u8; 32] {
    Sha256::digest(handle.as_bytes()).into()
}
