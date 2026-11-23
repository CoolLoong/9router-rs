use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::Utc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha384, Sha512};
use subtle::ConstantTimeEq;

use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::settings::{effective, reply};
use crate::storage::Storage;

const BODY_LIMIT: usize = 128 * 1024 * 1024;
const SESSION_AGE: i64 = 24 * 60 * 60;
const DEFAULT_PASSWORD: &str = "123456";
const DEFAULT_OIDC_LABEL: &str = "Sign in with OIDC";
const DEFAULT_SAML_LABEL: &str = "Sign in with SAML SSO";
const MAX_FAILURES: u8 = 5;
const LOCK_STEPS: [u64; 4] = [30, 120, 600, 1_800];
const FAIL_WINDOW: Duration = Duration::from_secs(60 * 60);
const RESET_HINT: &str =
    "Forgot password? Reset to default via 9Router CLI → Settings → Reset Password to Default.";

#[derive(Clone)]
pub(crate) struct SessionState {
    pub(crate) storage: Storage,
    attempts: Arc<Mutex<HashMap<String, Attempts>>>,
}

#[derive(Default)]
struct Attempts {
    count: u8,
    level: usize,
    last_failure: Option<Instant>,
    locked_until: Option<Instant>,
}

impl Attempts {
    fn locked(&self, now: Instant) -> Option<u64> {
        let remaining = self.locked_until?.checked_duration_since(now)?;
        (!remaining.is_zero())
            .then(|| remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0))
    }

    fn expired(&self, now: Instant) -> bool {
        self.last_failure
            .is_some_and(|time| now.duration_since(time) > FAIL_WINDOW)
            && self.locked(now).is_none()
    }

    fn fail(&mut self, now: Instant) {
        self.count += 1;
        self.last_failure = Some(now);
        if self.count >= MAX_FAILURES {
            let seconds = LOCK_STEPS[self.level.min(LOCK_STEPS.len() - 1)];
            self.locked_until = Some(now + Duration::from_secs(seconds));
            self.level = self.level.saturating_add(1);
            self.count = 0;
        }
    }
}

impl SessionState {
    pub(crate) fn check_lock(&self, ip: &str) -> Option<u64> {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        let current = attempts.get(ip)?;
        if current.expired(now) {
            attempts.remove(ip);
            return None;
        }
        current.locked(now)
    }

    pub(crate) fn succeed(&self, ip: &str) {
        self.attempts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(ip);
    }

    pub(crate) fn fail(&self, ip: String) -> Response {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        let entry = attempts.entry(ip).or_default();
        if entry.expired(now) {
            *entry = Attempts::default();
        }
        entry.fail(now);
        if let Some(retry) = entry.locked(now) {
            return locked(retry);
        }
        let left = MAX_FAILURES.saturating_sub(entry.count);
        reply(
            StatusCode::UNAUTHORIZED,
            json!({
                "error": format!("Invalid password. {left} attempt(s) left before lockout."),
                "remainingBeforeLock": left,
            }),
            false,
        )
    }
}

pub fn router(storage: Storage) -> Router {
    let state = SessionState {
        storage,
        attempts: Arc::new(Mutex::new(HashMap::new())),
    };
    Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/status", get(status))
        .merge(crate::saml::routes())
        .with_state(state)
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    reply(status, json!({ "error": message.into() }), false)
}

