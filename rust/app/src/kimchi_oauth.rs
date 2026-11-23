use std::collections::{HashMap, hash_map::Entry};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header, uri::Authority};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, ClientBuilder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

use crate::dashboard_session::authorization_owner;
use crate::provider_connections::object;
use crate::provider_index::js_trim;
use crate::provider_oauth::save_imported;
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

const AUTH: &str = "https://app.kimchi.dev/cli-auth";
const VALIDATE: &str = "https://api.cast.ai/v1/llm/openai/supported-providers";
const PROFILE: &str = "https://app.kimchi.dev/api/v1/me";
const FIXTURE: &str = "http://127.0.0.1:52989/kimchi-oauth";
const STATE_PREFIX: &str = "kimchi_";
const TTL: Duration = Duration::from_secs(600);
const MAX_PENDING: usize = 512;
const PROFILE_LIMIT: usize = 128 * 1024;

type Key = [u8; 32];
type Pending = Arc<Mutex<HashMap<Key, Attempt>>>;

#[derive(Clone)]
struct OAuth {
    storage: Storage,
    client: Client,
    fixture: bool,
    pending: Pending,
}

struct Attempt {
    owner: Key,
    verifier: Key,
    redirect: String,
    expires: Instant,
}

#[derive(Clone, Copy)]
enum Endpoint {
    Validate,
    Profile,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/oauth/kimchi/authorize", get(authorize))
        .route("/api/oauth/kimchi/exchange", post(exchange))
        .with_state(OAuth {
            storage,
            client: client_builder().build().expect("Kimchi OAuth HTTP client"),
            fixture: std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"),
            pending: Arc::default(),
        })
}

async fn authorize(State(oauth): State<OAuth>, request: Request) -> Response {
    let Some(owner) = authorization_owner(&oauth.storage, request.headers()).await else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let Some(redirect) = redirect(&request) else {
        return error(StatusCode::BAD_REQUEST, "Invalid redirect URI");
    };
    match oauth.authorize(owner, redirect) {
        Some(result) => reply(StatusCode::OK, result),
        None => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not prepare Kimchi authorization",
        ),
    }
}

async fn exchange(State(oauth): State<OAuth>, request: Request) -> Response {
    let Some(owner) = authorization_owner(&oauth.storage, request.headers()).await else {
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
    let (Some(token), Some(state), Some(verifier), Some(redirect)) = (
        text(&body["code"]).or_else(|| text(&body["token"])),
        text(&body["state"]),
        text(&body["codeVerifier"]),
        text(&body["redirectUri"]),
    ) else {
        return error(StatusCode::BAD_REQUEST, "Missing required fields");
    };
    let token = js_trim(token);
    if token.is_empty()
        || token.len() > 16 * 1024
        || !token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return error(StatusCode::BAD_REQUEST, "Invalid Kimchi token");
    }
    if !oauth.consume(owner, state, verifier, redirect) {
        return error(StatusCode::BAD_REQUEST, "Invalid or expired OAuth state");
    }
    let token = token.to_owned();
    tokio::spawn(async move { oauth.exchange(&token).await })
        .await
        .unwrap_or_else(|_| {
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not complete Kimchi authorization",
            )
        })
}

impl OAuth {
    fn authorize(&self, owner: Key, redirect: String) -> Option<Value> {
        let state = format!("{STATE_PREFIX}{}", random()?);
        let verifier = random()?;
        let verifier_hash = hash(&verifier);
        let challenge = URL_SAFE_NO_PAD.encode(verifier_hash);
        let authorize =
            Url::parse_with_params(AUTH, [("callback", redirect.as_str()), ("state", &state)])
                .ok()?;
        let now = Instant::now();
        let mut pending = entries(&self.pending);
        pending.retain(|_, attempt| attempt.expires > now);
        if pending.len() >= MAX_PENDING {
            return None;
        }
        let Entry::Vacant(slot) = pending.entry(hash(&state)) else {
            return None;
        };
        slot.insert(Attempt {
            owner,
            verifier: verifier_hash,
            redirect: redirect.clone(),
            expires: now + TTL,
        });
        Some(json!({
            "authUrl":authorize.as_str(), "state":state, "codeVerifier":verifier,
            "codeChallenge":challenge, "redirectUri":redirect, "flowType":"browser_token", "callbackPath":"/callback",
        }))
    }

    fn consume(&self, owner: Key, state: &str, verifier: &str, redirect: &str) -> bool {
        let key = hash(state);
        let mut pending = entries(&self.pending);
        let now = Instant::now();
        if !pending.get(&key).is_some_and(|attempt| {
            attempt.expires > now
                && attempt.owner == owner
                && attempt.redirect == redirect
                && bool::from(attempt.verifier.ct_eq(&hash(verifier)))
        }) {
            return false;
        }
        pending.remove(&key);
        true
    }

