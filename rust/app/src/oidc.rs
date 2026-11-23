use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

use crate::chat_output::js_text;
use crate::dashboard_session::{has_session, issued_cookie, secure_cookie};
use crate::provider_connections::truthy;
use crate::provider_http::Delivery;
use crate::provider_index::js_trim;
use crate::settings::reply;
use crate::storage::Storage;

const COOKIES: [&str; 3] = ["oidc_state", "oidc_nonce", "oidc_code_verifier"];
const CALLBACK: &str = "/api/auth/oidc/callback";
const DEFAULT_SCOPES: &str = "openid profile email";
const BODY_LIMIT: usize = 16 * 1024 * 1024;
const COOKIE_AGE: i64 = 600;
const TIMEOUT: Duration = Duration::from_secs(30);

struct Config {
    issuer: String,
    client_id: String,
    secret: String,
    scopes: String,
}

#[derive(Clone)]
struct Runtime {
    storage: Storage,
    http: reqwest::Client,
    protected: reqwest::Client,
}

pub fn router(storage: Storage) -> Router {
    let state = Runtime {
        storage,
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(crate::upstream::redirects())
            .timeout(TIMEOUT)
            .build()
            .expect("OIDC HTTP client"),
        protected: crate::provider_http::http_client(TIMEOUT)
            .build()
            .expect("protected OIDC HTTP client"),
    };
    Router::new()
        .route("/api/auth/oidc/start", get(start))
        .route(CALLBACK, get(callback))
        .route("/api/auth/oidc/test", post(test))
        .with_state(state)
}

impl Runtime {
    async fn request(
        &self,
        settings: &Value,
        method: reqwest::Method,
        url: Url,
    ) -> Result<reqwest::RequestBuilder, String> {
        if !url.username().is_empty() || url.password().is_some() {
            return Err("Invalid OIDC endpoint URL".into());
        }
        let unavailable = || "OIDC outbound proxy is unavailable".to_owned();
        let route = Delivery::select(&self.storage, settings, &Value::Null, url.as_str())
            .await
            .map_err(|_| unavailable())?;
        let client = if route.protected() {
            route
                .client(&self.protected, TIMEOUT)
                .ok_or_else(unavailable)?
        } else {
            self.http.clone()
        };
        Ok(client.request(method, url))
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

pub(crate) fn public_origin(request: &Request) -> Result<String, String> {
    for key in ["BASE_URL", "NEXT_PUBLIC_BASE_URL"] {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
        {
            return checked_origin(js_trim(&value));
        }
    }
    let headers = request.headers();
    let routed = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|peer| peer.0.ip().to_canonical().is_loopback());
    let forwarded_host = routed
        .then(|| header_text(headers, "x-forwarded-host"))
        .filter(|host| !host.is_empty());
    let host = forwarded_host
        .or_else(|| request.uri().authority().map(|value| value.as_str()))
        .unwrap_or_else(|| header_text(headers, "host"));
    let forwarded_proto = routed
        .then(|| header_text(headers, "x-forwarded-proto"))
        .filter(|proto| !proto.is_empty());
    let proto = forwarded_proto
        .or_else(|| request.uri().scheme_str())
        .unwrap_or("http");
    checked_origin(&format!("{}://{host}", proto.trim_end_matches(':')))
}

pub(crate) fn checked_origin(text: &str) -> Result<String, String> {
    let url = checked_url(text)?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Invalid URL".into());
    }
    Ok(text.trim_end_matches('/').to_owned())
}

fn checked_url(text: &str) -> Result<Url, String> {
    Url::parse(text)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
        .ok_or_else(|| "Invalid URL".into())
}

fn settings_text(settings: &Value, key: &str) -> String {
    settings[key].as_str().map(js_trim).unwrap_or("").to_owned()
}

fn config(settings: &Value) -> Option<Config> {
    if !matches!(settings["authMode"].as_str(), Some("oidc" | "both")) {
        return None;
    }
    let issuer = settings_text(settings, "oidcIssuerUrl")
        .trim_end_matches('/')
        .to_owned();
    let client_id = settings_text(settings, "oidcClientId");
    let secret = settings_text(settings, "oidcClientSecret");
    if issuer.is_empty() || client_id.is_empty() || secret.is_empty() {
        return None;
    }
    let scopes = settings_text(settings, "oidcScopes");
    Some(Config {
        issuer,
        client_id,
        secret,
        scopes: if scopes.is_empty() {
            DEFAULT_SCOPES.into()
        } else {
            scopes
        },
    })
}

