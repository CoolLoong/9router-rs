use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD},
};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio::time::Instant;
use url::Url;
use uuid::Uuid;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::catalog_guard::is_direct_loopback;
use crate::dashboard_session::authorization_owner;
use crate::provider_imports::save_xiaomi_authorized;
use crate::settings::reply;
use crate::storage::Storage;

const LIFETIME: Duration = Duration::from_secs(300);
const MAX_SESSIONS: usize = 32;
const SPKI_PREFIX: &[u8] = b"\x30\x2a\x30\x05\x06\x03\x2b\x65\x6e\x03\x21\x00";
const PLATFORM: &str = "https://platform.xiaomimimo.com/authorize";
const BASE: &str = "https://api.xiaomimimo.com/v1";

#[derive(Clone)]
struct Manager {
    storage: Storage,
    sessions: Arc<Mutex<HashMap<Uuid, Session>>>,
}

struct Session {
    owner: [u8; 32],
    name: String,
    port: u16,
    until: Instant,
    stage: Stage,
    _stop: oneshot::Sender<()>,
}

enum Stage {
    Reserved,
    Pending(StaticSecret),
    Ready(Credential),
    Saving,
}

#[derive(Clone)]
struct Credential {
    key: String,
    uid: Value,
    base: String,
}

#[derive(Clone)]
struct Callback {
    manager: Manager,
    id: Uuid,
    port: u16,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/oauth/xiaomi-mimo/authorize", get(authorize))
        .route("/api/oauth/xiaomi-mimo/start-proxy", get(start_proxy))
        .route("/api/oauth/xiaomi-mimo/stop-proxy", get(stop_proxy))
        .route("/api/oauth/xiaomi-mimo/poll-status", get(poll))
        .route("/api/oauth/xiaomi-mimo/exchange", post(exchange))
        .with_state(Manager {
            storage,
            sessions: Arc::default(),
        })
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}), true)
}

