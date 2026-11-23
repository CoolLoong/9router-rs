use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{SecondsFormat, Utc};
use reqwest::{Client, RequestBuilder};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use super::{
    RefreshError,
    flow::{ApiError, Credentials, Provider, generate_secret, nonempty},
    native_callback::{Callback, Kind, parameters, query_value},
    reply,
    store::{expiry, save},
};
use crate::{
    dashboard_session::authorization_owner,
    provider_http::{Delivery, http_client},
    provider_index::js_trim,
    storage::Storage,
};

const ORIGINS: &[&str] = &[
    "https://api.marscode.com",
    "https://api.trae.ai",
    "https://www.trae.ai",
    "https://www.marscode.com",
];
const GUIDANCE: &str = "/cloudide/api/v3/trae/GetLoginGuidance";
const EXCHANGE: &str = "/cloudide/api/v3/trae/oauth/ExchangeToken";
const PROFILE: &str = "/cloudide/api/v3/trae/GetUserInfo";
const REFRESH_URL: &str = "https://api.marscode.com/cloudide/api/v3/trae/oauth/ExchangeToken";
const CLIENT_ID: &str = "ono9krqynydwx5";
const USER_AGENT: &str = "Trae/1.0.0 antigravity-cockpit-tools";
const APP_VERSION: &str = "3.5.54";
const LIFETIME: i64 = 14 * 24 * 60 * 60;
const TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_LIMIT: usize = 256 * 1024;

static CLIENT: LazyLock<Client> = LazyLock::new(|| {
    http_client(TIMEOUT)
        .build()
        .expect("Trae OAuth HTTP client")
});

pub(super) fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/oauth/trae/{action}", any(dispatch))
        .with_state((Callback::new(storage.clone(), Kind::Trae), storage))
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
    let result = match (request.method(), action.as_str()) {
        (&Method::GET, "authorize") => {
            let redirect = query
                .get("redirect_uri")
                .filter(|value| !value.is_empty())
                .map_or("http://localhost:8080/callback", String::as_str);
            authorize(&storage, redirect).await
        }
        (&Method::GET, "start-proxy") => callback.start(owner).await,
        (&Method::GET, "poll-status") => {
            callback
                .poll(owner, query.get("state").map(String::as_str))
                .await
        }
        (&Method::GET, "stop-proxy") => Ok(callback.stop(owner).await),
        (&Method::GET, "ide-status") => Ok(callback.ide_status().await),
        (&Method::GET, "device-code") => {
            Err(ApiError::bad("Provider does not support device code flow"))
        }
        (&Method::POST, "exchange" | "register-session") => {
            let body = to_bytes(request.into_body(), 64 * 1024)
                .await
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .filter(Value::is_object);
            let Some(body) = body else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"error":"Invalid or empty request body"}),
                );
            };
            if action == "exchange" {
                match body["code"]
                    .as_str()
                    .map(js_trim)
                    .filter(|code| !code.is_empty())
                {
                    Some(code) => exchange(&storage, code).await.map(|conn| {
                        let mut summary = json!({"id":conn["id"],"provider":"trae"});
                        for field in ["email", "displayName"] {
                            if let Some(value) = conn.get(field) {
                                summary[field] = value.clone();
                            }
                        }
                        json!({"success":true,"connection":summary})
                    }),
                    None => Err(ApiError::bad("Missing token or callback URL")),
                }
            } else {
                let id = query
                    .get("state")
                    .map(String::as_str)
                    .or_else(|| nonempty(&body["state"]));
                callback.register(owner, id).await
            }
        }
        _ => Err(ApiError::bad("Unknown action")),
    };
    match result {
        Ok(body) => reply(StatusCode::OK, body),
        Err(error) => reply(error.status, json!({"error":error.message})),
    }
}

