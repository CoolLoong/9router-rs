use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use rand::rngs::OsRng;
use rsa::pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey, EncodeRsaPublicKey};
use rsa::pkcs8::LineEnding;
use rsa::traits::PublicKeyParts;
use rsa::{Oaep, Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use super::flow::{ApiError, generate_secret, nonempty};
use super::native_callback::{Callback, Kind, parameters, query_value};
use super::{reply, save_imported};
use crate::chat_output::js_text;
use crate::dashboard_session::authorization_owner;
use crate::provider_connections::object;
use crate::provider_imports::zed::{organization, user_info};
use crate::provider_index::js_trim;
use crate::storage::Storage;

const PORT: u16 = 58443;
const DEFAULT_REDIRECT: &str = "http://localhost:8080/callback";
const ISSUER: &str = "https://zed.dev/native_app_signin";
const VERIFIER: &str = "zed-rsa-pkcs1:";
const INPUT_LIMIT: usize = 64 * 1024;

pub(super) struct Login {
    key: RsaPrivateKey,
    system: String,
}

pub(super) struct Credentials {
    user: String,
    token: String,
    system: String,
}

pub(super) fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/oauth/zed/{action}", any(dispatch))
        .with_state((Callback::new(storage.clone(), Kind::Zed), storage))
}

async fn dispatch(
    State((callback, storage)): State<(Callback, Storage)>,
    Path(action): Path<String>,
    request: Request,
) -> Response {
    let Some(owner) = authorization_owner(&storage, request.headers()).await else {
        return reply(StatusCode::UNAUTHORIZED, json!({"error":"Unauthorized"}));
    };
    let query = parameters(request.uri().query().unwrap_or_default());
    let state = query_value(&query, &["state"]);
    let result = match (request.method(), action.as_str()) {
        (&Method::GET, "authorize") => {
            let redirect = query_value(&query, &["redirect_uri"]).unwrap_or(DEFAULT_REDIRECT);
            let preferred = query_value(&query, &["nativeAppPort", "native_app_port"]);
            authorize(redirect, preferred).await
        }
        (&Method::GET, "start-proxy") => {
            let port = query_value(&query, &["native_app_port"])
                .and_then(|port| port.parse::<u16>().ok())
                .filter(|port| *port != 0)
                .unwrap_or(PORT);
            callback.start_at_port(owner, port).await
        }
        (&Method::GET, "stop-proxy") => Ok(callback.stop(owner).await),
        (&Method::GET, "poll-status") => callback.poll(owner, state).await,
        (&Method::GET, "device-code") => {
            Err(ApiError::bad("Provider does not support device code flow"))
        }
        (&Method::POST, "register-session" | "exchange") => {
            let body = to_bytes(request.into_body(), INPUT_LIMIT)
                .await
                .ok()
                .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
                .filter(Value::is_object);
            let Some(body) = body else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"error":"Invalid or empty request body"}),
                );
            };
            let state = state.or_else(|| nonempty(&body["state"]));
            let system =
                nonempty(&body["systemId"]).or_else(|| nonempty(&body["meta"]["systemId"]));
            let verifier = nonempty(&body["codeVerifier"]);
            if action == "register-session" {
                match (state, verifier) {
                    (None, _) => Err(ApiError::bad("Missing state")),
                    (Some(_), None) => Ok(json!({"success":false})),
                    (Some(state), Some(verifier)) => match Login::parse(verifier, system) {
                        Ok(login) => callback.register_zed(owner, state, login).await,
                        Err(error) => Err(error),
                    },
                }
            } else {
                match (
                    nonempty(&body["code"]),
                    nonempty(&body["redirectUri"]),
                    verifier,
                ) {
                    (Some(code), Some(_), Some(verifier)) => match Login::parse(verifier, system) {
                        Ok(login) => callback
                            .exchange_zed(owner, state, code.to_owned(), login)
                            .await
                            .map(summary),
                        Err(error) => Err(error),
                    },
                    _ => Err(ApiError::bad("Missing required fields")),
                }
            }
        }
        _ => Err(ApiError::bad("Unknown action")),
    };
    match result {
        Ok(value) => reply(StatusCode::OK, value),
        Err(error) => reply(error.status, json!({"error":error.message})),
    }
}

