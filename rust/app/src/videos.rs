use std::collections::HashSet;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use url::Url;

use crate::chat_accounts::{self, Retry, Selection};
use crate::chat_output::{error, js_text};
use crate::cli_video::{component, redact_bearer};
use crate::observed_response::into_http;
use crate::provider_connections::{self, truthy};
use crate::provider_http::Delivery;
use crate::provider_oauth;
use crate::proxy_transport::{self, Route, Selection as ProxySelection};
use crate::storage::Storage;
use crate::{catalog_guard, key_policy, model_resolution};

const BODY_LIMIT: usize = 128 * 1024 * 1024;
const DEFAULT_PROVIDER: &str = "xai";
const XAI: &str = "https://api.x.ai/v1/videos";
const XAI_FIXTURE: &str = "http://127.0.0.1:52989/xai/v1/videos";
const OPENROUTER: &str = "https://openrouter.ai/api/v1/videos";
const OPENROUTER_FIXTURE: &str = "http://127.0.0.1:52989/openrouter/api/v1/videos";
const VARY: &str =
    "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch";
const VARY_ENCODING: &str = "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding";
const ACTIONS: &[&str] = &["generations", "edits", "extensions"];

#[derive(Clone)]
struct VideoState {
    storage: Storage,
    client: reqwest::Client,
    protected: reqwest::Client,
    timeout: Duration,
    fixture: bool,
}