async fn start_proxy(State(manager): State<Manager>, headers: HeaderMap) -> Response {
    let Some(owner) = authorization_owner(&manager.storage, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    match manager.open(owner, String::new(), Stage::Reserved).await {
        Ok(port) => reply(
            StatusCode::OK,
            json!({"success":true,"port":port,"callbackUrl":callback_url(port)}),
            true,
        ),
        Err(status) => error(status, "Could not start the MiMo callback listener"),
    }
}

async fn authorize(
    State(manager): State<Manager>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Some(owner) = authorization_owner(&manager.storage, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let name = match requested_state(&query) {
        Ok(Some(name)) => name.to_owned(),
        Ok(None) => Uuid::new_v4().to_string(),
        Err(()) => return error(StatusCode::BAD_REQUEST, "Invalid state"),
    };
    let mut seed = [0; 32];
    if getrandom::fill(&mut seed).is_err() {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not create a MiMo authorization key",
        );
    }
    let private = StaticSecret::from(seed);
    let public = PublicKey::from(&private);
    let port = match manager
        .open(owner, name.clone(), Stage::Pending(private))
        .await
    {
        Ok(port) => port,
        Err(status) => return error(status, "Could not open a new MiMo authorization session"),
    };
    let mut url = Url::parse(PLATFORM).expect("fixed MiMo platform URL");
    let redirect = callback_url(port);
    let spki = [SPKI_PREFIX, public.as_bytes().as_slice()].concat();
    url.query_pairs_mut()
        .append_pair("pk", &STANDARD.encode(spki))
        .append_pair("redirect_uri", &redirect)
        .append_pair("kn", "mimocode")
        .append_pair("key_name", &key_name());
    reply(
        StatusCode::OK,
        json!({"state":name,"authorizeUrl":url.as_str(),"redirectUri":redirect,"port":port}),
        true,
    )
}

impl Manager {
    async fn open(&self, owner: [u8; 32], name: String, stage: Stage) -> Result<u16, StatusCode> {
        let mut sessions = self.sessions.lock().await;
        let now = Instant::now();
        sessions.retain(|_, session| session.until > now);
        if !name.is_empty()
            && sessions
                .values()
                .any(|value| value.owner == owner && value.name == name)
        {
            return Err(StatusCode::CONFLICT);
        }
        if let Some(reserved) = sessions
            .values_mut()
            .find(|session| session.owner == owner && matches!(session.stage, Stage::Reserved))
        {
            reserved.name = name;
            reserved.stage = stage;
            return Ok(reserved.port);
        }
        if sessions.len() >= MAX_SESSIONS {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let port = listener
            .local_addr()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .port();
        let id = Uuid::new_v4();
        let (stop, stopped) = oneshot::channel();
        let until = now + LIFETIME;
        sessions.insert(
            id,
            Session {
                owner,
                name,
                port,
                until,
                stage,
                _stop: stop,
            },
        );
        let state = Callback {
            manager: self.clone(),
            id,
            port,
        };
        tokio::spawn(async move {
            let router = Router::new()
                .route("/", get(callback))
                .with_state(state.clone());
            tokio::select! {
                _ = axum::serve(listener, router).into_future() => {},
                _ = tokio::time::sleep_until(until) => {},
                _ = stopped => {},
            }
            state.manager.sessions.lock().await.remove(&state.id);
        });
        Ok(port)
    }
}

async fn callback(
    State(state): State<Callback>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if !callback_headers(&headers, state.port) {
        return callback_reply(StatusCode::FORBIDDEN, "Invalid callback origin");
    }
    let Some(packet) = query.get("u").filter(|value| value.len() <= 16 * 1024) else {
        return callback_reply(
            StatusCode::BAD_REQUEST,
            "Missing or invalid encrypted callback",
        );
    };
    let mut sessions = state.manager.sessions.lock().await;
    let Some(session) = sessions
        .get_mut(&state.id)
        .filter(|session| session.until > Instant::now())
    else {
        return callback_reply(StatusCode::GONE, "MiMo authorization has expired");
    };
    let Stage::Pending(private) = &session.stage else {
        return callback_reply(StatusCode::CONFLICT, "MiMo authorization is not pending");
    };
    let Some(credential) = decrypt(private, packet) else {
        return callback_reply(StatusCode::BAD_REQUEST, "Invalid encrypted MiMo callback");
    };
    session.stage = Stage::Ready(credential);
    callback_reply(
        StatusCode::OK,
        "Xiaomi account linked. Return to 9Router to save the connection.",
    )
}

async fn poll(
    State(manager): State<Manager>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Some(owner) = authorization_owner(&manager.storage, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let Ok(Some(name)) = requested_state(&query) else {
        return error(StatusCode::BAD_REQUEST, "Missing or invalid state");
    };
    let sessions = manager.sessions.lock().await;
    let status = sessions
        .values()
        .find(|session| {
            session.owner == owner && session.name == name && session.until > Instant::now()
        })
        .map_or("unknown", |session| {
            if matches!(session.stage, Stage::Ready(_)) {
                "done"
            } else {
                "pending"
            }
        });
    reply(StatusCode::OK, json!({"status":status}), true)
}

async fn stop_proxy(
    State(manager): State<Manager>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Some(owner) = authorization_owner(&manager.storage, &headers).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let Ok(name) = requested_state(&query) else {
        return error(StatusCode::BAD_REQUEST, "Invalid state");
    };
    manager.sessions.lock().await.retain(|_, session| {
        session.owner != owner
            || matches!(session.stage, Stage::Saving)
            || name.is_some_and(|name| name != session.name)
    });
    reply(StatusCode::OK, json!({"success":true}), true)
}

async fn exchange(State(manager): State<Manager>, request: Request) -> Response {
    let Some(owner) = authorization_owner(&manager.storage, request.headers()).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let include_session = is_direct_loopback(&request);
    let Some(body) = to_bytes(request.into_body(), 4096)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return error(StatusCode::BAD_REQUEST, "Invalid request body");
    };
    let Some(name) = body["state"].as_str().filter(|name| valid_state(name)) else {
        return error(StatusCode::BAD_REQUEST, "Missing or invalid state");
    };
    let (id, credential) = {
        let mut sessions = manager.sessions.lock().await;
        let Some((id, session)) = sessions.iter_mut().find(|(_, session)| {
            session.owner == owner && session.name == name && session.until > Instant::now()
        }) else {
            return error(
                StatusCode::BAD_REQUEST,
                "MiMo authorization session is unknown or expired",
            );
        };
        let Stage::Ready(credential) = &session.stage else {
            return error(
                StatusCode::CONFLICT,
                "MiMo authorization session is not ready to save",
            );
        };
        let credential = credential.clone();
        session.stage = Stage::Saving;
        (*id, credential)
    };
    tokio::spawn(async move {
        let result = save_xiaomi_authorized(
            &manager.storage,
            credential.key.clone(),
            credential.uid.clone(),
            credential.base.clone(),
            include_session,
        )
        .await;
        let mut sessions = manager.sessions.lock().await;
        match result {
            Ok(connection) => {
                sessions.remove(&id);
                reply(
                    StatusCode::OK,
                    json!({"success": true, "connection":connection["connection"]}),
                    true,
                )
            }
            Err(()) => {
                if let Some(session) = sessions.get_mut(&id) {
                    session.stage = Stage::Ready(credential);
                }
                error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not save the MiMo connection",
                )
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save the MiMo connection",
        )
    })
}

fn callback_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

fn callback_headers(headers: &HeaderMap, port: u16) -> bool {
    let authorities = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    headers.get(header::HOST).is_some_and(|host| {
        authorities
            .iter()
            .any(|authority| host == authority.as_str())
    }) && headers.get(header::ORIGIN).is_none_or(|origin| {
        origin
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("http://"))
            .is_some_and(|value| authorities.iter().any(|authority| value == authority))
    })
}

fn callback_reply(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        message,
    )
        .into_response()
}