fn locked(retry: u64) -> Response {
    let mut response = reply(
        StatusCode::TOO_MANY_REQUESTS,
        json!({
            "error": format!("Too many failed attempts. Try again in {retry}s. {RESET_HINT}"),
            "retryAfter": retry,
            "resetHint": RESET_HINT,
        }),
        false,
    );
    if let Ok(value) = HeaderValue::from_str(&retry.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

pub(crate) fn caller(request: &Request) -> (String, bool) {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|address| address.0.ip().to_canonical());
    let is_loopback = peer.is_some_and(|ip| {
        ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || ip == IpAddr::V6(Ipv6Addr::LOCALHOST)
    });
    let headers = request.headers();
    let via_proxy = ["x-real-ip", "x-forwarded-for"].iter().any(|name| {
        headers
            .get_all(*name)
            .iter()
            .any(|value| !value.as_bytes().is_empty())
    });
    let real = header_text(headers, "x-real-ip");
    let forward = header_text(headers, "x-forwarded-for");
    let proxy = if real.is_empty() {
        js_trim(forward.split(',').next().unwrap_or_default())
    } else {
        real
    };
    let ip = if is_loopback && !proxy.is_empty() {
        proxy.to_owned()
    } else {
        peer.map_or_else(|| "unknown".into(), |peer| peer.to_string())
    };
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let local_origin = match (origins.next(), origins.next()) {
        (None, _) => true,
        (Some(origin), None) => {
            origin.as_bytes().is_empty()
                || origin
                    .to_str()
                    .ok()
                    .and_then(|value| url::Url::parse(value).ok())
                    .is_some_and(|url| {
                        matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
                    })
        }
        _ => false,
    };
    (ip, is_loopback && !via_proxy && local_origin)
}

pub(crate) fn is_tunnel(headers: &HeaderMap, settings: &Value) -> Result<bool, url::ParseError> {
    let host = header_text(headers, "host")
        .split(':')
        .next()
        .unwrap_or_default();
    let mut tunnel = false;
    for key in ["tunnelUrl", "tailscaleUrl"] {
        if truthy(&settings[key]) {
            let text = js_text(&settings[key]);
            let url = url::Url::parse(&text)?;
            tunnel |= url
                .host_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(host));
        }
    }
    Ok(tunnel)
}

fn read_secret(storage: &Storage) -> io::Result<String> {
    if let Ok(value) = std::env::var("JWT_SECRET")
        && !value.is_empty()
    {
        return Ok(value);
    }
    fs::read_to_string(storage.root().join("jwt-secret")).map(|text| js_trim(&text).to_owned())
}