pub fn router(storage: Storage) -> Router {
    let timeout_ms = std::env::var("VIDEO_FETCH_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(120_000);
    let timeout = Duration::from_millis(timeout_ms);
    let state = VideoState {
        storage,
        client: reqwest::Client::builder()
            .timeout(timeout)
            .retry(reqwest::retry::never())
            .redirect(crate::upstream::redirects())
            .build()
            .expect("video upstream HTTP client"),
        protected: protected_client(timeout)
            .build()
            .expect("protected video HTTP client"),
        timeout,
        fixture: std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"),
    };
    let mut router = Router::new();
    for prefix in ["/v1", "/api/v1", "/v1/v1"] {
        for action in ACTIONS {
            router = router.route(
                &format!("{prefix}/videos/{action}"),
                post(create).options(create_options),
            );
        }
        router = router.route(
            &format!("{prefix}/videos/{{id}}"),
            get(poll).options(poll_options),
        );
    }
    router.with_state(state)
}

async fn create_options() -> Response {
    options("POST, OPTIONS")
}

async fn poll_options() -> Response {
    options("GET, OPTIONS")
}

fn options(methods: &str) -> Response {
    into_http(Ok(json!({
        "status": 200,
        "headers": {
            "access-control-allow-origin": "*",
            "access-control-allow-methods": methods,
            "access-control-allow-headers": "*",
        },
        "body": "",
    })))
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .filter(|value| !value.is_empty())
}

async fn authorize(
    storage: &Storage,
    settings: &Value,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, &'static str)> {
    if !settings.get("requireApiKey").is_none_or(truthy) {
        return Ok(());
    }
    let key = header_value(headers, "authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| header_value(headers, "x-api-key"));
    let Some(key) = key.filter(|key| !key.is_empty()) else {
        return Err((StatusCode::UNAUTHORIZED, "Missing API key"));
    };
    if !storage.api_key_is_active(key.to_owned()).await {
        return Err((StatusCode::UNAUTHORIZED, "Invalid API key"));
    }
    Ok(())
}

fn video_provider(provider: &str) -> bool {
    matches!(
        provider,
        "xai" | "openrouter" | "vertex" | "grok-web" | "muse-web" | "veoaifree-web" | "uc-web"
    )
}

enum CreatePlan {
    Resolved(String, String),
    Rejected(StatusCode, String),
}

async fn create(State(state): State<VideoState>, request: Request) -> Response {
    let Some(settings) = state.storage.read_settings().await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to read settings");
    };
    if let Err((status, message)) = authorize(&state.storage, &settings, request.headers()).await {
        return error(status, message);
    }
    let policy_key = catalog_guard::api_key(&request).filter(|key| !key.is_empty());
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, BODY_LIMIT).await else {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Video request body too large",
        );
    };
    let content_type = header_value(&parts.headers, "content-type");
    let is_json = content_type.is_some_and(|mime| mime.contains("application/json"));
    let mut forwarded = if is_json {
        match String::from_utf8_lossy(&bytes) {
            std::borrow::Cow::Borrowed(value) if !value.starts_with('\u{feff}') => bytes.clone(),
            body => {
                Bytes::copy_from_slice(body.strip_prefix('\u{feff}').unwrap_or(&body).as_bytes())
            }
        }
    } else {
        bytes.clone()
    };
    let parsed = if is_json {
        match serde_json::from_slice::<Value>(&forwarded) {
            Ok(value) => Some(value),
            Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid JSON body"),
        }
    } else {
        None
    };
    let requested = parsed
        .as_ref()
        .and_then(|value| value.get("model"))
        .filter(|value| truthy(value))
        .map(js_text);
    let lookup = requested.clone();
    let plan = state
        .storage
        .run(false, move |db| {
            let (provider, model) = if let Some(name) = &lookup {
                let Some((provider, model)) = model_resolution::get_model_info(db, name)? else {
                    return Ok(CreatePlan::Rejected(
                        StatusCode::BAD_REQUEST,
                        "Combos are not supported for video generation".into(),
                    ));
                };
                if video_provider(&provider) {
                    (provider, model)
                } else if !name.contains('/') {
                    (DEFAULT_PROVIDER.into(), name.clone())
                } else {
                    return Ok(CreatePlan::Rejected(
                        StatusCode::BAD_REQUEST,
                        format!("Provider '{provider}' does not support video generation"),
                    ));
                }
            } else {
                (DEFAULT_PROVIDER.into(), String::new())
            };
            if let Some(key) = &policy_key
                && !key_policy::permits_resolved(db, key, &provider, &model)?
            {
                let label = lookup
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| format!("{provider}/{model}"));
                return Ok(CreatePlan::Rejected(
                    StatusCode::FORBIDDEN,
                    format!("This API key is not allowed to use model \"{label}\""),
                ));
            }
            Ok(CreatePlan::Resolved(provider, model))
        })
        .await;
    let (provider, model) = match plan {
        Ok(CreatePlan::Resolved(provider, model)) => (provider, model),
        Ok(CreatePlan::Rejected(status, message)) => return error(status, &message),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to resolve model"),
    };
    if provider == "veoaifree-web" {
        let action = parts.uri.path().rsplit('/').next().unwrap_or_default();
        let Some(value) = parsed
            .as_ref()
            .filter(|_| matches!(action, "generations" | "edits"))
        else {
            return error(
                StatusCode::BAD_REQUEST,
                "Veo AI Free video accepts JSON generations and edits only",
            );
        };
        return match crate::veoaifree_media::video(&state.storage, &settings, &model, value).await {
            Ok(value) => into_http(Ok(json!({
                "status":200,"headers":{"content-type":"application/json","access-control-allow-origin":"*"},
                "body":value.to_string(),
            }))),
            Err(failure) => error(failure.status, &failure.message),
        };
    }
    if let Some(mut value) = parsed
        && !model.is_empty()
        && value["model"] != model
    {
        value["model"] = json!(model);
        forwarded = Bytes::from(value.to_string());
    }
    let action = parts.uri.path().rsplit('/').next().unwrap_or_default();
    let web_media = matches!(provider.as_str(), "uc-web" | "muse-web" | "grok-web");
    if web_media && (!matches!(action, "generations" | "edits") || !is_json) {
        return error(
            StatusCode::BAD_REQUEST,
            &format!("{provider} video accepts JSON generations and edits only"),
        );
    }
    let preferred = header_value(&parts.headers, "x-connection-id");
    let job = Job::Create {
        action,
        content_type,
        idempotency_key: header_value(&parts.headers, "idempotency-key"),
        body: forwarded,
    };

    let mut excluded = HashSet::new();
    let mut last = None;
    loop {
        let account =
            match chat_accounts::select(&state.storage, &provider, &model, &excluded, preferred)
                .await
            {
                Ok(Selection::Ready(account)) => account,
                Ok(Selection::CoolingDown(retry)) => {
                    return unavailable(retry, last, &provider, &model);
                }
                Ok(Selection::Empty) => {
                    return last.map_or_else(|| no_credentials(&provider), Failure::response);
                }
                Err(_) => {
                    return error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to select provider credentials",
                    );
                }
            };
        let Some(connection) = account["id"].as_str() else {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid provider connection",
            );
        };
        if web_media {
            let Job::Create { body, .. } = &job else {
                unreachable!("Web video creation body");
            };
            let Ok(value) = serde_json::from_slice::<Value>(body) else {
                return error(StatusCode::BAD_REQUEST, "Invalid web video request body");
            };
            let result = match provider.as_str() {
                "uc-web" => {
                    crate::uc_media::video(&state.storage, &settings, &account, &model, &value)
                        .await
                        .map_err(|error| (error.status, error.message))
                }
                "grok-web" => crate::grok_web_media::video(
                    &state.storage,
                    &settings,
                    &account,
                    &model,
                    &value,
                )
                .await
                .map_err(|error| (error.status, error.message)),
                _ => crate::muse_web::video(&state.storage, &settings, &account, &model, &value)
                    .await
                    .map_err(|error| (error.status, error.message)),
            };
            return match result {
                Ok(value) => {
                    let _ = chat_accounts::mark_success(&state.storage, connection, &model).await;
                    let mut response = into_http(Ok(json!({
                        "status":200,
                        "headers":{"content-type":"application/json","access-control-allow-origin":"*"},
                        "body":value.to_string(),
                    })));
                    set_connection(&mut response, connection);
                    response
                }
                Err((status, message)) => error(status, &message),
            };
        }
        let egress = match video_egress(&state, &settings, &provider, &account, &job).await {
            Ok(selected) => selected,
            Err(failure) => return failure.response(),
        };
        let protected = egress.protected;
        match send_with_refresh(&state, &provider, &account, &job, egress).await {
            Ok(mut response) => {
                let _ = chat_accounts::mark_success(&state.storage, connection, &model).await;
                set_connection(&mut response, connection);
                return response;
            }
            Err(failure) if protected => return failure.response(),
            Err(failure) => {
                let retry =
                    mark_failure(&state.storage, &provider, connection, &model, &failure).await;
                if !matches!(
                    failure.status,
                    StatusCode::UNAUTHORIZED
                        | StatusCode::FORBIDDEN
                        | StatusCode::TOO_MANY_REQUESTS
                ) || !retry
                {
                    return failure.response();
                }
                excluded.insert(connection.to_owned());
                last = Some(failure);
            }
        }
    }
}

