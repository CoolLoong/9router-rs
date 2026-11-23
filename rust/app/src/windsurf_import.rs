use std::sync::LazyLock;
use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use super::{
    flow::{ApiError, generate_secret, nonempty},
    native_callback::{Callback, Kind, parameters, query_value},
    reply, save_imported,
};
use crate::dashboard_session::authorization_owner;
use crate::provider_connections::object;
use crate::provider_http::{Delivery, http_client};
use crate::provider_index::js_trim;
use crate::storage::Storage;

const CLIENT_ID: &str = "3GUryQ7ldAeKEuD2obYnppsnmj58eP5u";
const AUTH_URL: &str = "https://www.windsurf.com/windsurf/signin";
const REGISTER: &str = "https://register.windsurf.com";
const API_SERVER: &str = "https://server.codeium.com";
const SELF_SERVE: &str = "https://server.self-serve.windsurf.com";
const REGISTER_USER: &str = "RegisterUser";
const ONE_TIME_AUTH: &str = "GetOneTimeAuthToken";
const CURRENT_USER: &str = "GetCurrentUser";
const USER_AGENT: &str = "antigravity-cockpit-tools";
const RPC: &str = "/exa.seat_management_pb.SeatManagementService/";
const KEY_START: &str = "sk-ws-";
const INPUT_LIMIT: usize = 64 * 1024;
const RESPONSE_LIMIT: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

static CLIENT: LazyLock<Option<Client>> = LazyLock::new(|| http_client(TIMEOUT).build().ok());

pub(super) fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/oauth/windsurf/{action}", any(dispatch))
        .with_state((Callback::new(storage.clone(), Kind::Windsurf), storage))
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
        (&Method::GET, "authorize") => authorize(
            query
                .get("redirect_uri")
                .map(String::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("http://localhost:8080/callback"),
        ),
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
            let body = to_bytes(request.into_body(), INPUT_LIMIT)
                .await
                .ok()
                .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
                .filter(Value::is_object);
            let Some(body) = body else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"error":"Invalid or empty request body"}),
                );
            };
            if action == "exchange" {
                match nonempty(&body["code"])
                    .map(js_trim)
                    .filter(|code| !code.is_empty())
                {
                    Some(code) => exchange(&storage, code, nonempty(&body["state"]))
                        .await
                        .map(|connection| {
                            let mut summary = json!({"id":connection["id"],"provider":"windsurf"});
                            for field in ["email", "displayName"] {
                                if let Some(value) = connection.get(field) {
                                    summary[field] = value.clone();
                                }
                            }
                            json!({"success":true,"connection":summary})
                        }),
                    None => Err(ApiError::bad("Missing token or callback URL")),
                }
            } else {
                let state = query
                    .get("state")
                    .map(String::as_str)
                    .filter(|id| !id.is_empty())
                    .or_else(|| nonempty(&body["state"]));
                callback.register(owner, state).await
            }
        }
        _ => Err(ApiError::bad("Unknown action")),
    };
    match result {
        Ok(data) => reply(StatusCode::OK, data),
        Err(error) => reply(error.status, json!({"error":error.message})),
    }
}

fn authorize(redirect: &str) -> Result<Value, ApiError> {
    let callback = Url::parse(redirect).map_err(|_| ApiError::bad("Invalid redirect URI"))?;
    if callback.scheme() != "http"
        || !matches!(callback.host_str(), Some("127.0.0.1" | "localhost"))
        || !(callback.path() == "/callback" || callback.path() == Kind::Windsurf.path())
        || callback.fragment().is_some()
        || callback.query().is_some()
        || !callback.username().is_empty()
        || callback.password().is_some()
    {
        return Err(ApiError::bad("Invalid redirect URI"));
    }
    let state = generate_secret(32)?;
    let verifier = generate_secret(32)?;
    let mut url = Url::parse(AUTH_URL).expect("Windsurf sign-in URL");
    url.query_pairs_mut().extend_pairs([
        ("response_type", "token"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect),
        ("state", state.as_str()),
        ("prompt", "login"),
        ("redirect_parameters_type", "query"),
        ("workflow", "onboarding"),
    ]);
    Ok(json!({
        "authUrl":url.as_str(),"state":state,"codeVerifier":verifier,
        "codeChallenge":URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        "redirectUri":redirect,"flowType":"authorization_code","callbackPath":Kind::Windsurf.path(),
    }))
}

async fn exchange(storage: &Storage, input: &str, state: Option<&str>) -> Result<Value, ApiError> {
    let key = strip_bearer(input);
    if key.starts_with(KEY_START) {
        if key.len() == KEY_START.len() || !valid_token(key) || key.contains("access_token=") {
            return Err(ApiError::bad("Invalid Windsurf API key"));
        }
        return save(storage, key, API_SERVER, "imported", None, None).await;
    }
    if input.contains('?') || input.contains("access_token=") {
        let query = input
            .split_once('?')
            .map_or(input, |(_, raw)| raw)
            .trim_start_matches('#');
        return parse_callback(storage, query, state, false).await;
    }
    if key.starts_with("eyJ")
        && key.split('.').count() == 3
        && key.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
    {
        return register(storage, key, "imported").await;
    }
    Err(ApiError::bad(
        "Enter a Windsurf API key beginning with sk-ws-",
    ))
}

