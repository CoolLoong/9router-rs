use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Extension, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use url::Url;

use super::{OAuthState, Operation, Provider, dashboard_session, handle, reply};

const TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Loopback {
    Codex,
    Xai,
}

impl Loopback {
    fn provider(self) -> Provider {
        match self {
            Self::Codex => Provider::Codex,
            Self::Xai => Provider::Xai,
        }
    }

    fn port(self) -> u16 {
        match self {
            Self::Codex => 1455,
            Self::Xai => 56121,
        }
    }

    fn running(self, state: &OAuthState) -> &Mutex<Running> {
        match self {
            Self::Codex => &state.callback.0,
            Self::Xai => &state.xai_callback.0,
        }
    }
}

pub(super) fn routes(flow: Loopback) -> Router<OAuthState> {
    let base = format!("/api/oauth/{}", flow.provider().id());
    let router = Router::new()
        .route(&format!("{base}/start-proxy"), get(start))
        .route(&format!("{base}/poll-status"), get(poll))
        .route(&format!("{base}/stop-proxy"), get(stop));
    let router = if flow == Loopback::Xai {
        router.route(&format!("{base}/manual-code"), post(manual))
    } else {
        router
    };
    router.layer(Extension(flow))
}

#[derive(Clone, Default)]
pub(super) struct Proxy(Arc<Mutex<Running>>);

#[derive(Default)]
struct Running {
    listener: Option<Listener>,
    sessions: HashMap<String, Session>,
}

impl Running {
    fn stop_when_idle(&mut self) {
        if !self
            .sessions
            .values()
            .any(|entry| !matches!(entry.stage, Stage::Finished(_)))
            && let Some(listener) = self.listener.take()
        {
            let _ = listener.close.send(());
        }
    }
}

struct Listener {
    close: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

struct Session {
    created: Instant,
    stage: Stage,
}

enum Stage {
    Pending { verifier: String, redirect: String },
    Exchanging,
    Finished(Value),
}

async fn start(
    State(state): State<OAuthState>,
    Extension(flow): Extension<Loopback>,
    request: Request,
) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return denied();
    }
    let query = query(&request);
    if !query.contains_key("app_port") {
        return bad("Missing app_port");
    }
    if !query
        .get("app_port")
        .and_then(|value| value.parse::<u16>().ok())
        .is_some_and(|port| port > 0)
    {
        return bad("Invalid app_port");
    }
    let Some((nonce, verifier, redirect)) = query
        .get("state")
        .zip(query.get("code_verifier"))
        .zip(query.get("redirect_uri"))
        .map(|((nonce, verifier), redirect)| (nonce, verifier, redirect))
    else {
        return bad(match flow {
            Loopback::Codex => "Missing Codex authorization",
            Loopback::Xai => "Missing xAI authorization",
        });
    };
    if !valid_redirect(flow, redirect)
        || !state
            .flow
            .has_code_attempt(flow.provider(), nonce, redirect, verifier)
    {
        return bad("Invalid or expired OAuth state");
    }
    let mut proxy = flow.running(&state).lock().await;
    proxy
        .sessions
        .retain(|_, entry| entry.created.elapsed() < TTL);
    if proxy.sessions.len() >= 512 {
        return reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":match flow {
                Loopback::Codex => "Too many pending Codex sign-ins",
                Loopback::Xai => "Too many pending xAI sign-ins",
            }}),
        );
    }
    if proxy
        .listener
        .as_ref()
        .is_none_or(|listener| listener.task.is_finished())
    {
        let listener = match TcpListener::bind(("127.0.0.1", flow.port())).await {
            Ok(listener) => listener,
            Err(error) => {
                let reason = match error.kind() {
                    std::io::ErrorKind::AddrInUse => "port_busy",
                    _ => "listener_unavailable",
                };
                return reply(
                    StatusCode::OK,
                    json!({"success":false,"reason":reason,"serverSide":false}),
                );
            }
        };
        let (close, closed) = oneshot::channel::<()>();
        let app = Router::new()
            .route("/auth/callback", get(receive))
            .route("/callback", get(receive))
            .layer(Extension(flow))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    tokio::select! {
                        _ = closed => (),
                        _ = tokio::time::sleep(TTL) => (),
                    }
                })
                .await;
        });
        proxy.listener = Some(Listener { close, task });
    }
    proxy
        .sessions
        .entry(nonce.clone())
        .or_insert_with(|| Session {
            created: Instant::now(),
            stage: Stage::Pending {
                verifier: verifier.clone(),
                redirect: redirect.clone(),
            },
        });
    reply(StatusCode::OK, json!({"success":true,"serverSide":true}))
}