fn secret(storage: &Storage) -> io::Result<String> {
    match read_secret(storage) {
        Ok(secret) => return Ok(secret),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    let file = storage.root().join("jwt-secret");
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let text = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut temporary = tempfile::NamedTempFile::new_in(storage.root())?;
    temporary.write_all(text.as_bytes())?;
    match temporary.persist_noclobber(file) {
        Ok(_) => Ok(text),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => read_secret(storage),
        Err(error) => Err(error.error),
    }
}

fn session(storage: &Storage, headers: &HeaderMap) -> Option<Value> {
    let token = session_cookie(headers)?;
    let secret = read_secret(storage).ok()?;
    if secret.is_empty() {
        return None;
    }
    verify(&token, secret.as_bytes())
}

pub(crate) fn has_session(storage: &Storage, headers: &HeaderMap) -> bool {
    session(storage, headers).is_some()
}

pub(crate) async fn authorization_owner(
    storage: &Storage,
    headers: &HeaderMap,
) -> Option<[u8; 32]> {
    if let Some(token) = session_cookie(headers)
        && read_secret(storage)
            .ok()
            .is_some_and(|secret| !secret.is_empty() && verify(&token, secret.as_bytes()).is_some())
    {
        return Some(Sha256::digest(token.as_bytes()).into());
    }
    authenticated(storage, headers).await.then_some([0; 32])
}

fn session_token(secret: &str, now: i64, extra: Value) -> Option<String> {
    if secret.is_empty() {
        return None;
    }
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256"}"#);
    let mut claims = json!({"authenticated": true});
    if let Some(extra) = extra.as_object()
        && let Some(claims) = claims.as_object_mut()
    {
        claims.extend(extra.clone());
    }
    claims["iat"] = json!(now);
    claims["exp"] = json!(now + SESSION_AGE);
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let signed = format!("{header}.{payload}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(signed.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Some(format!("{signed}.{signature}"))
}

fn js_type(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        _ => "object",
    }
}

pub(crate) fn compare_password(password: Option<&Value>, hash: &Value) -> Result<bool, String> {
    let (Some(password_text), Some(hash_text)) = (password.and_then(Value::as_str), hash.as_str())
    else {
        return Err(format!(
            "Illegal arguments: {}, {}",
            js_type(password),
            js_type(Some(hash))
        ));
    };
    if hash_text.encode_utf16().count() != 60 {
        return Ok(false);
    }
    bcrypt::verify(password_text, hash_text).map_err(|error| error.to_string())
}

pub(crate) fn verify_password(settings: &Value, password: Option<&Value>) -> Result<bool, String> {
    let Some(text) = password
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    else {
        return Ok(false);
    };
    if truthy(&settings["password"]) {
        return compare_password(password, &settings["password"]);
    }
    let initial = std::env::var("INITIAL_PASSWORD")
        .ok()
        .filter(|password| !password.is_empty());
    let expected = initial.as_deref().unwrap_or(DEFAULT_PASSWORD);
    Ok(bool::from(text.as_bytes().ct_eq(expected.as_bytes())))
}

pub(crate) fn hash_password(password: &Value) -> Result<String, String> {
    let Some(text) = password.as_str() else {
        return Err(format!(
            "Illegal arguments: {}, string",
            js_type(Some(password))
        ));
    };
    bcrypt::hash(text, 10).map_err(|error| error.to_string())
}

pub(crate) fn saml_configured(settings: &Value) -> bool {
    truthy(&settings["samlEntryPoint"]) && truthy(&settings["samlCert"])
}

fn configured_text(value: &Value) -> Option<&str> {
    if truthy(value) {
        value.as_str().map(js_trim)
    } else {
        Some("")
    }
}

pub(crate) fn oidc_configured(settings: &Value) -> Option<bool> {
    let issuer = configured_text(&settings["oidcIssuerUrl"])?;
    if issuer.trim_end_matches('/').is_empty() {
        return Some(false);
    }
    let id = configured_text(&settings["oidcClientId"])?;
    if id.is_empty() {
        return Some(false);
    }
    Some(!configured_text(&settings["oidcClientSecret"])?.is_empty())
}

async fn login(State(state): State<SessionState>, request: Request) -> Response {
    let Some(settings) = state.storage.read_settings().await.map(effective) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read dashboard settings",
        );
    };
    let (ip, local) = caller(&request);
    if let Some(retry) = state.check_lock(&ip) {
        return locked(retry);
    }
    let (parts, body) = request.into_parts();
    let value = to_bytes(body, BODY_LIMIT)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok());
    let Some(value) = value else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Invalid JSON body");
    };
    if value.is_null() {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot destructure property 'password' of '(intermediate value)' as it is null.",
        );
    }
    match is_tunnel(&parts.headers, &settings) {
        Ok(true) if settings["tunnelDashboardAccess"] != true => {
            return error(
                StatusCode::FORBIDDEN,
                "Dashboard access via tunnel is disabled",
            );
        }
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Invalid URL"),
        _ => (),
    }

    let mode = settings["authMode"].as_str().unwrap_or_default();
    let sso_type = settings["ssoType"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or(if mode == "saml" { "saml" } else { "oidc" });
    if matches!(mode, "sso" | "saml" | "oidc") && sso_type == "saml" && saml_configured(&settings) {
        return error(
            StatusCode::FORBIDDEN,
            "Password login is disabled. Use SAML SSO sign in.",
        );
    }
    if matches!(mode, "sso" | "saml" | "oidc") && sso_type == "oidc" {
        match oidc_configured(&settings) {
            Some(true) => {
                return error(
                    StatusCode::FORBIDDEN,
                    "Password login is disabled. Use OIDC sign in.",
                );
            }
            None => return error(StatusCode::INTERNAL_SERVER_ERROR, "Invalid OIDC settings"),
            Some(false) => (),
        }
    }

    let initial = std::env::var("INITIAL_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty());
    let hash = settings.get("password").filter(|value| truthy(value));
    let permitted = if let Some(hash) = hash {
        let hash = hash.clone();
        match tokio::task::spawn_blocking(move || compare_password(value.get("password"), &hash))
            .await
        {
            Ok(Ok(valid)) => valid,
            Ok(Err(message)) => return error(StatusCode::INTERNAL_SERVER_ERROR, message),
            Err(_) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not verify password",
                );
            }
        }
    } else {
        let expected = initial.as_deref().unwrap_or(DEFAULT_PASSWORD);
        value["password"]
            .as_str()
            .is_some_and(|password| bool::from(password.as_bytes().ct_eq(expected.as_bytes())))
    };
    if !permitted {
        return state.fail(ip);
    }
    state.succeed(&ip);
    if hash.is_none() && initial.is_none() && !local {
        return reply(
            StatusCode::FORBIDDEN,
            json!({
                "success": false,
                "error": "Default password must be changed before remote access. Change it from the local machine (or set INITIAL_PASSWORD).",
                "mustChangePassword": true,
            }),
            true,
        );
    }

    let cookie = match issued_cookie(&state.storage, &parts.headers, Value::Null) {
        Ok(cookie) => cookie,
        Err(reason) => return error(StatusCode::INTERNAL_SERVER_ERROR, reason),
    };
    let mut response = reply(
        StatusCode::OK,
        json!({ "success": true, "mustChangePassword": false }),
        true,
    );
    response.headers_mut().append(header::SET_COOKIE, cookie);
    response
}