fn valid_state(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte))
}

fn requested_state(query: &HashMap<String, String>) -> Result<Option<&str>, ()> {
    let value = query.get("state").map(String::as_str);
    if value.is_some_and(|value| !valid_state(value)) {
        return Err(());
    }
    Ok(value)
}

fn key_name() -> String {
    let os = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into());
    let digest = format!("{:x}", Sha256::digest(format!("{os}-{host}")));
    format!("9router-xmd-{}", &digest[..8])
}

fn decrypt(private: &StaticSecret, value: &str) -> Option<Credential> {
    let packet = STANDARD
        .decode(value)
        .or_else(|_| STANDARD_NO_PAD.decode(value))
        .or_else(|_| URL_SAFE_NO_PAD.decode(value))
        .ok()?;
    let nonce = Nonce::assume_unique_for_key(<[u8; 12]>::try_from(packet.get(..12)?).ok()?);
    let peer = PublicKey::from(<[u8; 32]>::try_from(packet.get(12..44)?).ok()?);
    let secret = private.diffie_hellman(&peer);
    if !secret.was_contributory() {
        return None;
    }
    let hash = Sha256::digest(secret.as_bytes());
    let cipher = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &hash).ok()?);
    let mut body = packet.get(44..)?.to_vec();
    let clear = cipher.open_in_place(nonce, Aad::empty(), &mut body).ok()?;
    let payload: Value = serde_json::from_slice(clear).ok()?;
    let key = payload["sk"]
        .as_str()?
        .strip_prefix("sk-")
        .filter(|secret| {
            !secret.is_empty() && secret.bytes().all(|byte| byte.is_ascii_graphic())
        })?;
    let base = match &payload["url"] {
        Value::Null => BASE,
        Value::String(value) if value.is_empty() => BASE,
        Value::String(value) => value.trim_end_matches('/'),
        _ => return None,
    };
    let url = Url::parse(base).ok()?;
    if url.scheme() != "https"
        || !url
            .host_str()
            .is_some_and(|host| host.ends_with(".xiaomimimo.com"))
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let uid = match &payload["uid"] {
        Value::String(value)
            if value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_graphic()) =>
        {
            payload["uid"].clone()
        }
        Value::Number(_) | Value::Null => payload["uid"].clone(),
        _ => return None,
    };
    Some(Credential {
        key: format!("sk-{key}"),
        uid,
        base: base.to_owned(),
    })
}