async fn discovery(state: &Runtime, settings: &Value, issuer: &str) -> Result<Value, String> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        js_trim(issuer).trim_end_matches('/')
    );
    let response = state
        .request(settings, reqwest::Method::GET, checked_url(&url)?)
        .await?
        .send()
        .await
        .map_err(|_| "fetch failed".to_owned())?;
    if !response.status().is_success() {
        return Err(format!("Failed to load OIDC discovery document from {url}"));
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| "Invalid OIDC discovery document".to_owned())
}

fn random<const N: usize>() -> Result<String, String> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|_| "Could not initialize sign in".to_owned())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn append_cookies(response: &mut Response, values: impl IntoIterator<Item = HeaderValue>) {
    for value in values {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

fn clear_cookies(response: &mut Response) {
    append_cookies(
        response,
        COOKIES.into_iter().filter_map(|name| {
            HeaderValue::from_str(&format!(
                "{name}=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
            ))
            .ok()
        }),
    );
}

fn cookies(headers: &HeaderMap) -> [Option<String>; 3] {
    let mut result = [None, None, None];
    for header in headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
    {
        for (name, value) in header
            .split(';')
            .filter_map(|entry| js_trim(entry).split_once('='))
        {
            if let Some(index) = COOKIES.iter().position(|cookie| *cookie == name)
                && result[index].is_none()
            {
                result[index] = percent_decode_str(value)
                    .decode_utf8()
                    .ok()
                    .map(|text| text.into_owned());
            }
        }
    }
    result
}

pub(crate) fn redirect(location: &str) -> Response {
    let Ok(location) = HeaderValue::from_str(location) else {
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "Invalid redirect"}),
            false,
        );
    };
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    response.headers_mut().insert(header::LOCATION, location);
    response.headers_mut().insert(
        header::VARY,
        HeaderValue::from_static(
            "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch",
        ),
    );
    response
}

pub(crate) fn redirect_error(origin: &str, error: &str, clear: bool) -> Response {
    let mut response = match checked_url(&format!("{origin}/login")) {
        Ok(mut url) => {
            url.query_pairs_mut().append_pair("error", error);
            redirect(&url.as_str().replace('+', "%20"))
        }
        Err(_) => redirect("/login?error=oidc_invalid_origin"),
    };
    if clear {
        clear_cookies(&mut response);
    }
    response
}

async fn start(State(state): State<Runtime>, request: Request) -> Response {
    let origin = match public_origin(&request) {
        Ok(origin) => origin,
        Err(_) => return redirect_error("", "oidc_invalid_origin", false),
    };
    let settings = state.storage.read_settings().await.unwrap_or_default();
    let Some(config) = config(&settings) else {
        return redirect_error(&origin, "oidc_not_configured", false);
    };
    let secure = secure_cookie(request.headers());
    match prepare_authorization(&state, &settings, &config, &origin).await {
        Ok((url, values)) => {
            let mut response = redirect(url.as_str());
            let now = chrono::Utc::now() + chrono::Duration::seconds(COOKIE_AGE);
            let expires = now.format("%a, %d %b %Y %H:%M:%S GMT");
            let secure = if secure { "; Secure" } else { "" };
            append_cookies(&mut response, COOKIES.into_iter().zip(values).filter_map(|(name, value)| {
                HeaderValue::from_str(&format!("{name}={value}; Path=/; Expires={expires}; Max-Age={COOKIE_AGE}{secure}; HttpOnly; SameSite=lax")).ok()
            }));
            response
        }
        Err(reason) => redirect_error(&origin, &reason, false),
    }
}