pub(crate) fn secure_cookie(headers: &HeaderMap) -> bool {
    std::env::var("AUTH_COOKIE_SECURE").as_deref() == Ok("true")
        || header_text(headers, "x-forwarded-proto") == "https"
}

pub(crate) fn issued_cookie(
    storage: &Storage,
    headers: &HeaderMap,
    claims: Value,
) -> Result<HeaderValue, String> {
    let secret = secret(storage).map_err(|error| error.to_string())?;
    let now = Utc::now();
    let token = session_token(&secret, now.timestamp(), claims)
        .ok_or_else(|| "Could not issue dashboard session".to_owned())?;
    let expires =
        (now + chrono::Duration::seconds(SESSION_AGE)).format("%a, %d %b %Y %H:%M:%S GMT");
    let secure = if secure_cookie(headers) {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "auth_token={token}; Path=/; Expires={expires}; Max-Age={SESSION_AGE}{secure}; HttpOnly; SameSite=lax",
    )).map_err(|_| "Could not issue dashboard cookie".to_owned())
}

fn label(value: &Value, fallback: &'static str) -> Option<String> {
    let value = configured_text(value)?;
    Some(if value.is_empty() {
        fallback.into()
    } else {
        value.into()
    })
}

fn claim(session: &Value, key: &str) -> String {
    let value = &session[key];
    if truthy(value) {
        js_trim(&js_text(value)).to_owned()
    } else {
        String::new()
    }
}

fn nonempty(text: &str) -> Option<&str> {
    (!text.is_empty()).then_some(text)
}

fn status_body(settings: &Value, session: Option<Value>) -> Option<Value> {
    let authenticated = session.is_some();
    let session = session.unwrap_or(Value::Null);
    let oidc_name = claim(&session, "oidcName");
    let oidc_email = claim(&session, "oidcEmail");
    let saml_name = claim(&session, "samlName");
    let saml_email = claim(&session, "samlEmail");
    let oidc = truthy(&session["oidc"]);
    let saml = truthy(&session["saml"]);
    let login = if saml {
        "SAML"
    } else if oidc {
        "OIDC"
    } else {
        "Password"
    };
    let display_name = [&saml_name, &saml_email, &oidc_name, &oidc_email]
        .into_iter()
        .find_map(|text| nonempty(text))
        .unwrap_or(if saml {
            "SAML user"
        } else if oidc {
            "OIDC user"
        } else {
            "Password user"
        });
    Some(json!({
        "requireLogin": settings["requireLogin"] != false,
        "authMode": if truthy(&settings["authMode"]) { settings["authMode"].clone() } else { json!("password") },
        "ssoType": if truthy(&settings["ssoType"]) { settings["ssoType"].clone() } else { json!("oidc") },
        "oidcConfigured": oidc_configured(settings)?,
        "oidcLoginLabel": label(&settings["oidcLoginLabel"], DEFAULT_OIDC_LABEL)?,
        "samlConfigured": saml_configured(settings),
        "samlLoginLabel": label(&settings["samlLoginLabel"], DEFAULT_SAML_LABEL)?,
        "hasPassword": truthy(&settings["password"]),
        "displayName": display_name,
        "loginMethod": login,
        "authenticated": authenticated,
        "oidcName": nonempty(&oidc_name),
        "oidcEmail": nonempty(&oidc_email),
        "oidcLogin": oidc,
        "samlName": nonempty(&saml_name),
        "samlEmail": nonempty(&saml_email),
        "samlLogin": saml,
    }))
}