pub(super) async fn exchange_callback(
    storage: &Storage,
    raw: &str,
    state: &str,
) -> Result<Value, ApiError> {
    parse_callback(storage, raw, Some(state), true).await
}

async fn parse_callback(
    storage: &Storage,
    raw: &str,
    expected: Option<&str>,
    require_state: bool,
) -> Result<Value, ApiError> {
    let params = parameters(raw);
    if query_value(&params, &["error"]).is_some() {
        return Err(ApiError::bad("Windsurf authorization failed"));
    }
    let token = query_value(&params, &["access_token", "token"])
        .filter(|token| valid_token(token))
        .ok_or_else(|| ApiError::bad("Windsurf callback missing access_token"))?;
    let state = query_value(&params, &["state"]);
    if require_state && state != expected
        || expected
            .zip(state)
            .is_some_and(|(expected, state)| expected != state)
    {
        return Err(ApiError::bad("Windsurf callback state mismatch"));
    }
    register(storage, token, "oauth").await
}

async fn register(
    storage: &Storage,
    firebase: &str,
    method: &'static str,
) -> Result<Value, ApiError> {
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(|| ApiError::failed("Could not load Windsurf authorization settings"))?;
    let response = seat_request(
        storage,
        &settings,
        REGISTER,
        REGISTER_USER,
        &json!({"firebase_id_token":firebase}),
    )
    .await
    .ok_or_else(|| ApiError::failed("Windsurf RegisterUser failed"))?;
    let key = field(&response, &["apiKey", "api_key"])
        .filter(|key| valid_token(key))
        .ok_or_else(|| ApiError::failed("Windsurf RegisterUser missing apiKey"))?;
    let origin = api_server(field(&response, &["apiServerUrl", "api_server_url"]))
        .ok_or_else(|| ApiError::failed("Windsurf returned an unsupported API server"))?;
    let profile = profile(storage, &settings, origin, firebase).await;
    save(storage, key, origin, method, Some(firebase), profile).await
}

pub(crate) fn api_server(value: Option<&str>) -> Option<&'static str> {
    let Some(value) = value else {
        return Some(API_SERVER);
    };
    let url = Url::parse(value).ok()?;
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    match url.host_str()? {
        "server.codeium.com" => Some(API_SERVER),
        "server.self-serve.windsurf.com" => Some(SELF_SERVE),
        _ => None,
    }
}

async fn profile(
    storage: &Storage,
    settings: &Value,
    origin: &'static str,
    firebase: &str,
) -> Option<Value> {
    let response = seat_request(
        storage,
        settings,
        origin,
        ONE_TIME_AUTH,
        &json!({"firebaseIdToken":firebase}),
    )
    .await?;
    let auth = field(&response, &["authToken", "auth_token"]).filter(|token| valid_token(token))?;
    seat_request(
        storage,
        settings,
        origin,
        CURRENT_USER,
        &json!({"authToken":auth,"includeSubscription":true}),
    )
    .await
}

async fn seat_request(
    storage: &Storage,
    settings: &Value,
    origin: &'static str,
    method: &'static str,
    body: &Value,
) -> Option<Value> {
    let endpoint = format!("{origin}{RPC}{method}");
    let delivery = Delivery::select(storage, settings, &Value::Null, &endpoint)
        .await
        .ok()?;
    let client = delivery.client(CLIENT.as_ref()?, TIMEOUT)?;
    let endpoint = if delivery.use_fixture() {
        format!("http://127.0.0.1:52989/windsurf{RPC}{method}")
    } else {
        endpoint
    };
    let mut response = client
        .post(endpoint)
        .header("accept", "application/json")
        .header("user-agent", USER_AGENT)
        .json(body)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > RESPONSE_LIMIT - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}

async fn save(
    storage: &Storage,
    key: &str,
    origin: &'static str,
    method: &'static str,
    firebase: Option<&str>,
    profile: Option<Value>,
) -> Result<Value, ApiError> {
    let mut fields = object(json!({
        "authType":"api_key","accessToken":key,
        "providerSpecificData":{"authMethod":method,"apiServerUrl":origin,"firebaseIdToken":firebase},
    }));
    if let Some(profile) = profile {
        let user = profile
            .get("user")
            .filter(|user| user.is_object())
            .unwrap_or(&profile);
        for (source, target) in [("email", "email"), ("name", "displayName")] {
            if let Some(value) = field(user, &[source]) {
                fields.insert(target.into(), json!(value));
            }
        }
    }
    save_imported(storage, "windsurf", fields)
        .await
        .map_err(|_| ApiError::failed("Could not save the provider connection"))
}

fn field<'a>(body: &'a Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|key| {
        nonempty(&body[*key])
            .map(js_trim)
            .filter(|value| !value.is_empty())
    })
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= INPUT_LIMIT
        && !value.contains(['?', '#'])
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn strip_bearer(input: &str) -> &str {
    input
        .get(..6)
        .filter(|prefix| prefix.eq_ignore_ascii_case("Bearer"))
        .and_then(|_| input.get(6..))
        .and_then(|rest| {
            let trimmed = js_trim(rest);
            (trimmed != rest).then_some(trimmed)
        })
        .unwrap_or(input)
}
