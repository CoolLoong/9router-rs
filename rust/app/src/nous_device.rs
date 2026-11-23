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
use chrono::{SecondsFormat, TimeDelta, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::dashboard_session::authorization_owner;
use crate::provider_connections::{object, truthy};
use crate::provider_oauth::{
    nous_portal::{self, Endpoint, Portal},
    save_imported,
};
use crate::storage::Storage;

const MAX_PENDING: usize = 512;
const DEVICE_ERROR: &str = "Nous device code request failed";

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
    device_code: String,
    expires: Instant,
    next_poll: Instant,
    interval: Duration,
    polling: bool,
}

enum Waiting {
    Pending,
    SlowDown,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/oauth/nous/device-code", get(begin))
        .route("/api/oauth/nous/poll", post(poll))
        .with_state(Device {
            storage,
            portal: Portal::new().expect("Nous OAuth HTTP client"),
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
    let Some(handle) = nonempty(&body["deviceCode"]) else {
        return error(StatusCode::BAD_REQUEST, "Missing device code");
    };
    let (device_code, lease) = match state.claim(owner, hash(handle.as_bytes())) {
        Ok(claimed) => claimed,
        Err(code) => return reply(StatusCode::OK, poll_error(code)),
    };
    let result = tokio::spawn(async move { state.poll(&device_code, lease).await })
        .await
        .unwrap_or_else(|_| exchange_failed("Could not complete the Nous sign-in"));
    reply(StatusCode::OK, result)
}

impl Device {
    async fn begin(&self, owner: Key) -> Option<Value> {
        let (status, data) = self
            .portal
            .send(&self.storage, &Value::Null, Endpoint::DeviceCode)
            .await
            .ok()?;
        let device_code = nonempty(&data["device_code"]).filter(|_| status.is_success())?;
        let user_code = nonempty(&data["user_code"])?;
        let verification = verification_uri(&data["verification_uri"])?;
        let seconds = data["expires_in"].as_u64().filter(|value| *value > 0)?;
        let interval = data["interval"].as_u64().filter(|value| *value > 0);
        let now = Instant::now();
        let expires = now.checked_add(Duration::from_secs(seconds))?;
        let wait = Duration::from_secs(interval.unwrap_or(5));
        let next_poll = now.checked_add(wait)?;
        let handle = random()?;
        let verifier = random()?;
        let mut pending = entries(&self.pending);
        pending.retain(|_, attempt| attempt.polling || attempt.expires > now);
        if pending.len() >= MAX_PENDING {
            return None;
        }
        let Entry::Vacant(slot) = pending.entry(hash(handle.as_bytes())) else {
            return None;
        };
        slot.insert(Attempt {
            owner,
            device_code: device_code.into(),
            expires,
            next_poll,
            interval: wait,
            polling: false,
        });
        let mut reply = json!({
            "device_code":handle, "user_code":user_code, "verification_uri":verification,
            "expires_in":seconds, "codeVerifier":verifier,
        });
        if let Some(verification) = verification_uri(&data["verification_uri_complete"]) {
            reply["verification_uri_complete"] = json!(verification);
        }
        if let Some(interval) = interval {
            reply["interval"] = json!(interval);
        }
        Some(reply)
    }

    fn claim(&self, owner: Key, key: Key) -> Result<(String, Lease), &'static str> {
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
            attempt.device_code.clone(),
            Lease {
                pending: self.pending.clone(),
                key,
                waiting: None,
            },
        ))
    }

    async fn poll(&self, device_code: &str, mut lease: Lease) -> Value {
        let Ok((status, data)) = self
            .portal
            .send(&self.storage, &Value::Null, Endpoint::Token(device_code))
            .await
        else {
            return exchange_failed("Could not poll for a Nous device token");
        };
        let ok = status.is_success();
        if let Some(access) = nonempty(&data["access_token"]).filter(|_| ok) {
            let minted = self
                .portal
                .mint(&self.storage, &Value::Null, access, None)
                .await;
            return self.save(access, &data, &minted).await;
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
            None if ok => "no_access_token",
            _ => "device_authorization_failed",
        };
        lease.waiting = match code {
            "authorization_pending" => Some(Waiting::Pending),
            "slow_down" => Some(Waiting::SlowDown),
            _ => None,
        };
        let mut response = poll_error(code);
        if code == "no_access_token" {
            response["errorDescription"] = json!("No access token received");
        }
        response
    }

    async fn save(&self, access: &str, data: &Value, minted: &Value) -> Value {
        let (key, mut details) = nous_portal::credentials(access, minted);
        details.retain(|_, value| truthy(value));
        let mut fields = object(json!({
            "accessToken":access, "apiKey":key,
            "providerSpecificData":details,
        }));
        for (from, to) in [
            ("refresh_token", "refreshToken"),
            ("scope", "scope"),
            ("expires_in", "expiresIn"),
        ] {
            if let Some(value) = data.get(from) {
                fields.insert(to.into(), value.clone());
            }
        }
        if let Some(expiry) = data["expires_in"]
            .as_i64()
            .or_else(|| data["expires_in"].as_str()?.parse().ok())
            .filter(|seconds| *seconds != 0)
        {
            let Some(at) = TimeDelta::try_seconds(expiry)
                .and_then(|delta| Utc::now().checked_add_signed(delta))
            else {
                return exchange_failed("Invalid Nous token expiration");
            };
            fields.insert(
                "expiresAt".into(),
                json!(at.to_rfc3339_opts(SecondsFormat::Millis, true)),
            );
        }
        match save_imported(&self.storage, "nous", fields).await {
            Ok(saved) => json!({"success":true,"connection":{"id":saved["id"],"provider":"nous"}}),
            Err(_) => exchange_failed("Could not save the provider connection"),
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
            && let Some(status) = &self.waiting
        {
            attempt.polling = false;
            if matches!(status, Waiting::SlowDown) {
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
    value.as_str().filter(|value| {
        Url::parse(value).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some("portal.nousresearch.com")
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

fn hash(value: &[u8]) -> Key {
    Sha256::digest(value).into()
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn poll_error(code: &str) -> Value {
    json!({"success":false,"error":code,"pending":matches!(code,"authorization_pending"|"slow_down")})
}

fn exchange_failed(message: &'static str) -> Value {
    json!({"success":false,"error":"exchange_failed","errorDescription":message,"pending":false,"fatal":true})
}

fn error(status: StatusCode, message: &'static str) -> Response {
    reply(status, json!({"error":message}))
}

fn reply(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}