async fn poll(
    State(state): State<VideoState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let Some(settings) = state.storage.read_settings().await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to read settings");
    };
    if let Err((status, message)) = authorize(&state.storage, &settings, request.headers()).await {
        return error(status, message);
    }
    if id.is_empty() {
        return error(StatusCode::BAD_REQUEST, "Missing video request id");
    }
    let preferred = header_value(request.headers(), "x-connection-id");
    let pinned = if let Some(connection) = preferred {
        let connection = connection.to_owned();
        state
            .storage
            .run(false, move |db| {
                provider_connections::by_id(db, &connection)
            })
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let query_provider = || {
        request.uri().query().and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find_map(|(key, value)| (key == "provider").then(|| value.into_owned()))
                .filter(|provider| video_provider(provider))
        })
    };
    let provider = pinned
        .as_ref()
        .and_then(|account| account["provider"].as_str())
        .filter(|provider| video_provider(provider))
        .map(str::to_owned)
        .or_else(query_provider)
        .or_else(|| {
            id.starts_with("veoaifree-web:")
                .then(|| "veoaifree-web".to_owned())
        })
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_owned());
    if matches!(
        provider.as_str(),
        "uc-web" | "muse-web" | "grok-web" | "veoaifree-web"
    ) {
        return error(
            StatusCode::BAD_REQUEST,
            &format!("{provider} video returns completed media in the creation response"),
        );
    }
    if matches!(id.as_str(), "." | "..") {
        return error(StatusCode::BAD_REQUEST, "Invalid video request id");
    }
    let Ok(Selection::Ready(account)) =
        chat_accounts::select(&state.storage, &provider, "", &HashSet::new(), preferred).await
    else {
        return no_credentials(&provider);
    };
    let Some(connection) = account["id"].as_str() else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Invalid provider connection",
        );
    };
    let job = Job::Poll { id: &id };
    let egress = match video_egress(&state, &settings, &provider, &account, &job).await {
        Ok(selected) => selected,
        Err(failure) => return failure.response(),
    };
    let protected = egress.protected;
    match send_with_refresh(&state, &provider, &account, &job, egress).await {
        Ok(mut response) => {
            let _ = chat_accounts::mark_success(&state.storage, connection, "").await;
            set_connection(&mut response, connection);
            response
        }
        Err(failure) => {
            if !protected {
                mark_failure(&state.storage, &provider, connection, "", &failure).await;
            }
            failure.response()
        }
    }
}