async fn prepare_authorization(
    state: &Runtime,
    settings: &Value,
    config: &Config,
    origin: &str,
) -> Result<(Url, [String; 3]), String> {
    let profile = discovery(state, settings, &config.issuer).await?;
    let mut url = endpoint(&profile, "authorization_endpoint")?;
    let state = random::<16>()?;
    let nonce = random::<16>()?;
    let verifier = random::<32>()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let redirect = format!("{origin}{CALLBACK}");
    let args = [
        ("response_type", "code"),
        ("client_id", &config.client_id),
        ("redirect_uri", &redirect),
        ("scope", &config.scopes),
        ("state", &state),
        ("nonce", &nonce),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ];
    let original: Vec<_> = url.query_pairs().into_owned().collect();
    let (mut query, mut seen) = (Vec::new(), std::collections::HashSet::new());
    for (key, value) in &original {
        if let Some((key, replacement)) = args.iter().find(|(target, _)| key == target) {
            if seen.insert(*key) {
                query.push((*key, *replacement));
            }
        } else {
            query.push((key, value));
        }
    }
    query.extend(args.into_iter().filter(|(key, _)| !seen.contains(key)));
    url.query_pairs_mut().clear().extend_pairs(query);
    Ok((url, [state, nonce, verifier]))
}

fn endpoint(profile: &Value, key: &str) -> Result<Url, String> {
    checked_url(profile[key].as_str().unwrap_or_default())
}

fn query(request: &Request, name: &str) -> Option<String> {
    url::form_urlencoded::parse(request.uri().query()?.as_bytes())
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
        .filter(|value| !value.is_empty())
}

async fn callback(State(state): State<Runtime>, request: Request) -> Response {
    let origin = match public_origin(&request) {
        Ok(origin) => origin,
        Err(_) => return redirect_error("", "oidc_invalid_origin", false),
    };
    if let Some(error) = query(&request, "error") {
        return redirect_error(&origin, &error, false);
    }
    let (Some(code), Some(given_state)) = (query(&request, "code"), query(&request, "state"))
    else {
        return redirect_error(&origin, "oidc_missing_code", false);
    };
    let [Some(stored_state), Some(nonce), Some(verifier)] = cookies(request.headers()) else {
        return redirect_error(&origin, "oidc_invalid_state", true);
    };
    if nonce.is_empty()
        || verifier.is_empty()
        || !bool::from(stored_state.as_bytes().ct_eq(given_state.as_bytes()))
    {
        return redirect_error(&origin, "oidc_invalid_state", true);
    }
    let settings = state.storage.read_settings().await.unwrap_or_default();
    let Some(config) = config(&settings) else {
        return redirect_error(&origin, "oidc_not_configured", true);
    };
    let redirect_uri = format!("{origin}{CALLBACK}");
    let claims = match exchange(
        &state,
        &settings,
        &config,
        &code,
        &redirect_uri,
        &verifier,
        &nonce,
    )
    .await
    {
        Ok(claims) => claims,
        Err(error) => return redirect_error(&origin, &error, true),
    };
    let display = ["preferred_username", "email", "name", "given_name", "sub"]
        .into_iter()
        .find_map(|key| claims.get(key).filter(|value| truthy(value)).cloned())
        .unwrap_or(json!("OIDC user"));
    let claims = json!({
        "oidc": true,
        "oidcSub": claims["sub"],
        "oidcEmail": claims["email"],
        "oidcName": display,
    });
    let cookie = match issued_cookie(&state.storage, request.headers(), claims) {
        Ok(cookie) => cookie,
        Err(reason) => return redirect_error(&origin, &reason, true),
    };
    let mut response = redirect(&format!("{origin}/dashboard"));
    clear_cookies(&mut response);
    append_cookies(&mut response, [cookie]);
    response
}