async fn authorize(storage: &Storage, redirect: &str) -> Result<Value, ApiError> {
    let parsed = Url::parse(redirect).map_err(|_| ApiError::bad("Invalid redirect URI"))?;
    if !parsed.has_host()
        || !matches!(parsed.scheme(), "http" | "https")
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(ApiError::bad("Invalid redirect URI"));
    }
    let state = Uuid::new_v4().to_string();
    let body = json!({"loginTraceID":state,"login_trace_id":state});
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(|| ApiError::failed("Could not load Trae authorization settings"))?;
    let mut authorization = None;
    for origin in &ORIGINS[..3] {
        if let Some(data) = oauth_post(storage, &settings, origin, GUIDANCE, &body, None).await
            && let Some(host) = extract(
                &data,
                &[
                    "/Result/LoginHost",
                    "/Result/loginHost",
                    "/Result/LoginURL",
                    "/result/loginHost",
                    "/data/Result/LoginHost",
                    "/data/loginHost",
                    "/LoginHost",
                    "/loginHost",
                ],
            )
            && let Some(url) = login_url(&host)
        {
            authorization = Some(url);
            break;
        }
    }
    let mut url = authorization.ok_or_else(|| ApiError::failed("Trae GetLoginGuidance failed"))?;
    let device = Uuid::new_v4().to_string();
    url.set_path("/authorization");
    url.set_query(None);
    url.set_fragment(None);
    url.query_pairs_mut().extend_pairs([
        ("login_version", "1"),
        ("auth_from", "trae"),
        ("login_channel", "native_ide"),
        ("plugin_version", "local"),
        ("auth_type", "local"),
        ("client_id", CLIENT_ID),
        ("redirect", "0"),
        ("login_trace_id", &state),
        ("auth_callback_url", redirect),
        ("machine_id", &device),
        ("device_id", "0"),
        ("x_device_id", "0"),
        ("x_machine_id", &device),
        ("x_device_brand", "unknown"),
        ("x_device_type", "unknown"),
        ("x_os_version", "unknown"),
        ("x_env", ""),
        ("x_app_version", APP_VERSION),
        ("x_app_type", "stable"),
    ]);
    let verifier = generate_secret(32)?;
    Ok(json!({
        "authUrl":url.as_str(),"state":state,"codeVerifier":verifier,
        "codeChallenge":URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        "redirectUri":redirect,"flowType":"authorization_code","callbackPath":"/callback",
    }))
}