fn protected_client(timeout: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

fn proxy_required(state: &VideoState, settings: &Value, account: &Value) -> bool {
    truthy(&settings["outboundProxyEnabled"])
        || proxy_transport::account_requires_tunnel(account)
        || !state.fixture
            && ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
                .iter()
                .any(|key| std::env::var(key).is_ok_and(|url| !url.is_empty()))
}

fn no_credentials(provider: &str) -> Response {
    error(
        StatusCode::BAD_REQUEST,
        &format!("No credentials for provider: {provider}"),
    )
}

fn unavailable(retry: Retry, last: Option<Failure>, provider: &str, model: &str) -> Response {
    let status = last
        .as_ref()
        .map(|failure| failure.status)
        .or_else(|| {
            retry
                .last_error_code
                .and_then(|code| StatusCode::from_u16(code).ok())
        })
        .unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    let reason = last
        .as_ref()
        .map(|failure| failure.message.as_str())
        .or(retry.last_error.as_deref())
        .unwrap_or("Unavailable");
    let model = if model.is_empty() { "video" } else { model };
    let message = format!(
        "[{provider}/{model}] {reason} ({})",
        retry.retry_after_human
    );
    into_http(Ok(json!({
        "status": status.as_u16(),
        "headers": {"content-type": "application/json", "retry-after": retry.retry_after_seconds.to_string()},
        "body": json!({"error": {"message": message}}).to_string(),
    })))
}

async fn mark_failure(
    storage: &Storage,
    provider: &str,
    connection: &str,
    model: &str,
    failure: &Failure,
) -> bool {
    chat_accounts::mark_unavailable(
        storage,
        provider,
        connection,
        model,
        failure.status.as_u16(),
        &failure.message,
        None,
    )
    .await
    .is_ok_and(|outcome| outcome.should_fallback)
}

fn set_connection(response: &mut Response, connection: &str) {
    if let Ok(value) = HeaderValue::from_str(connection) {
        response
            .headers_mut()
            .insert("x-9router-connection-id", value);
    }
}

enum Job<'a> {
    Create {
        action: &'a str,
        content_type: Option<&'a str>,
        idempotency_key: Option<&'a str>,
        body: Bytes,
    },
    Poll {
        id: &'a str,
    },
}

impl Job<'_> {
    fn vertex(&self) -> crate::vertex_video::Job<'_> {
        match self {
            Self::Create {
                action,
                content_type,
                body,
                ..
            } => crate::vertex_video::Job::Create {
                action,
                content_type: *content_type,
                raw_body: body,
            },
            Self::Poll { id } => crate::vertex_video::Job::Poll(id),
        }
    }
}

struct VideoEgress {
    client: reqwest::Client,
    url: String,
    relay: Option<Route>,
    vertex: Option<crate::vertex_video::Plan>,
    protected: bool,
    identity: EgressIdentity,
}

#[derive(Clone, PartialEq, Eq)]
enum EgressIdentity {
    Direct,
    Bypass,
    Tunnel(Url),
    Relay(String),
}