async fn exchange(
    state: &Runtime,
    settings: &Value,
    config: &Config,
    code: &str,
    redirect: &str,
    verifier: &str,
    nonce: &str,
) -> Result<Value, String> {
    let profile = discovery(state, settings, &config.issuer).await?;
    let (status, token) = token_response(
        state,
        settings,
        endpoint(&profile, "token_endpoint")?,
        [
            ("grant_type", "authorization_code"),
            ("client_id", &config.client_id),
            ("code", code),
            ("redirect_uri", redirect),
            ("code_verifier", verifier),
            ("client_secret", &config.secret),
        ],
    )
    .await?;
    if !status.is_success() {
        let fallback = format!("OIDC token exchange failed ({})", status.as_u16());
        let reason = [config.secret.as_str(), code, verifier].into_iter().fold(
            description(&token).unwrap_or(&fallback).to_owned(),
            |reason, secret| reason.replace(secret, "[redacted]"),
        );
        return Err(reason);
    }
    let token = token["id_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "OIDC provider did not return an id_token".to_owned())?;
    let jwks = state
        .request(
            settings,
            reqwest::Method::GET,
            endpoint(&profile, "jwks_uri")?,
        )
        .await?
        .send()
        .await
        .ok()
        .and_then(|response| response.error_for_status().ok())
        .ok_or_else(|| "Failed to fetch OIDC public keys".to_owned())?
        .json::<Value>()
        .await
        .map_err(|_| "Invalid OIDC public keys".to_owned())?;
    let issuer = profile["issuer"]
        .as_str()
        .filter(|issuer| !issuer.is_empty())
        .unwrap_or(&config.issuer);
    verify_token(token, &jwks, issuer, &config.client_id, nonce)
}

async fn token_response<const N: usize>(
    state: &Runtime,
    settings: &Value,
    url: Url,
    form: [(&str, &str); N],
) -> Result<(reqwest::StatusCode, Value), String> {
    let response = state
        .request(settings, reqwest::Method::POST, url)
        .await?
        .form(form.as_slice())
        .send()
        .await
        .map_err(|_| "fetch failed".to_owned())?;
    Ok((
        response.status(),
        response.json::<Value>().await.unwrap_or_default(),
    ))
}

fn verify_token(
    token: &str,
    jwks: &Value,
    issuer: &str,
    client: &str,
    nonce: &str,
) -> Result<Value, String> {
    let header = decode_header(token).map_err(|_| "Invalid OIDC ID token".to_owned())?;
    if matches!(
        header.alg,
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
    ) {
        return Err("Invalid OIDC ID token".into());
    }
    let algorithm = serde_json::to_value(header.alg).unwrap_or_default();
    let keys = jwks["keys"]
        .as_array()
        .ok_or_else(|| "Invalid OIDC public keys".to_owned())?;
    let mut candidates = keys
        .iter()
        .filter(|key| {
            key["kty"] != "oct"
                && header.kid.as_deref().is_none_or(|id| key["kid"] == id)
                && key.get("alg").is_none_or(|name| name == &algorithm)
                && key.get("use").is_none_or(|use_| use_ == "sig")
                && key["key_ops"]
                    .as_array()
                    .is_none_or(|uses| uses.iter().any(|operation| operation == "verify"))
        })
        .filter_map(|key| {
            let key = serde_json::from_value::<jsonwebtoken::jwk::Jwk>(key.clone()).ok()?;
            DecodingKey::from_jwk(&key).ok()
        });
    let key = candidates
        .next()
        .ok_or_else(|| "No matching OIDC public key".to_owned())?;
    if candidates.next().is_some() {
        return Err("Multiple matching OIDC public keys".into());
    }
    let mut validation = Validation::new(header.alg);
    validation.leeway = 0;
    validation.validate_nbf = true;
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[client]);
    validation.set_required_spec_claims(&["iss", "aud", "exp"]);
    let claims = decode::<Value>(token, &key, &validation)
        .map_err(|_| "Invalid OIDC ID token".to_owned())?
        .claims;
    if !claims["nonce"]
        .as_str()
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(nonce.as_bytes())))
    {
        return Err("Invalid OIDC nonce".into());
    }
    Ok(claims)
}

fn description(value: &Value) -> Option<&str> {
    ["error_description", "error"]
        .into_iter()
        .find_map(|key| value[key].as_str().filter(|text| !text.is_empty()))
}

async fn test(State(state): State<Runtime>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let settings = state.storage.read_settings().await.unwrap_or_default();
    if settings["requireLogin"] != false && !has_session(&state.storage, &parts.headers) {
        return reply(
            StatusCode::UNAUTHORIZED,
            json!({ "error": "Unauthorized" }),
            false,
        );
    }
    let bytes = to_bytes(body, BODY_LIMIT).await.unwrap_or_default();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let result = test_result(
        &state,
        &settings,
        &body,
        Request::from_parts(parts, Body::empty()),
    )
    .await;
    match result {
        Ok(payload) => reply(StatusCode::OK, payload, false),
        Err((code, message)) => reply(code, json!({ "error": message }), false),
    }
}

