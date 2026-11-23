use std::collections::{HashMap, hash_map::Entry};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::dashboard_session::authorization_owner;
use crate::provider_oauth::{
    kimi_portal::{Endpoint, Portal, VERIFY, text, token_fields},
    save_imported,
};
use crate::storage::Storage;

const DEVICE_ERROR: &str = "Device code request failed";
const MAX_PENDING: usize = 512;

type Key = [u8; 32];
type Pool = Arc<Mutex<HashMap<Key, Attempt>>>;

#[derive(Clone)]
struct Device {
    storage: Storage,
    portal: Portal,
    pending: Pool,
}

struct Attempt {
    owner: Key,
    registration: Arc<Registration>,
    expires: Instant,
    next_poll: Instant,
    interval: Duration,
    polling: bool,
}

struct Registration {
    device_code: String,
    device_id: String,
}

enum Waiting {
    Pending,
    SlowDown,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/oauth/kimi/device-code", get(begin))
        .route("/api/oauth/kimi/poll", post(poll))
        .route("/api/oauth/kimi-coding/device-code", get(begin))
        .route("/api/oauth/kimi-coding/poll", post(poll))
        .with_state(Device {
            storage,
            portal: Portal::new().expect("Kimi OAuth HTTP client"),
            pending: Arc::default(),
        })
}

