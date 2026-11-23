use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use super::{body, error, metadata_bytes, metadata_client, nonempty, reply};
use crate::dashboard_session;
use crate::provider_connections::object;
use crate::provider_oauth::{save_imported, token_claims};
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;
use crate::thinking_intent::numeric;

const LOGIN: &str = "https://prod.us-east-1.auth.desktop.kiro.dev/login";
const EXCHANGE: &str = "https://prod.us-east-1.auth.desktop.kiro.dev/oauth/token";
const CALLBACK: &str = "kiro://kiro.kiroAgent/authenticate-success";
const TTL: Duration = Duration::from_secs(600);
static PENDING: LazyLock<Mutex<HashMap<[u8; 32], Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct Pending {
    issued: Instant,
    provider: &'static str,
    state: String,
}

pub(super) async fn authorize(State(storage): State<Storage>, request: Request) -> Response {
    if !dashboard_session::authenticated(&storage, request.headers()).await {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let selected =
        url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
            .find_map(|(key, value)| (key == "provider").then_some(value));
    let Some((provider, upstream)) = selected.as_deref().and_then(provider_info) else {
        return error(
            StatusCode::BAD_REQUEST,
            "Invalid provider. Use 'google' or 'github'",
        );
    };
    let Some((verifier, state)) = random_base64().zip(random_base64()) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not generate OAuth state",
        );
    };
    let challenge = challenge(&verifier);
    let proof = URL_SAFE_NO_PAD.encode(challenge);
    let mut url = Url::parse(LOGIN).expect("pinned Kiro login URL");
    url.query_pairs_mut()
        .append_pair("idp", upstream)
        .append_pair("redirect_uri", CALLBACK)
        .append_pair("code_challenge", &proof)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("prompt", "select_account");
    let Ok(mut pending) = PENDING.lock() else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not record OAuth state",
        );
    };
    pending.retain(|_, entry| entry.issued.elapsed() < TTL);
    if pending.len() >= 512 {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many pending Kiro sign-ins",
        );
    }
    pending.insert(
        challenge,
        Pending {
            issued: Instant::now(),
            provider,
            state: state.clone(),
        },
    );
    reply(
        StatusCode::OK,
        json!({
            "authUrl":url.as_str(),"state":state,"codeVerifier":verifier,"codeChallenge":proof,"provider":provider,
        }),
        true,
    )
}

pub(super) async fn exchange(State(storage): State<Storage>, request: Request) -> Response {
    let input = match body(&storage, request).await {
        Ok(input) => input,
        Err((status, message)) => return error(status, message),
    };
    let Some((code, verifier)) = nonempty(&input["code"]).zip(nonempty(&input["codeVerifier"]))
    else {
        return error(StatusCode::BAD_REQUEST, "Missing required fields");
    };
    let Some((provider, display)) = input["provider"].as_str().and_then(provider_info) else {
        return error(StatusCode::BAD_REQUEST, "Invalid provider");
    };
    if !claim(&input, verifier, provider) {
        return error(StatusCode::BAD_REQUEST, "Invalid or expired Kiro sign-in");
    }
    let record = match tokens(&storage, code, verifier).await {
        Ok(record) => record,
        Err(reason) => return error(StatusCode::INTERNAL_SERVER_ERROR, reason),
    };
    let Some(token) = nonempty(&record["accessToken"]) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Kiro response omitted access token",
        );
    };
    let claims = token_claims(token);
    let email = ["email", "preferred_username", "sub"]
        .iter()
        .find_map(|field| nonempty(&claims[*field]));
    let expires_in = numeric(&record["expiresIn"])
        .filter(|seconds| *seconds > 0.0)
        .unwrap_or(3600.0);
    let ms = (Utc::now().timestamp_millis() as f64 + expires_in * 1000.0).trunc() as i64;
    let Some(expires) = DateTime::<Utc>::from_timestamp_millis(ms) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Invalid Kiro token expiration",
        );
    };
    let mut details = object(json!({"authMethod":provider,"provider":display}));
    if let Some(profile) = record.get("profileArn") {
        details.insert("profileArn".into(), profile.clone());
    }
    let saved = object(json!({
        "accessToken":token,"refreshToken":record["refreshToken"],
        "expiresAt":expires.to_rfc3339_opts(SecondsFormat::Millis,true),
        "email":email,"providerSpecificData":details,
    }));
    match save_imported(&storage, "kiro", saved).await {
        Ok(saved) => reply(
            StatusCode::OK,
            json!({
                "success":true,"connection":{"id":saved["id"],"provider":"kiro","email":saved["email"]},
            }),
            true,
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save the provider connection",
        ),
    }
}

fn claim(input: &Value, verifier: &str, provider: &str) -> bool {
    let Ok(mut pending) = PENDING.lock() else {
        return false;
    };
    let hash = challenge(verifier);
    let allowed = pending.get(&hash).is_some_and(|login| {
        login.issued.elapsed() < TTL
            && login.provider == provider
            && input.get("state").is_none_or(|state| state == &login.state)
    });
    if allowed {
        pending.remove(&hash);
    }
    allowed
}

async fn tokens(storage: &Storage, code: &str, verifier: &str) -> Result<Value, String> {
    let failure = || "Kiro token exchange failed".to_owned();
    let settings = storage.read_settings().await.ok_or_else(failure)?;
    let route = proxy_transport::select(storage, &settings, &Value::Null, EXCHANGE)
        .await
        .map_err(|_| failure())?;
    let client = metadata_client(&route, Duration::from_secs(15)).ok_or_else(failure)?;
    let target = match &route {
        Route::Relay { url, .. } => url,
        Route::Direct if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") => {
            "http://127.0.0.1:52989/kiro-social/oauth/token"
        }
        _ => EXCHANGE,
    };
    let request = client.post(target).json(&json!({
        "code":code,"code_verifier":verifier,"redirect_uri":CALLBACK,
    }));
    let mut response = route.relay(request).send().await.map_err(|_| failure())?;
    let http = response.status();
    let bytes = metadata_bytes(&mut response).await.ok_or_else(failure)?;
    if !http.is_success() {
        let text = String::from_utf8_lossy(&bytes)
            .replace(code, "[redacted]")
            .replace(verifier, "[redacted]");
        let detail = text
            .chars()
            .filter(|ch| !ch.is_control())
            .take(256)
            .collect::<String>();
        return Err(format!("Token exchange failed: {detail}"));
    }
    serde_json::from_slice(&bytes).map_err(|_| failure())
}

fn random_base64() -> Option<String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).ok()?;
    Some(URL_SAFE_NO_PAD.encode(seed))
}

fn challenge(verifier: &str) -> [u8; 32] {
    Sha256::digest(verifier.as_bytes()).into()
}

fn provider_info(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "google" => ("google", "Google"),
        "github" => ("github", "Github"),
        _ => return None,
    })
}