async fn poll(
    State(state): State<OAuthState>,
    Extension(flow): Extension<Loopback>,
    request: Request,
) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return denied();
    }
    let query = query(&request);
    let Some(nonce) = query.get("state").filter(|value| !value.is_empty()) else {
        return bad("Missing state");
    };
    let mut proxy = flow.running(&state).lock().await;
    proxy
        .sessions
        .retain(|_, entry| entry.created.elapsed() < TTL);
    let data = match proxy.sessions.get(nonce).map(|entry| &entry.stage) {
        Some(Stage::Finished(result)) => {
            let result = result.clone();
            proxy.sessions.remove(nonce);
            result
        }
        Some(Stage::Pending { .. } | Stage::Exchanging) => json!({"status":"pending"}),
        None => json!({"status":"unknown"}),
    };
    reply(StatusCode::OK, data)
}

async fn stop(
    State(state): State<OAuthState>,
    Extension(flow): Extension<Loopback>,
    request: Request,
) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return denied();
    }
    let mut proxy = flow.running(&state).lock().await;
    let requested = query(&request);
    let nonce = requested.get("state");
    proxy.sessions.retain(|id, entry| {
        !matches!(entry.stage, Stage::Pending { .. }) || nonce.is_some_and(|nonce| id != nonce)
    });
    proxy.stop_when_idle();
    reply(StatusCode::OK, json!({"success":true}))
}

async fn manual(
    State(state): State<OAuthState>,
    Extension(flow): Extension<Loopback>,
    request: Request,
) -> Response {
    if !dashboard_session::authenticated(&state.storage, request.headers()).await {
        return denied();
    }
    let body = to_bytes(request.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok());
    let Some(body) = body.filter(Value::is_object) else {
        return bad("Invalid or empty request body");
    };
    let field = |name: &str| {
        body.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let (Some(nonce), Some(code)) = (field("state"), field("code")) else {
        return bad("Missing authorization state or code");
    };
    let (verifier, redirect) = {
        let mut proxy = flow.running(&state).lock().await;
        match take_pending(&mut proxy, nonce, None) {
            Ok(fields) => fields,
            Err((status, message)) => return reply(status, json!({"error":message})),
        }
    };
    let result = exchange_code(&state, flow, nonce, code, &verifier, &redirect).await;
    let mut proxy = flow.running(&state).lock().await;
    proxy.sessions.remove(nonce);
    proxy.stop_when_idle();
    match result {
        Ok(result) => reply(StatusCode::OK, result),
        Err(error) => reply(error.status, json!({"error":error.message})),
    }
}

async fn receive(
    State(state): State<OAuthState>,
    Extension(flow): Extension<Loopback>,
    request: Request,
) -> Response {
    if !callback_origin(flow, &request) {
        return (
            StatusCode::FORBIDDEN,
            "Authorization callback origin rejected",
        )
            .into_response();
    }
    let query = query(&request);
    let Some(nonce) = query.get("state").filter(|value| !value.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "Missing authorization state").into_response();
    };
    let (verifier, redirect) = {
        let mut proxy = flow.running(&state).lock().await;
        match take_pending(&mut proxy, nonce, Some(request.uri().path())) {
            Ok(fields) => fields,
            Err(error) => return error.into_response(),
        }
    };
    let result = if query.contains_key("error") {
        json!({"status":"error","error":"The provider declined authorization"})
    } else if let Some(code) = query.get("code").filter(|value| !value.is_empty()) {
        match exchange_code(&state, flow, nonce, code, &verifier, &redirect).await {
            Ok(result) => {
                json!({"status":"done","connectionId":result["connection"]["id"],"email":result["connection"]["email"]})
            }
            Err(reason) => json!({"status":"error","error":reason.message}),
        }
    } else {
        json!({"status":"error","error":"No authorization code received"})
    };
    let complete = result["status"] == "done";
    let mut proxy = flow.running(&state).lock().await;
    if let Some(session) = proxy.sessions.get_mut(nonce) {
        session.stage = Stage::Finished(result);
    }
    proxy.stop_when_idle();
    let output = match (flow, complete) {
        (Loopback::Xai, true) => "You can close this window.",
        (Loopback::Codex, true) => "Authorization saved. You can close this tab.",
        (_, false) => "Authorization failed. Return to 9Router to try again.",
    };
    let mut response = (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        if flow == Loopback::Xai {
            format!("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>9Router authorization</title><p>{output}</p></html>")
        } else {
            output.to_owned()
        },
    )
        .into_response();
    if flow == Loopback::Xai {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/html; charset=utf-8"),
        );
    }
    response
}

