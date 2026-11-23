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
use subtle::ConstantTimeEq;
use url::Url;

use super::{GITLAB_DEFAULT, body, error, metadata_bytes, metadata_client, nonempty, reply};
use crate::dashboard_session;
use crate::provider_connections::object;
use crate::provider_oauth::save_imported;
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;
use crate::thinking_intent::numeric;

const TTL: Duration = Duration::from_secs(600);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(15);
const NETWORK_ERROR: &str = "GitLab token exchange failed";
static PENDING: LazyLock<Mutex<HashMap<String, Attempt>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct Attempt {
    issued: Instant,
    digest: [u8; 32],
    redirect: String,
    base: String,
    client: String,
}

pub(super) async fn authorize(State(storage): State<Storage>, request: Request) -> Response {
    if !dashboard_session::authenticated(&storage, request.headers()).await {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let query = Value::Object(
        url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
            .map(|(key, value)| (key.into_owned(), Value::String(value.into_owned())))
            .collect(),
    );
    let base = nonempty(&query["baseUrl"])
        .unwrap_or(GITLAB_DEFAULT)
        .trim_end_matches('/');
    let redirect = nonempty(&query["redirect_uri"]).unwrap_or("http://localhost:8080/callback");
    if !valid_url(base, false) || !valid_url(redirect, true) {
        return error(
            StatusCode::BAD_REQUEST,
            "Invalid GitLab base or redirect URL",
        );
    }
    let client = query["clientId"].as_str().unwrap_or_default();
    let Some((verifier, state)) = secret().zip(secret()) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not generate OAuth state",
        );
    };
    let digest = digest(&verifier);
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    let mut url =
        Url::parse(&format!("{base}/oauth/authorize")).expect("validated GitLab base URL");
    url.query_pairs_mut().extend_pairs([
        ("client_id", client),
        ("redirect_uri", redirect),
        ("response_type", "code"),
        ("state", &state),
        ("scope", "api read_user"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ]);
    let Ok(mut pending) = PENDING.lock() else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not record OAuth state",
        );
    };
    pending.retain(|_, attempt| attempt.issued.elapsed() < TTL);
    if pending.len() >= 512 {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many pending OAuth attempts",
        );
    }
    pending.insert(
        state.clone(),
        Attempt {
            issued: Instant::now(),
            digest,
            redirect: redirect.to_owned(),
            base: base.to_owned(),
            client: client.to_owned(),
        },
    );
    reply(
        StatusCode::OK,
        json!({
            "authUrl":url.as_str(),"state":state,"codeVerifier":verifier,"codeChallenge":challenge,
            "redirectUri":redirect,"flowType":"authorization_code_pkce","callbackPath":"/callback",
        }),
        true,
    )
}