    async fn exchange(&self, token: &str) -> Response {
        let validation = self.request(Endpoint::Validate, token).await;
        let Ok(validation) = validation else {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not validate the Kimchi token",
            );
        };
        if !validation.status().is_success() {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!(
                    "Kimchi token validation failed: {}",
                    validation.status().as_u16()
                ),
            );
        }
        let user = self.profile(token).await.unwrap_or_default();
        let field = |name: &str| text(&user[name]).filter(|value| !value.contains(token));
        let user_id = field("id")
            .map(str::to_owned)
            .or_else(|| {
                user["id"]
                    .as_number()
                    .filter(|id| id.as_f64().is_some_and(|id| id != 0.0))
                    .map(ToString::to_string)
                    .filter(|id| !id.contains(token))
            })
            .unwrap_or_default();
        let username = field("username").unwrap_or_default();
        let email = field("email")
            .map(str::to_owned)
            .or_else(|| (!user_id.is_empty()).then(|| format!("kimchi-user-{user_id}")));
        let display_name = field("name").or_else(|| (!username.is_empty()).then_some(username));
        let credentials = object(json!({
            "accessToken":token, "refreshToken":null, "email":email, "displayName":display_name,
            "providerSpecificData":{"authMethod":"browser_token","userId":user_id,"username":username},
        }));
        let Ok(saved) = save_imported(&self.storage, "kimchi", credentials).await else {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the provider connection",
            );
        };
        let mut connection = json!({"id":saved["id"],"provider":"kimchi"});
        for name in ["email", "displayName"] {
            if let Some(value) = saved.get(name) {
                connection[name] = value.clone();
            }
        }
        reply(
            StatusCode::OK,
            json!({"success":true,"connection":connection}),
        )
    }

    async fn profile(&self, token: &str) -> Option<Value> {
        let mut response = self.request(Endpoint::Profile, token).await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if chunk.len() > PROFILE_LIMIT - bytes.len() {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<Value>(&bytes)
            .ok()
            .filter(Value::is_object)
    }

    async fn request(&self, endpoint: Endpoint, token: &str) -> Result<reqwest::Response, ()> {
        let target = match endpoint {
            Endpoint::Validate => VALIDATE,
            Endpoint::Profile => PROFILE,
        };
        let (client, address) = if self.fixture {
            let path = match endpoint {
                Endpoint::Validate => "supported-providers",
                Endpoint::Profile => "me",
            };
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
        client
            .get(address)
            .bearer_auth(token)
            .header(header::ACCEPT, "application/json")
            .header(header::USER_AGENT, "9Router")
            .send()
            .await
            .map_err(|_| ())
    }
}

fn redirect(request: &Request) -> Option<String> {
    let headers = request.headers();
    let authority = request.uri().authority().cloned().or_else(|| {
        headers
            .get(header::HOST)?
            .to_str()
            .ok()?
            .parse::<Authority>()
            .ok()
    })?;
    let forwarded_scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok());
    let observed_scheme = request
        .uri()
        .scheme_str()
        .or(forwarded_scheme)
        .filter(|scheme| matches!(*scheme, "http" | "https"))
        .unwrap_or("http");
    let supplied =
        url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes()).find_map(
            |(key, value)| (key == "redirect_uri" && !value.is_empty()).then(|| value.into_owned()),
        );
    let value = supplied.unwrap_or_else(|| format!("{observed_scheme}://{authority}/callback"));
    let candidate = Url::parse(&value).ok()?;
    if candidate.scheme() != observed_scheme
        || candidate.path() != "/callback"
        || !candidate.username().is_empty()
        || candidate.password().is_some()
        || candidate.query().is_some()
        || candidate.fragment().is_some()
    {
        return None;
    }
    let observed = Url::parse(&format!("{observed_scheme}://{authority}")).ok()?;
    if candidate.origin() != observed.origin() {
        return None;
    }
    if headers.get(header::ORIGIN).is_some_and(|origin| {
        origin
            .to_str()
            .ok()
            .and_then(|origin| Url::parse(origin).ok())
            .is_none_or(|origin| origin.origin() != candidate.origin())
    }) {
        return None;
    }
    Some(value)
}

fn client_builder() -> ClientBuilder {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

fn entries(pending: &Pending) -> MutexGuard<'_, HashMap<Key, Attempt>> {
    pending.lock().unwrap_or_else(|error| error.into_inner())
}

fn random() -> Option<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

fn hash(value: &str) -> Key {
    Sha256::digest(value.as_bytes()).into()
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

fn error(status: StatusCode, message: &str) -> Response {
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