async fn authorize(redirect: &str, preferred: Option<&str>) -> Result<Value, ApiError> {
    let redirect_url = Url::parse(redirect)
        .ok()
        .filter(|url| {
            url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
                && matches!(url.path(), "/" | "/callback")
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
        .ok_or(ApiError::bad("Invalid Zed callback URL"))?;
    let port = redirect_url
        .port()
        .or_else(|| preferred.and_then(|value| value.parse().ok()))
        .filter(|port| *port != 0)
        .unwrap_or(PORT);
    let (public, verifier) = tokio::task::spawn_blocking(|| {
        let private = RsaPrivateKey::new(&mut OsRng, 2048).ok()?;
        let public = RsaPublicKey::from(&private).to_pkcs1_der().ok()?;
        let pem = private.to_pkcs1_pem(LineEnding::LF).ok()?;
        Some((
            URL_SAFE.encode(public.as_bytes()),
            format!("{VERIFIER}{}", URL_SAFE_NO_PAD.encode(pem.as_bytes())),
        ))
    })
    .await
    .ok()
    .flatten()
    .ok_or(ApiError::failed("Could not generate Zed authorization key"))?;
    let state = generate_secret(32)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(generate_secret(32)?));
    let system = Uuid::new_v4().to_string();
    let mut url =
        Url::parse(ISSUER).map_err(|_| ApiError::failed("Invalid Zed authorization endpoint"))?;
    url.query_pairs_mut()
        .append_pair("native_app_port", &port.to_string())
        .append_pair("native_app_public_key", &public)
        .append_pair("system_id", &system);
    Ok(
        json!({"authUrl":url.as_str(),"state":state,"codeVerifier":verifier,"codeChallenge":challenge,
        "redirectUri":redirect,"flowType":"authorization_code","callbackPath":"/","systemId":system}),
    )
}

impl Login {
    fn parse(verifier: &str, system: Option<&str>) -> Result<Self, ApiError> {
        let encoded = verifier
            .strip_prefix(VERIFIER)
            .filter(|value| value.len() <= 8192)
            .ok_or(ApiError::bad("Missing or invalid Zed private key verifier"))?;
        let decoded = decode(encoded).ok_or(ApiError::bad("Invalid Zed private key verifier"))?;
        let key = std::str::from_utf8(&decoded)
            .ok()
            .and_then(|pem| RsaPrivateKey::from_pkcs1_pem(pem).ok())
            .filter(|key| key.n().bits() == 2048)
            .ok_or(ApiError::bad("Invalid Zed private key verifier"))?;
        let system = system.map(js_trim).filter(|value| !value.is_empty());
        if system.is_some_and(|value| !wire(value)) {
            return Err(ApiError::bad("Invalid Zed system ID"));
        }
        let system = system.map_or_else(|| Uuid::new_v4().to_string(), str::to_owned);
        Ok(Self { key, system })
    }

    pub(super) fn key_id(&self) -> [u8; 32] {
        Sha256::new()
            .chain_update(self.key.n().to_bytes_be())
            .chain_update(self.key.e().to_bytes_be())
            .finalize()
            .into()
    }

    pub(super) fn decrypt(&self, raw: &str) -> Option<Credentials> {
        let raw = js_trim(raw);
        let (user, encrypted) = if let Ok(data) = serde_json::from_str::<Value>(raw) {
            let user = ["user_id", "userId"]
                .into_iter()
                .find_map(|field| data.get(field).filter(|value| !value.is_null()))?;
            let encrypted = ["access_token", "accessToken", "token"]
                .into_iter()
                .find_map(|field| nonempty(&data[field]))?;
            (js_text(user), encrypted.to_owned())
        } else {
            let url = Url::parse(raw).ok();
            let query = if let Some(url) = &url {
                url.query()?
            } else {
                raw.split_once('?').map_or(raw, |(_, query)| query)
            };
            let fields = parameters(query);
            let user = query_value(&fields, &["user_id", "userId"])?;
            let encrypted = query_value(&fields, &["access_token", "accessToken", "token"])?;
            (user.to_owned(), encrypted.to_owned())
        };
        if !wire(&user) || encrypted.len() > 512 {
            return None;
        }
        let encrypted = decode(&encrypted)?;
        if encrypted.len() != self.key.size() {
            return None;
        }
        let data = self
            .key
            .decrypt_blinded(&mut OsRng, Oaep::new::<Sha256>(), &encrypted)
            .or_else(|_| {
                self.key
                    .decrypt_blinded(&mut OsRng, Pkcs1v15Encrypt, &encrypted)
            })
            .ok()?;
        let token = String::from_utf8(data).ok().filter(|token| wire(token))?;
        Some(Credentials {
            user,
            token,
            system: self.system.clone(),
        })
    }
}

fn decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(text)
        .or_else(|_| URL_SAFE.decode(text))
        .ok()
}

fn wire(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

pub(super) async fn save(storage: &Storage, account: Credentials) -> Result<Value, ApiError> {
    let mut profile = user_info(storage, &account.user, &account.token, &account.system)
        .await
        .unwrap_or_default();
    crate::provider_tools::scrub_payload(&mut profile, std::slice::from_ref(&account.token));
    let name = nonempty(&profile["name"]).or_else(|| nonempty(&profile["display_name"]));
    let mut details = object(json!({
        "authMethod":"oauth","userId":account.user,"systemId":account.system,
    }));
    let organization = organization(&profile);
    if !organization.is_empty() {
        details.insert("organizationId".into(), json!(organization));
    }
    let mut fields = object(json!({
        "accessToken":account.token,
        "refreshToken":null,
        "expiresAt":null,
        "providerSpecificData":details,
    }));
    if let Some(email) = nonempty(&profile["email"]) {
        fields.insert("email".into(), json!(email));
    }
    if let Some(name) = name {
        fields.insert("displayName".into(), json!(name));
    }
    save_imported(storage, "zed", fields)
        .await
        .map_err(|_| ApiError::failed("Could not save the Zed connection"))
}

fn summary(connection: Value) -> Value {
    let mut summary = json!({"id":connection["id"],"provider":"zed"});
    for field in ["email", "displayName"] {
        if let Some(value) = connection.get(field) {
            summary[field] = value.clone();
        }
    }
    json!({"success":true,"connection":summary})
}