async fn video_egress(
    state: &VideoState,
    settings: &Value,
    provider: &str,
    account: &Value,
    job: &Job<'_>,
) -> Result<VideoEgress, Failure> {
    let vertex = if provider == "vertex" {
        Some(
            crate::vertex_video::prepare(account, job.vertex()).map_err(|message| {
                Failure::for_provider(StatusCode::BAD_REQUEST, provider, message)
            })?,
        )
    } else {
        None
    };
    let (endpoint, direct_url) = if let Some(plan) = &vertex {
        (plan.url().to_owned(), plan.direct_url())
    } else {
        let (base, fixture) = match provider {
            "xai" => (XAI, XAI_FIXTURE),
            "openrouter" => (OPENROUTER, OPENROUTER_FIXTURE),
            _ => {
                return Err(Failure::for_provider(
                    StatusCode::BAD_REQUEST,
                    provider,
                    "Provider does not support video generation",
                ));
            }
        };
        let path = match job {
            Job::Create { .. } if provider == "openrouter" => String::new(),
            Job::Create { action, .. } => format!("/{action}"),
            Job::Poll { id } => format!("/{}", component(id)),
        };
        let endpoint = format!("{base}{path}");
        let direct = if state.fixture {
            format!("{fixture}{path}")
        } else {
            endpoint.clone()
        };
        (endpoint, direct)
    };
    let direct = |protected| VideoEgress {
        client: if protected {
            state.protected.clone()
        } else {
            state.client.clone()
        },
        url: direct_url,
        relay: None,
        vertex: None,
        protected,
        identity: if protected {
            EgressIdentity::Bypass
        } else {
            EgressIdentity::Direct
        },
    };
    let unavailable = || {
        Failure::for_provider(
            StatusCode::SERVICE_UNAVAILABLE,
            provider,
            "video connection proxy is unavailable",
        )
    };
    let relay_unavailable = || {
        Failure::for_provider(
            StatusCode::SERVICE_UNAVAILABLE,
            provider,
            if provider == "openrouter" {
                "video requires a validated Relay pool"
            } else {
                "video requires an HTTP or SOCKS tunnel"
            },
        )
    };
    let route = crate::provider_http::select(&state.storage, settings, account, &endpoint)
        .await
        .map_err(|_| unavailable())?;
    let mut selected = match route {
        ProxySelection::Routed(Route::Direct) if !proxy_required(state, settings, account) => {
            direct(false)
        }
        ProxySelection::ExplicitBypass => direct(true),
        ProxySelection::Routed(Route::Proxy { url, .. }) => {
            let proxy = proxy_transport::tunnel_proxy_url(&url).ok_or_else(unavailable)?;
            let client = proxy_transport::vendor_metadata_client(
                proxy.clone(),
                protected_client(state.timeout),
            )
            .ok_or_else(unavailable)?;
            VideoEgress {
                client,
                url: endpoint,
                relay: None,
                vertex: None,
                protected: true,
                identity: EgressIdentity::Tunnel(proxy),
            }
        }
        ProxySelection::Routed(
            relay @ Route::Relay {
                authorization: Some(_),
                ..
            },
        ) if provider == "openrouter" => {
            let Route::Relay { url, .. } = &relay else {
                unreachable!()
            };
            let identity = EgressIdentity::Relay(url.clone());
            VideoEgress {
                url: url.clone(),
                client: state.protected.clone(),
                relay: Some(relay),
                vertex: None,
                protected: true,
                identity,
            }
        }
        ProxySelection::Routed(Route::Relay { .. }) => return Err(relay_unavailable()),
        _ => return Err(unavailable()),
    };
    if let Some(draft) = vertex {
        let (client, fixture) = if draft.needs_token_exchange() {
            let delivery = Delivery::select(
                &state.storage,
                settings,
                account,
                crate::vertex_transport::TOKEN_URL,
            )
            .await
            .map_err(|_| unavailable())?;
            let client = delivery
                .client(&state.protected, state.timeout)
                .ok_or_else(unavailable)?;
            (client, delivery.use_fixture())
        } else {
            (state.protected.clone(), state.fixture)
        };
        selected.vertex = Some(draft.authorize(&client, fixture).await.map_err(|message| {
            Failure::for_provider(StatusCode::BAD_REQUEST, provider, message)
        })?);
    }
    Ok(selected)
}

struct Failure {
    status: StatusCode,
    message: String,
}

impl Failure {
    fn for_provider(status: StatusCode, provider: &str, message: impl std::fmt::Display) -> Self {
        Self {
            status,
            message: format!("[{provider}] {message}"),
        }
    }

    fn response(self) -> Response {
        error(self.status, &self.message)
    }
}