async fn status(State(state): State<SessionState>, headers: HeaderMap) -> Response {
    let body = state
        .storage
        .read_settings()
        .await
        .and_then(|raw| status_body(&effective(raw), session(&state.storage, &headers)))
        .or_else(|| status_body(&Value::Null, None))
        .unwrap_or(Value::Null);
    reply(StatusCode::OK, body, false)
}

async fn logout() -> Response {
    let mut response = reply(StatusCode::OK, json!({ "success": true }), true);
    for cookie in [
        "auth_token=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
        "oidc_state=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
        "oidc_nonce=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
        "oidc_code_verifier=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
    ] {
        response
            .headers_mut()
            .append(header::SET_COOKIE, HeaderValue::from_static(cookie));
    }
    response
}

pub async fn authenticated(storage: &Storage, headers: &HeaderMap) -> bool {
    if crate::catalog_guard::valid_cli_token(storage, headers) {
        return true;
    }
    if session(storage, headers).is_some() {
        return true;
    }
    storage
        .read_settings()
        .await
        .is_some_and(|settings| settings["requireLogin"] == Value::Bool(false))
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .enumerate()
        .map(|(index, pair)| {
            if index == 0 {
                pair
            } else {
                pair.trim_start_matches(' ')
            }
        })
        .map(|pair| pair.split_once('=').unwrap_or((pair, "true")))
        .filter(|(name, _)| name == &"auth_token")
        .filter_map(|(_, value)| decode_cookie_value(value))
        .last()
}

fn decode_cookie_value(value: &str) -> Option<String> {
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' && !(bytes.next()?.is_ascii_hexdigit() && bytes.next()?.is_ascii_hexdigit())
        {
            return None;
        }
    }
    percent_decode_str(value)
        .decode_utf8()
        .ok()
        .map(|value| value.into_owned())
}

fn verify(token: &str, secret: &[u8]) -> Option<Value> {
    let (signed, signature) = token.rsplit_once('.')?;
    let (protected, payload) = signed.split_once('.')?;
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(protected).ok()?).ok()?;
    match header.get("crit") {
        None => (),
        Some(crit)
            if crit
                .as_array()
                .is_some_and(|items| items.as_slice() == [Value::String("b64".into())])
                && header["b64"] == true => {}
        _ => return None,
    }

    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let valid = match header["alg"].as_str()? {
        "HS256" => {
            let mut mac = Hmac::<Sha256>::new_from_slice(secret).ok()?;
            mac.update(signed.as_bytes());
            mac.verify_slice(&signature).is_ok()
        }
        "HS384" => {
            let mut mac = Hmac::<Sha384>::new_from_slice(secret).ok()?;
            mac.update(signed.as_bytes());
            mac.verify_slice(&signature).is_ok()
        }
        "HS512" => {
            let mut mac = Hmac::<Sha512>::new_from_slice(secret).ok()?;
            mac.update(signed.as_bytes());
            mac.verify_slice(&signature).is_ok()
        }
        _ => false,
    };
    if !valid {
        return None;
    }
    let payload: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    let claims = payload.as_object()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs_f64()
        .floor();
    if let Some(value) = claims.get("iat") {
        let _ = value.as_f64()?;
    }
    if claims
        .get("nbf")
        .map(Value::as_f64)
        .is_some_and(|value| !value.is_some_and(|n| n <= now))
        || claims
            .get("exp")
            .map(Value::as_f64)
            .is_some_and(|value| !value.is_some_and(|n| n > now))
    {
        return None;
    }
    Some(payload)
}