fn take_pending(
    proxy: &mut Running,
    nonce: &str,
    path: Option<&str>,
) -> Result<(String, String), (StatusCode, &'static str)> {
    let session = proxy
        .sessions
        .get_mut(nonce)
        .filter(|entry| entry.created.elapsed() < TTL)
        .ok_or((
            StatusCode::BAD_REQUEST,
            "Authorization state is invalid or expired",
        ))?;
    let Stage::Pending { verifier, redirect } = &session.stage else {
        return Err((
            StatusCode::CONFLICT,
            "Authorization callback was already received",
        ));
    };
    if path.is_some_and(|path| !Url::parse(redirect).is_ok_and(|url| url.path() == path)) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Authorization callback path rejected",
        ));
    }
    let fields = (verifier.clone(), redirect.clone());
    session.stage = Stage::Exchanging;
    Ok(fields)
}

async fn exchange_code(
    state: &OAuthState,
    flow: Loopback,
    nonce: &str,
    code: &str,
    verifier: &str,
    redirect: &str,
) -> Result<Value, super::ApiError> {
    let fields = json!({"code":code,"state":nonce,"redirectUri":redirect,"codeVerifier":verifier});
    handle(
        &state.storage,
        &state.flow,
        flow.provider(),
        Operation::Exchange,
        "",
        &fields,
    )
    .await
}

fn callback_origin(flow: Loopback, request: &Request) -> bool {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let host_ok = host
        .and_then(|host| host.split_once(':'))
        .is_some_and(|(name, port)| {
            matches!(name, "localhost" | "127.0.0.1") && port == flow.port().to_string()
        });
    host_ok
        && request.headers().get(header::ORIGIN).is_none_or(|origin| {
            origin
                .to_str()
                .ok()
                .and_then(|raw| Url::parse(raw).ok())
                .is_some_and(|url| {
                    url.scheme() == "http"
                        && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
                        && url.username().is_empty()
                        && url.password().is_none()
                })
        })
}

fn valid_redirect(flow: Loopback, uri: &str) -> bool {
    if flow == Loopback::Xai {
        return uri == "http://127.0.0.1:56121/callback";
    }
    matches!(
        uri,
        "http://localhost:1455/auth/callback"
            | "http://127.0.0.1:1455/auth/callback"
            | "http://localhost:1455/callback"
            | "http://127.0.0.1:1455/callback"
    )
}

fn query(request: &Request) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for (key, value) in
        url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
    {
        values
            .entry(key.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    values
}

fn bad(message: &str) -> Response {
    reply(StatusCode::BAD_REQUEST, json!({"error":message}))
}

fn denied() -> Response {
    reply(StatusCode::UNAUTHORIZED, json!({"error":"Unauthorized"}))
}