async fn send_with_refresh(
    state: &VideoState,
    provider: &str,
    account: &Value,
    job: &Job<'_>,
    mut egress: VideoEgress,
) -> Result<Response, Failure> {
    let identity = egress.identity.clone();
    let xai_oauth = provider == "xai" && account["authType"] == "oauth";
    let route_changed = || {
        Failure::for_provider(
            StatusCode::SERVICE_UNAVAILABLE,
            provider,
            "Could not verify the video connection route after OAuth refresh",
        )
    };
    let refreshed = if xai_oauth && provider_oauth::refresh_due_for_provider(provider, account) {
        let settings = state
            .storage
            .read_settings()
            .await
            .ok_or_else(route_changed)?;
        provider_oauth::refresh_xai_routed(&state.storage, &settings, account)
            .await
            .ok()
    } else {
        None
    };
    let account = if let Some(refreshed) = &refreshed {
        let settings = state
            .storage
            .read_settings()
            .await
            .ok_or_else(route_changed)?;
        egress = video_egress(state, &settings, provider, refreshed, job).await?;
        if egress.identity != identity {
            return Err(route_changed());
        }
        refreshed
    } else {
        account
    };
    let initial = if xai_oauth && video_credential(account).is_none() {
        Err(Failure::for_provider(
            StatusCode::UNAUTHORIZED,
            provider,
            "Missing xAI OAuth access credential",
        ))
    } else {
        send(provider, account, job, egress).await
    };
    let failure = match initial {
        Err(failure)
            if xai_oauth
                && truthy(&account["refreshToken"])
                && matches!(
                    failure.status,
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                ) =>
        {
            failure
        }
        result => return result,
    };
    let Some(settings) = state.storage.read_settings().await else {
        return Err(failure);
    };
    if !video_egress(state, &settings, provider, account, job)
        .await
        .is_ok_and(|route| route.identity == identity)
    {
        return Err(failure);
    }
    let Ok(refreshed) =
        provider_oauth::refresh_xai_routed(&state.storage, &settings, account).await
    else {
        return Err(failure);
    };
    if refreshed["accessToken"] == account["accessToken"] || video_credential(&refreshed).is_none()
    {
        return Err(failure);
    }
    let Some(settings) = state.storage.read_settings().await else {
        return Err(failure);
    };
    let Ok(egress) = video_egress(state, &settings, provider, &refreshed, job).await else {
        return Err(failure);
    };
    if egress.identity != identity {
        return Err(failure);
    }
    send(provider, &refreshed, job, egress).await
}

fn video_credential(account: &Value) -> Option<&str> {
    let xai_oauth = account["provider"] == "xai" && account["authType"] == "oauth";
    ["accessToken", "apiKey"].into_iter().find_map(|field| {
        account[field].as_str().filter(|token| {
            !token.is_empty() && (!xai_oauth || token.bytes().all(|byte| byte.is_ascii_graphic()))
        })
    })
}