pub(super) async fn exchange(State(storage): State<Storage>, request: Request) -> Response {
    let input = match body(&storage, request).await {
        Ok(input) => input,
        Err((status, message)) => return error(status, message),
    };
    let (Some(code), Some(verifier), Some(redirect), Some(state)) = (
        nonempty(&input["code"]),
        nonempty(&input["codeVerifier"]),
        nonempty(&input["redirectUri"]),
        nonempty(&input["state"]),
    ) else {
        return error(StatusCode::BAD_REQUEST, "Missing required fields");
    };
    let Some(attempt) = claim(state, verifier, redirect, &input["meta"]) else {
        return error(StatusCode::BAD_REQUEST, "Invalid or expired OAuth state");
    };
    let client_secret = nonempty(&input["meta"]["clientSecret"]);
    let tokens = match exchange_token(&storage, &attempt, code, verifier, client_secret).await {
        Some(tokens) => tokens,
        None => return error(StatusCode::INTERNAL_SERVER_ERROR, NETWORK_ERROR),
    };
    let Some(access) = nonempty(&tokens["access_token"]) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, NETWORK_ERROR);
    };
    let user = user_info(&storage, &attempt.base, access)
        .await
        .unwrap_or_default();
    let email = nonempty(&user["email"])
        .or_else(|| nonempty(&user["public_email"]))
        .unwrap_or_default();
    let expiry = numeric(&tokens["expires_in"])
        .filter(|seconds| *seconds > 0.0)
        .and_then(|seconds| {
            DateTime::<Utc>::from_timestamp_millis(
                (Utc::now().timestamp_millis() as f64 + seconds * 1000.0).trunc() as i64,
            )
        })
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Millis, true));
    let mut fields = object(json!({
        "accessToken":access,"expiresAt":expiry,"providerSpecificData":{
            "username":nonempty(&user["username"]).unwrap_or_default(),
            "email":email,"name":nonempty(&user["name"]).unwrap_or_default(),
            "baseUrl":attempt.base,"clientId":attempt.client,"authKind":"oauth",
        },
    }));
    for (wire, stored) in [
        ("refresh_token", "refreshToken"),
        ("expires_in", "expiresIn"),
        ("scope", "scope"),
    ] {
        if let Some(value) = tokens.get(wire) {
            fields.insert(stored.into(), value.clone());
        }
    }
    match save_imported(&storage, "gitlab", fields).await {
        Ok(account) => reply(
            StatusCode::OK,
            json!({"success":true,"connection":{"id":account["id"],"provider":"gitlab"}}),
            true,
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save the provider connection",
        ),
    }
}

fn claim(state: &str, verifier: &str, redirect: &str, meta: &Value) -> Option<Attempt> {
    let mut pending = PENDING.lock().ok()?;
    let attempt = pending.get(state)?;
    if attempt.issued.elapsed() >= TTL
        || attempt.redirect != redirect
        || !bool::from(attempt.digest.ct_eq(&digest(verifier)))
        || nonempty(&meta["baseUrl"]).is_some_and(|base| base.trim_end_matches('/') != attempt.base)
        || nonempty(&meta["clientId"]).is_some_and(|client| client != attempt.client)
    {
        return None;
    }
    pending.remove(state)
}

async fn exchange_token(
    storage: &Storage,
    attempt: &Attempt,
    code: &str,
    verifier: &str,
    secret: Option<&str>,
) -> Option<Value> {
    let target = format!("{}/oauth/token", attempt.base);
    let settings = storage.read_settings().await?;
    let route = proxy_transport::select(storage, &settings, &Value::Null, &target)
        .await
        .ok()?;
    let client = metadata_client(&route, NETWORK_TIMEOUT)?;
    let address = match &route {
        Route::Relay { url, .. } => url,
        _ => &target,
    };
    let mut form = vec![
        ("client_id", attempt.client.as_str()),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", attempt.redirect.as_str()),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = secret {
        form.push(("client_secret", secret));
    }
    let mut response = route
        .relay(
            client
                .post(address)
                .header("accept", "application/json")
                .form(&form),
        )
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    serde_json::from_slice(&metadata_bytes(&mut response).await?).ok()
}

async fn user_info(storage: &Storage, base: &str, access: &str) -> Option<Value> {
    let target = format!("{base}/api/v4/user");
    let settings = storage.read_settings().await?;
    let route = proxy_transport::select(storage, &settings, &Value::Null, &target)
        .await
        .ok()?;
    let client = metadata_client(&route, NETWORK_TIMEOUT)?;
    let address = match &route {
        Route::Relay { url, .. } => url,
        _ => &target,
    };
    let mut response = route
        .relay(client.get(address).bearer_auth(access))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    serde_json::from_slice(&metadata_bytes(&mut response).await?).ok()
}

fn valid_url(value: &str, redirect: bool) -> bool {
    value.len() <= 2048
        && Url::parse(value).is_ok_and(|url| {
            matches!(url.scheme(), "https" | "http")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && (redirect || url.query().is_none())
        })
}

fn secret() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

fn digest(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}