async fn begin(State(state): State<Device>, headers: HeaderMap) -> Response {
    let Some(owner) = authorization_owner(&state.storage, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    match state.begin(owner).await {
        Some(body) => reply(StatusCode::OK, body),
        None => error(StatusCode::INTERNAL_SERVER_ERROR, DEVICE_ERROR),
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
    let Some(handle) = text(&body["deviceCode"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing device code");
    };
    let (registration, lease) = match state.claim(owner, hash(handle)) {
        Ok(claimed) => claimed,
        Err(code) => return reply(StatusCode::OK, failure(code, None)),
    };
    let result = tokio::spawn(async move { state.poll(registration, lease).await })
        .await
        .unwrap_or_else(|_| {
            failure(
                "exchange_failed",
                Some("Could not complete the Kimi sign-in"),
            )
        });
    reply(StatusCode::OK, result)
}

impl Device {
    async fn begin(&self, owner: Key) -> Option<Value> {
        let device_id = Uuid::new_v4().to_string();
        let (status, data) = self
            .portal
            .prepare(&self.storage, &Value::Null, &device_id, Endpoint::Register)
            .await
            .ok()?
            .send()
            .await
            .ok()?;
        let device_code = text(&data["device_code"]).filter(|_| status.is_success())?;
        let user_code = text(&data["user_code"])?;
        let verify = verification_uri(&data["verification_uri"]).unwrap_or(VERIFY);
        let uri;
        let complete = if let Some(value) = verification_uri(&data["verification_uri_complete"]) {
            value
        } else {
            uri = Url::parse_with_params(VERIFY, [("user_code", user_code)]).ok()?;
            uri.as_str()
        };
        let seconds = data["expires_in"].as_u64().filter(|seconds| *seconds > 0)?;
        let interval = data["interval"]
            .as_u64()
            .filter(|seconds| *seconds > 0)
            .unwrap_or(5);
        let now = Instant::now();
        let expires = now.checked_add(Duration::from_secs(seconds))?;
        let wait = Duration::from_secs(interval);
        let next_poll = now.checked_add(wait)?;
        let handle = random()?;
        let verifier = random()?;
        let reply = json!({
            "device_code":handle, "user_code":user_code, "verification_uri":verify,
            "verification_uri_complete":complete, "expires_in":seconds, "interval":interval,
            "codeVerifier":verifier, "_kimiDeviceId":device_id,
        });
        let mut pending = entries(&self.pending);
        pending.retain(|_, attempt| attempt.polling || attempt.expires > now);
        if pending.len() >= MAX_PENDING {
            return None;
        }
        let Entry::Vacant(slot) = pending.entry(hash(&handle)) else {
            return None;
        };
        slot.insert(Attempt {
            owner,
            registration: Arc::new(Registration {
                device_code: device_code.into(),
                device_id,
            }),
            expires,
            next_poll,
            interval: wait,
            polling: false,
        });
        Some(reply)
    }

    fn claim(&self, owner: Key, key: Key) -> Result<(Arc<Registration>, Lease), &'static str> {
        let mut pending = entries(&self.pending);
        let now = Instant::now();
        let Some(attempt) = pending
            .get_mut(&key)
            .filter(|attempt| attempt.owner == owner && attempt.expires > now)
        else {
            return Err("expired_token");
        };
        if attempt.polling {
            return Err("authorization_pending");
        }
        if attempt.next_poll > now {
            return Err("slow_down");
        }
        attempt.polling = true;
        attempt.next_poll = now.checked_add(attempt.interval).unwrap_or(attempt.expires);
        Ok((
            attempt.registration.clone(),
            Lease {
                pending: self.pending.clone(),
                key,
                waiting: None,
            },
        ))
    }

    async fn poll(&self, registration: Arc<Registration>, mut lease: Lease) -> Value {
        let result = self
            .portal
            .prepare(
                &self.storage,
                &Value::Null,
                &registration.device_id,
                Endpoint::Poll(&registration.device_code),
            )
            .await;
        let Ok(request) = result else {
            return failure(
                "exchange_failed",
                Some("Could not prepare Kimi device authorization"),
            );
        };
        let Ok((status, data)) = request.send().await else {
            return failure(
                "exchange_failed",
                Some("Could not poll for a Kimi device token"),
            );
        };
        if status.is_success() && text(&data["access_token"]).is_some() && data["error"].is_null() {
            return self.save(&registration.device_id, &data).await;
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
        lease.waiting = match code {
            "authorization_pending" => Some(Waiting::Pending),
            "slow_down" => Some(Waiting::SlowDown),
            _ => None,
        };
        let description = match code {
            "no_access_token" => Some("No access token received"),
            "invalid_response" => Some("non-json token response"),
            _ => None,
        };
        failure(code, description)
    }

    async fn save(&self, device_id: &str, data: &Value) -> Value {
        let Ok(mut fields) = token_fields(data, Utc::now()) else {
            return failure("exchange_failed", Some("Invalid Kimi device token"));
        };
        let mut details = json!({"authMethod":"device_code","deviceId":device_id});
        if let Some(client_id) = self.portal.saved_client_id() {
            details["deviceClientId"] = json!(client_id);
        }
        fields.insert("providerSpecificData".into(), details);
        match save_imported(&self.storage, "kimi", fields).await {
            Ok(saved) => json!({"success":true,"connection":{"id":saved["id"],"provider":"kimi"}}),
            Err(_) => failure(
                "exchange_failed",
                Some("Could not save the provider connection"),
            ),
        }
    }
}

struct Lease {
    pending: Pool,
    key: Key,
    waiting: Option<Waiting>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut pending = entries(&self.pending);
        let now = Instant::now();
        if let Some(attempt) = pending
            .get_mut(&self.key)
            .filter(|attempt| attempt.expires > now)
            && let Some(waiting) = &self.waiting
        {
            attempt.polling = false;
            if matches!(waiting, Waiting::SlowDown) {
                attempt.interval = attempt.interval.saturating_add(Duration::from_secs(5));
                attempt.next_poll = now.checked_add(attempt.interval).unwrap_or(attempt.expires);
            }
        } else {
            pending.remove(&self.key);
        }
    }
}

fn entries(pool: &Pool) -> MutexGuard<'_, HashMap<Key, Attempt>> {
    pool.lock().unwrap_or_else(|error| error.into_inner())
}

fn verification_uri(value: &Value) -> Option<&str> {
    text(value).filter(|value| {
        Url::parse(value).is_ok_and(|url| {
            url.scheme() == "https"
                && matches!(
                    url.host_str(),
                    Some("kimi.com" | "www.kimi.com" | "auth.kimi.com")
                )
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
        })
    })
}

fn random() -> Option<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

fn hash(value: &str) -> Key {
    Sha256::digest(value.as_bytes()).into()
}

fn failure(code: &str, description: Option<&str>) -> Value {
    let mut result = json!({"success":false,"error":code,"pending":matches!(code,"authorization_pending"|"slow_down")});
    if let Some(description) = description {
        result["errorDescription"] = json!(description);
    }
    result
}

fn error(status: StatusCode, text: &'static str) -> Response {
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