async fn test_result(
    state: &Runtime,
    settings: &Value,
    body: &Value,
    request: Request,
) -> Result<Value, (StatusCode, String)> {
    let get = |param: &str, key: &str| {
        body.get(param)
            .filter(|value| truthy(value))
            .map(js_text)
            .map_or_else(|| settings_text(settings, key), |s| js_trim(&s).into())
    };
    let issuer = get("issuerUrl", "oidcIssuerUrl");
    let client = get("clientId", "oidcClientId");
    let secret = body.get("clientSecret").map(js_text).map_or_else(
        || settings_text(settings, "oidcClientSecret"),
        |s| js_trim(&s).into(),
    );
    let scopes = get("scopes", "oidcScopes");
    let scopes = if scopes.is_empty() {
        DEFAULT_SCOPES
    } else {
        &scopes
    };
    for (value, message) in [
        (&issuer, "Issuer URL is required"),
        (&client, "Client ID is required"),
    ] {
        if value.is_empty() {
            return Err((StatusCode::BAD_REQUEST, message.into()));
        }
    }
    let internal = |error: String| (StatusCode::INTERNAL_SERVER_ERROR, error);
    let origin = public_origin(&request).map_err(internal)?;
    let profile = discovery(state, settings, &issuer)
        .await
        .map_err(internal)?;
    let redirect = format!("{origin}{CALLBACK}");
    let (tested, valid, message) = if secret.is_empty() {
        (
            false,
            None,
            "No client secret was provided, so secret validation was skipped.".into(),
        )
    } else {
        let endpoint = endpoint(&profile, "token_endpoint").map_err(internal)?;
        let (status, data) = token_response(
            state,
            settings,
            endpoint,
            [
                ("grant_type", "authorization_code"),
                ("client_id", &client),
                ("client_secret", &secret),
                ("code", "__oidc_test_invalid_code__"),
                ("redirect_uri", &redirect),
                ("code_verifier", "__oidc_test_invalid_verifier__"),
            ],
        )
        .await
        .map_err(internal)?;
        let (valid, message) = probe(status, &data);
        (true, valid, message.replace(&secret, "[redacted]"))
    };
    let mut result = json!({
        "ok": valid != Some(false), "discoveryOk": true, "clientSecretTested": tested,
        "clientSecretValid": valid, "issuerUrl": issuer, "clientId": client,
        "scopes": scopes, "redirectUri": redirect,
        "authorizationEndpoint": profile["authorization_endpoint"].as_str().unwrap_or_default(),
        "tokenEndpoint": profile["token_endpoint"].as_str().unwrap_or_default(),
        "jwksUri": profile["jwks_uri"].as_str().unwrap_or_default(),
    });
    if valid == Some(false) {
        result["error"] = json!(format!(
            "Discovery loaded, but the client secret is not valid: {message}"
        ));
    } else {
        result["message"] = json!(message);
    }
    Ok(result)
}

fn probe(status: reqwest::StatusCode, body: &Value) -> (Option<bool>, String) {
    if status.is_success() {
        return (
            Some(true),
            "Client secret was accepted by the token endpoint.".into(),
        );
    }
    let error = body["error"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let description = description(body).unwrap_or_default();
    let lower = description.to_ascii_lowercase();
    if matches!(error.as_str(), "invalid_client" | "unauthorized_client")
        || lower.split_once("client").is_some_and(|(_, tail)| {
            ["invalid", "failed", "mismatch"]
                .iter()
                .any(|v| tail.contains(v))
        })
    {
        return (
            Some(false),
            if description.is_empty() {
                "Client secret is not valid.".into()
            } else {
                description.into()
            },
        );
    }
    if matches!(error.as_str(), "invalid_grant" | "invalid_code")
        || lower.contains("grant")
        || lower.contains("code")
    {
        return (Some(true), "Client secret was accepted; the token exchange failed only because the test authorization code is invalid.".into());
    }
    (
        None,
        if description.is_empty() {
            format!("Token endpoint responded with {}", status.as_u16())
        } else {
            description.into()
        },
    )
}