fn login_url(host: &str) -> Option<Url> {
    let normalized;
    let raw = if host.starts_with("https://") {
        host
    } else {
        normalized = format!("https://{}", host.trim_start_matches('/'));
        &normalized
    };
    let url = Url::parse(raw).ok()?;
    let domain = url.host_str()?;
    (url.scheme() == "https"
        && ["trae.ai", "marscode.com"].iter().any(|suffix| {
            domain == *suffix
                || domain
                    .strip_suffix(suffix)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

pub(super) async fn exchange(storage: &Storage, raw: &str) -> Result<Value, ApiError> {
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(|| ApiError::failed("Could not load Trae authorization settings"))?;
    let callback = raw.contains(['?', '=', '&'])
        && (raw.contains("refreshToken")
            || raw.contains("refresh_token")
            || raw.contains("RefreshToken"));
    let (access, refresh, expires, method) = if callback {
        let fragment = raw
            .strip_prefix('#')
            .unwrap_or_else(|| raw.split_once('?').map_or(raw, |(_, query)| query));
        let query = parameters(fragment);
        if query_value(&query, &["error", "error_code", "errorCode"]).is_some() {
            return Err(ApiError::failed("Trae authorisation failed"));
        }
        let refresh = query_value(&query, &["refreshToken", "refresh_token", "RefreshToken"])
            .ok_or_else(|| ApiError::failed("Trae callback missing refreshToken"))?;
        query_value(
            &query,
            &[
                "loginHost",
                "login_host",
                "LoginHost",
                "host",
                "consoleHost",
            ],
        )
        .ok_or_else(|| ApiError::failed("Trae callback missing loginHost"))?;
        let cloudide = query_value(
            &query,
            &[
                "x-cloudide-token",
                "xCloudideToken",
                "accessToken",
                "access_token",
                "token",
            ],
        )
        .map(token_header)
        .transpose()
        .map_err(|_| ApiError::bad("Invalid Trae callback token"))?;
        let body = exchange_body(refresh);
        let mut tokens = None;
        for origin in ORIGINS {
            if let Some(data) = oauth_post(
                storage,
                &settings,
                origin,
                EXCHANGE,
                &body,
                cloudide.as_ref(),
            )
            .await
                && let Some(access) = extract(
                    &data,
                    &[
                        "/Result/AccessToken",
                        "/Result/accessToken",
                        "/result/access_token",
                        "/accessToken",
                    ],
                )
            {
                let refresh = extract(
                    &data,
                    &[
                        "/Result/RefreshToken",
                        "/result/refresh_token",
                        "/refreshToken",
                    ],
                )
                .unwrap_or(Cow::Borrowed(refresh));
                let expires = extract(
                    &data,
                    &[
                        "/Result/ExpiresAt",
                        "/Result/expiresAt",
                        "/result/expires_at",
                        "/expiresAt",
                    ],
                )
                .and_then(|raw| raw.parse::<i64>().ok())
                .map_or(LIFETIME, |date| {
                    date.saturating_sub(Utc::now().timestamp()).max(60)
                });
                tokens = Some((
                    access.into_owned(),
                    Some(refresh.into_owned()),
                    expires,
                    "oauth",
                ));
                break;
            }
        }
        tokens.ok_or_else(|| ApiError::failed("Trae ExchangeToken failed"))?
    } else {
        let clean = raw
            .find(char::is_whitespace)
            .and_then(|separator| {
                let prefix = &raw[..separator];
                (prefix.eq_ignore_ascii_case("cloud-ide-jwt")
                    || prefix.eq_ignore_ascii_case("bearer"))
                .then(|| js_trim(&raw[separator..]))
            })
            .unwrap_or(raw);
        (clean.to_owned(), None, LIFETIME, "imported")
    };
    let token = token_header(&access)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad("Invalid Trae access token"))?;

    let mut profile = Value::Null;
    for origin in ORIGINS {
        if let Some(body) = oauth_post(
            storage,
            &settings,
            origin,
            PROFILE,
            &json!({}),
            Some(&token),
        )
        .await
        {
            profile = body;
            break;
        }
    }
    let email = extract(
        &profile,
        &[
            "/Result/NonPlainTextEmail",
            "/Result/Email",
            "/Result/email",
            "/email",
            "/data/email",
        ],
    );
    let name = extract(
        &profile,
        &[
            "/Result/ScreenName",
            "/Result/Nickname",
            "/Result/Name",
            "/result/nickname",
            "/nickname",
            "/name",
        ],
    );
    let ai_region = extract(
        &profile,
        &["/Result/AIRegion", "/Result/aiRegion", "/aiRegion"],
    )
    .unwrap_or(Cow::Borrowed("US-East"));
    let region = extract(&profile, &["/Result/Region", "/Result/region", "/region"])
        .unwrap_or(Cow::Borrowed(&ai_region));
    let tenant = extract(
        &profile,
        &["/Result/TenantID", "/Result/tenantId", "/tenantId"],
    )
    .unwrap_or(Cow::Borrowed("marscode"));
    let user_id = extract(&profile, &["/Result/UserID", "/Result/userId", "/userId"])
        .unwrap_or(Cow::Borrowed(""));
    let scope_region = ai_region.to_ascii_lowercase();
    let scope = if scope_region == "sg" || scope_region.contains("singapore") {
        "marscode-sg"
    } else if scope_region.contains("cn") || scope_region.contains("china") {
        "marscode-cn"
    } else {
        "marscode-us"
    };
    let mut fields = Map::from_iter([
        ("accessToken".into(), json!(access)),
        ("refreshToken".into(), json!(refresh)),
        ("expiresIn".into(), json!(expires)),
        (
            "providerSpecificData".into(),
            json!({
                "authMethod":method,"aiRegion":ai_region,"region":region,"tenant":tenant,
                "userId":user_id,"scope":scope,"webId":"","bizUserId":"","userUniqueId":"",
                "appLanguage":"en","appVersion":APP_VERSION,
                "userRegion":if ai_region == "SG" {"SG"} else {"US"},"userIdentity":"Free",
            }),
        ),
    ]);
    for (key, value) in [("email", email), ("displayName", name)] {
        if let Some(value) = value {
            fields.insert(key.into(), json!(value));
        }
    }
    save(storage, Provider::Trae, Credentials(fields)).await
}

pub(super) async fn refresh(
    storage: &Storage,
    account: &Value,
) -> Result<Map<String, Value>, RefreshError> {
    let refresh =
        nonempty(&account["refreshToken"]).ok_or(RefreshError::ReauthenticationRequired)?;
    let settings = storage.read_settings().await.ok_or(RefreshError::Storage)?;
    let delivery = Delivery::select(storage, &settings, account, REFRESH_URL)
        .await
        .map_err(|_| RefreshError::ProviderUnavailable)?;
    let client = delivery
        .client(&CLIENT, TIMEOUT)
        .ok_or(RefreshError::ProviderUnavailable)?;
    let data = read(request(&client, REFRESH_URL, &exchange_body(refresh)))
        .await
        .ok_or(RefreshError::ProviderUnavailable)?;
    let result = pick(&data, &["/Result", "/result"]).unwrap_or(&data);
    let access = pick(result, &["/AccessToken", "/accessToken"])
        .and_then(nonempty)
        .ok_or(RefreshError::ProviderUnavailable)?;
    let refresh = pick(result, &["/RefreshToken", "/refreshToken"])
        .and_then(nonempty)
        .unwrap_or(refresh);
    let now = Utc::now();
    let mut patch = Map::from_iter([
        ("accessToken".into(), json!(access)),
        ("refreshToken".into(), json!(refresh)),
        (
            "lastRefreshAt".into(),
            json!(now.to_rfc3339_opts(SecondsFormat::Millis, true)),
        ),
    ]);
    if let Some(declared) = pick(result, &["/ExpiresAt", "/expiresAt"]) {
        let seconds = if let Some(epoch) = declared.as_i64() {
            Some(epoch.saturating_sub(now.timestamp()).max(1))
        } else {
            declared
                .as_str()
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .map(|date| date.signed_duration_since(now).num_seconds())
                .filter(|delta| *delta > 0)
        };
        if let Some(seconds) = seconds {
            let seconds = json!(seconds);
            let date =
                expiry(now, Some(&seconds)).map_err(|_| RefreshError::ProviderUnavailable)?;
            patch.insert("expiresIn".into(), seconds);
            patch.insert("expiresAt".into(), date);
        }
    }
    Ok(patch)
}

fn token_header(token: &str) -> Result<header::HeaderValue, header::InvalidHeaderValue> {
    let mut value = header::HeaderValue::from_str(token)?;
    value.set_sensitive(true);
    Ok(value)
}

fn exchange_body(refresh: &str) -> Value {
    json!({"ClientID":CLIENT_ID,"RefreshToken":refresh,"ClientSecret":"-","UserID":""})
}

fn request(client: &Client, endpoint: &str, body: &Value) -> RequestBuilder {
    client
        .post(endpoint)
        .header(header::ACCEPT, "application/json")
        .header(header::USER_AGENT, USER_AGENT)
        .json(body)
}

async fn oauth_post(
    storage: &Storage,
    settings: &Value,
    origin: &str,
    path: &str,
    body: &Value,
    token: Option<&header::HeaderValue>,
) -> Option<Value> {
    let endpoint = format!("{origin}{path}");
    let delivery = Delivery::select(storage, settings, &Value::Null, &endpoint)
        .await
        .ok()?;
    let client = delivery.client(&CLIENT, TIMEOUT)?;
    let endpoint = if delivery.use_fixture() {
        format!("http://127.0.0.1:52989/trae-fixture{path}")
    } else {
        endpoint
    };
    let mut request = request(&client, &endpoint, body);
    if let Some(token) = token {
        request = request.header("x-cloudide-token", token.clone());
    }
    read(request).await
}

async fn read(request: RequestBuilder) -> Option<Value> {
    let mut response = request.send().await.ok()?.error_for_status().ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > RESPONSE_LIMIT - data.len() {
            return None;
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).ok()
}

fn pick<'a>(object: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| {
        object
            .pointer(key)
            .filter(|value| crate::provider_connections::truthy(value))
    })
}

fn extract<'a>(object: &'a Value, keys: &[&str]) -> Option<Cow<'a, str>> {
    keys.iter().find_map(|key| match object.pointer(key)? {
        Value::String(text) if !js_trim(text).is_empty() => Some(Cow::Borrowed(js_trim(text))),
        Value::Number(value) => Some(Cow::Owned(value.to_string())),
        _ => None,
    })
}