async fn send(
    provider: &str,
    account: &Value,
    job: &Job<'_>,
    egress: VideoEgress,
) -> Result<Response, Failure> {
    let openrouter = provider == "openrouter";
    let vertex = egress.vertex.as_ref();
    let (method, mut request) = if let Some(vertex) = &vertex {
        (
            if matches!(job, Job::Poll { .. }) {
                "GET"
            } else {
                "POST"
            },
            vertex.request_at(&egress.client, &egress.url),
        )
    } else {
        let route = &egress;
        match job {
            Job::Poll { .. } => ("GET", route.client.get(&route.url)),
            Job::Create {
                action,
                content_type,
                idempotency_key,
                body,
            } => {
                let mut request = if openrouter {
                    if *action != "generations" {
                        return Err(Failure::for_provider(
                            StatusCode::BAD_REQUEST,
                            provider,
                            format_args!(
                                "OpenRouter video supports 'generations' only (got '{action}')"
                            ),
                        ));
                    }
                    if content_type.is_some_and(|mime| !mime.contains("application/json")) {
                        return Err(Failure::for_provider(
                            StatusCode::BAD_REQUEST,
                            provider,
                            "OpenRouter video requires an application/json body",
                        ));
                    }
                    route
                        .client
                        .post(&route.url)
                        .header(header::CONTENT_TYPE, "application/json")
                } else {
                    let mut request = route.client.post(&route.url);
                    if let Some(content_type) = content_type {
                        request = request.header(header::CONTENT_TYPE, *content_type);
                    }
                    if let Some(idempotency_key) = idempotency_key {
                        request = request.header("idempotency-key", *idempotency_key);
                    }
                    request
                };
                request = request.body(body.clone());
                ("POST", request)
            }
        }
    };
    let protected = egress.protected;
    let relay = egress.relay.as_ref();
    let relay_token = match relay {
        Some(Route::Relay {
            authorization: Some(secret),
            ..
        }) => secret.to_str().ok(),
        _ => None,
    };
    if vertex.is_none() {
        request = request.header(header::ACCEPT, "application/json");
        if let Some(token) = video_credential(account) {
            if let Some(Route::Relay { path, .. }) = relay {
                let path = percent_encoding::percent_decode_str(path).decode_utf8_lossy();
                if path.contains(token) || relay_token.is_some_and(|secret| path.contains(secret)) {
                    return Err(Failure::for_provider(
                        StatusCode::BAD_REQUEST,
                        provider,
                        "video Relay destination contains a credential",
                    ));
                }
            }
            request = request.bearer_auth(token);
        } else if protected {
            return Err(Failure::for_provider(
                StatusCode::UNAUTHORIZED,
                provider,
                "Missing video API key",
            ));
        }
    }
    if openrouter {
        request = request
            .header("HTTP-Referer", "https://endpoint-proxy.local")
            .header("X-Title", "Endpoint Proxy");
    }
    let request = if let Some(relay) = relay {
        relay.relay(request)
    } else {
        request
    };
    let response = request.send().await.map_err(|err| {
        if err.is_timeout() {
            Failure::for_provider(
                StatusCode::REQUEST_TIMEOUT,
                provider,
                format_args!("video {method} aborted: The operation was aborted due to timeout"),
            )
        } else {
            Failure::for_provider(
                StatusCode::BAD_GATEWAY,
                provider,
                "video upstream fetch failed: fetch failed",
            )
        }
    })?;
    let status = response.status();
    if protected && status.is_redirection() {
        return Err(Failure::for_provider(
            StatusCode::BAD_GATEWAY,
            provider,
            "video upstream redirect refused",
        ));
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .cloned()
        .filter(|value| !value.is_empty())
        .unwrap_or(HeaderValue::from_static("application/json"));
    let bytes = response.bytes().await.unwrap_or_default();
    let raw = String::from_utf8_lossy(&bytes);
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    if !status.is_success() {
        let message = if raw.is_empty() {
            format!("HTTP {}", status.as_u16())
        } else {
            let safe = vertex.map_or_else(|| raw.to_owned(), |plan| plan.redact_error(raw));
            let safe = sanitized(&safe, account);
            match relay_token {
                Some(secret) => safe.replace(secret, "[redacted]"),
                None => safe,
            }
        };
        let message =
            String::from_utf16_lossy(&message.encode_utf16().take(2000).collect::<Vec<_>>());
        return Err(Failure::for_provider(status, provider, message));
    }
    if vertex.is_some()
        && let Some(normalized) = crate::vertex_video::normalize(raw)
    {
        return Ok(success(
            status,
            HeaderValue::from_static("application/json"),
            &normalized,
        ));
    }
    Ok(success(status, content_type, raw))
}

fn sanitized(raw: &str, account: &Value) -> String {
    let mut out = redact_bearer(raw);
    for field in ["accessToken", "refreshToken", "apiKey"] {
        if let Some(secret) = account[field]
            .as_str()
            .filter(|secret| secret.encode_utf16().count() >= 8)
        {
            out = out.replace(secret, "[redacted]");
        }
    }
    out
}

fn success(status: StatusCode, content_type: HeaderValue, body: &str) -> Response {
    let mime = content_type
        .to_str()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let compressible = mime.starts_with("text/")
        || matches!(
            mime.as_str(),
            "application/json" | "application/javascript" | "application/xml"
        )
        || mime.ends_with("+json")
        || mime.ends_with("+xml");
    let body = if matches!(status, StatusCode::NO_CONTENT | StatusCode::RESET_CONTENT) {
        Body::empty()
    } else {
        Body::from(body.to_owned())
    };
    (
        status,
        [
            (header::CONTENT_TYPE, content_type),
            (
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("*"),
            ),
            (
                header::VARY,
                HeaderValue::from_static(if compressible { VARY_ENCODING } else { VARY }),
            ),
        ],
        body,
    )
        .into_response()
}
