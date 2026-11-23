use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::Utc;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot, watch};
use tokio::task::JoinHandle;
use url::Url;

use super::{flow::ApiError, trae, windsurf, zed};
use crate::provider_index::js_trim;
use crate::storage::Storage;

const TTL: Duration = Duration::from_secs(600);
type Owner = [u8; 32];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Trae,
    Windsurf,
    Zed,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Trae => "Trae",
            Self::Windsurf => "Windsurf",
            Self::Zed => "Zed",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Trae => "trae",
            Self::Windsurf => "windsurf",
            Self::Zed => "zed",
        }
    }

    pub(super) fn path(self) -> &'static str {
        match self {
            Self::Trae => "/callback",
            Self::Windsurf => "/windsurf-auth-callback",
            Self::Zed => "/",
        }
    }

    fn state_matches(self, query: &HashMap<String, String>, expected: &str) -> bool {
        match self {
            Self::Trae => query_value(query, &["state", "login_trace_id", "loginTraceID"])
                .is_none_or(|state| state == expected),
            Self::Windsurf => query_value(query, &["state"]) == Some(expected),
            Self::Zed => query_value(query, &["state"]).is_none_or(|state| state == expected),
        }
    }
}

#[derive(Clone)]
pub(super) struct Callback {
    storage: Storage,
    kind: Kind,
    running: Arc<Mutex<Running>>,
}

#[derive(Default)]
struct Running {
    listener: Option<Listener>,
    session: Option<Session>,
}

struct Listener {
    port: u16,
    owner: Owner,
    close: oneshot::Sender<()>,
    deadline: watch::Sender<tokio::time::Instant>,
    task: JoinHandle<()>,
}

struct Session {
    id: String,
    owner: Owner,
    port: u16,
    created: Instant,
    stage: Stage,
    zed: Option<Arc<zed::Login>>,
    zed_key: Option<[u8; 32]>,
}

struct RegisteredZed {
    id: String,
    port: u16,
    login: Arc<zed::Login>,
}

enum Stage {
    Pending,
    Exchanging,
    Finished(Value),
}

impl Callback {
    pub(super) fn new(storage: Storage, kind: Kind) -> Self {
        Self {
            storage,
            kind,
            running: Arc::default(),
        }
    }

    pub(super) async fn start(&self, owner: Owner) -> Result<Value, ApiError> {
        self.start_at_port(owner, 0).await
    }

    pub(super) async fn start_at_port(
        &self,
        owner: Owner,
        preferred: u16,
    ) -> Result<Value, ApiError> {
        let mut running = self.running.lock().await;
        if let Some(listener) = &running.listener
            && !listener.task.is_finished()
        {
            if listener.owner != owner {
                return Ok(json!({"success":false,"reason":"listener_unavailable"}));
            }
            if self.kind == Kind::Zed {
                listener
                    .deadline
                    .send_replace(tokio::time::Instant::now() + TTL);
            }
            return Ok(self.status(listener.port));
        }
        let listener = match TcpListener::bind(("127.0.0.1", preferred)).await {
            Err(error) if preferred != 0 && error.kind() == std::io::ErrorKind::AddrInUse => {
                TcpListener::bind(("127.0.0.1", 0)).await
            }
            result => result,
        };
        let Ok(listener) = listener else {
            return Ok(json!({"success":false,"reason":"listener_unavailable"}));
        };
        let port = listener
            .local_addr()
            .map_err(|_| {
                ApiError::failed(match self.kind {
                    Kind::Trae => "Could not start Trae callback listener",
                    Kind::Windsurf => "Could not start Windsurf callback listener",
                    Kind::Zed => "Could not start Zed callback listener",
                })
            })?
            .port();
        let (close, mut closed) = oneshot::channel();
        let (deadline, mut expiry) = watch::channel(tokio::time::Instant::now() + TTL);
        let app = Router::new().route(self.kind.path(), get(receive));
        let app = match self.kind {
            Kind::Trae => app.route("/auth/callback", get(receive)),
            Kind::Zed => app.route("/callback", get(receive)),
            Kind::Windsurf => app,
        };
        let app = app.with_state((self.clone(), port));
        let zed_listener =
            (self.kind == Kind::Zed).then(|| (self.running.clone(), deadline.clone()));
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    loop {
                        let until = *expiry.borrow_and_update();
                        tokio::select! {
                            _ = &mut closed => break,
                            _ = tokio::time::sleep_until(until) => break,
                            changed = expiry.changed() => {
                                if changed.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                })
                .await;
            if let Some((running, deadline)) = zed_listener {
                let mut running = running.lock().await;
                if running
                    .listener
                    .as_ref()
                    .is_some_and(|listener| listener.deadline.same_channel(&deadline))
                {
                    running.listener = None;
                    if running.session.as_ref().is_some_and(|session| {
                        session.port == port && matches!(session.stage, Stage::Pending)
                    }) {
                        running.session = None;
                    }
                }
            }
        });
        running.listener = Some(Listener {
            port,
            owner,
            close,
            deadline,
            task,
        });
        Ok(self.status(port))
    }

    fn status(&self, port: u16) -> Value {
        json!({"success":true,"port":port,"callbackUrl":format!("http://127.0.0.1:{port}{}", self.kind.path())})
    }

    pub(super) async fn register(&self, owner: Owner, id: Option<&str>) -> Result<Value, ApiError> {
        self.register_session(owner, id, None).await
    }

    pub(super) async fn register_zed(
        &self,
        owner: Owner,
        id: &str,
        login: zed::Login,
    ) -> Result<Value, ApiError> {
        self.register_session(owner, Some(id), Some(Arc::new(login)))
            .await
    }

    async fn register_session(
        &self,
        owner: Owner,
        id: Option<&str>,
        zed: Option<Arc<zed::Login>>,
    ) -> Result<Value, ApiError> {
        let id = id
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ApiError::bad("Missing state"))?;
        if id.len() > 128 {
            return Err(ApiError::bad("Invalid state"));
        }
        let mut running = self.running.lock().await;
        let Some(listener) = running
            .listener
            .as_ref()
            .filter(|listener| listener.owner == owner && !listener.task.is_finished())
        else {
            return Ok(json!({"success":false}));
        };
        let port = listener.port;
        if running.session.as_ref().is_some_and(|session| {
            session.port == port && matches!(session.stage, Stage::Exchanging)
        }) {
            return Ok(json!({"success":false}));
        }
        let zed_key = zed.as_ref().map(|login| login.key_id());
        running.session = Some(Session {
            id: id.to_owned(),
            owner,
            port,
            created: Instant::now(),
            stage: Stage::Pending,
            zed,
            zed_key,
        });
        Ok(json!({"success":true}))
    }

    pub(super) async fn exchange_zed(
        &self,
        owner: Owner,
        id: Option<&str>,
        raw: String,
        login: zed::Login,
    ) -> Result<Value, ApiError> {
        let registration = {
            let running = self.running.lock().await;
            if let Some(session) = running.session.as_ref().filter(|session| {
                session.owner == owner
                    && id == Some(session.id.as_str())
                    && session.created.elapsed() < TTL
            }) {
                if session.zed_key != Some(login.key_id()) {
                    return Err(ApiError::bad(
                        "Zed authorization does not match this session",
                    ));
                }
                match &session.stage {
                    Stage::Pending => Some(RegisteredZed {
                        id: session.id.clone(),
                        port: session.port,
                        login: session
                            .zed
                            .as_ref()
                            .cloned()
                            .ok_or(ApiError::bad("Zed authorization did not complete"))?,
                    }),
                    Stage::Exchanging => return Err(zed_exchange_in_progress()),
                    Stage::Finished(result) if result["status"] == "done" => {
                        return Ok(
                            json!({"id":result["connectionId"],"email":result["email"],"displayName":result["displayName"]}),
                        );
                    }
                    Stage::Finished(_) => {
                        return Err(ApiError::bad("Zed authorization did not complete"));
                    }
                }
            } else {
                None
            }
        };
        if let Some(registration) = registration {
            self.complete_zed(registration, raw).await
        } else {
            let credentials = decrypt_zed(Arc::new(login), raw).await?;
            zed::save(&self.storage, credentials).await
        }
    }

    async fn complete_zed(
        &self,
        registration: RegisteredZed,
        raw: String,
    ) -> Result<Value, ApiError> {
        let credentials = decrypt_zed(registration.login.clone(), raw).await?;
        {
            let mut running = self.running.lock().await;
            let session = running
                .session
                .as_mut()
                .filter(|session| {
                    session.id == registration.id
                        && session.port == registration.port
                        && session.created.elapsed() < TTL
                        && session
                            .zed
                            .as_ref()
                            .is_some_and(|login| Arc::ptr_eq(login, &registration.login))
                        && matches!(session.stage, Stage::Pending)
                })
                .ok_or_else(zed_exchange_in_progress)?;
            session.stage = Stage::Exchanging;
        }
        let result = zed::save(&self.storage, credentials).await;
        self.finish(registration.port, &registration.id, &result)
            .await;
        result
    }

    async fn finish(&self, port: u16, id: &str, result: &Result<Value, ApiError>) {
        let mut running = self.running.lock().await;
        if let Some(session) = running
            .session
            .as_mut()
            .filter(|session| session.port == port && session.id == id)
        {
            let created =
                Utc::now().timestamp_millis() - session.created.elapsed().as_millis() as i64;
            let mut data = match result {
                Ok(connection) => {
                    let mut data = json!({"status":"done","connectionId":connection["id"]});
                    if let Some(value) = connection.get("email") {
                        data["email"] = value.clone();
                    }
                    if self.kind == Kind::Zed
                        && let Some(value) = connection.get("displayName")
                    {
                        data["displayName"] = value.clone();
                    }
                    data
                }
                Err(error) => json!({"status":"error","error":error.message}),
            };
            data["state"] = json!(id);
            data["createdAt"] = json!(created);
            session.stage = Stage::Finished(data);
            session.zed = None;
            if running
                .listener
                .as_ref()
                .is_some_and(|listener| listener.port == port)
            {
                close(&mut running);
            }
        }
    }

    pub(super) async fn poll(&self, owner: Owner, id: Option<&str>) -> Result<Value, ApiError> {
        let id = id
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ApiError::bad("Missing state"))?;
        let mut running = self.running.lock().await;
        if let Some(session) = &running.session
            && session.owner == owner
            && session.id == id
            && session.created.elapsed() < TTL
        {
            return match &session.stage {
                Stage::Finished(result) => {
                    let result = result.clone();
                    running.session = None;
                    Ok(result)
                }
                _ => Ok(json!({"status":"pending"})),
            };
        }
        Ok(json!({"status":"unknown"}))
    }

    pub(super) async fn stop(&self, owner: Owner) -> Value {
        let mut running = self.running.lock().await;
        if running
            .listener
            .as_ref()
            .is_some_and(|listener| listener.owner == owner)
        {
            close(&mut running);
        }
        if running
            .session
            .as_ref()
            .is_some_and(|session| session.owner == owner)
        {
            running.session = None;
        }
        json!({"success":true})
    }

    pub(super) async fn ide_status(&self) -> Value {
        let title = self.kind.name();
        let app = self.kind.id();
        let candidates: Vec<PathBuf> = match std::env::consts::OS {
            "macos" => vec![format!("/Applications/{title}.app").into()],
            "windows" => [
                ("LOCALAPPDATA", format!("Programs/{title}/{title}.exe")),
                ("ProgramFiles", format!("{title}/{title}.exe")),
            ]
            .into_iter()
            .filter_map(|(name, suffix)| {
                std::env::var_os(name).map(|root| PathBuf::from(root).join(suffix))
            })
            .collect(),
            "linux" => [
                format!("/usr/bin/{app}"),
                format!("/usr/local/bin/{app}"),
                format!("/opt/{app}"),
                format!("/opt/{title}"),
            ]
            .into_iter()
            .map(PathBuf::from)
            .collect(),
            _ => Vec::new(),
        };
        for path in candidates {
            if tokio::fs::metadata(&path).await.is_ok() {
                return json!({"installed":true,"path":path});
            }
        }
        let exe = if cfg!(windows) {
            format!("{app}.exe")
        } else {
            app.to_owned()
        };
        if let Some(paths) = std::env::var_os("PATH") {
            for path in std::env::split_paths(&paths) {
                if let Ok(metadata) = tokio::fs::metadata(path.join(&exe)).await {
                    #[cfg(unix)]
                    let executable = {
                        use std::os::unix::fs::PermissionsExt;
                        metadata.permissions().mode() & 0o111 != 0
                    };
                    #[cfg(not(unix))]
                    let executable = true;
                    if metadata.is_file() && executable {
                        return json!({"installed":true,"path":app});
                    }
                }
            }
        }
        json!({"installed":false,"path":null})
    }
}

fn zed_exchange_in_progress() -> ApiError {
    ApiError {
        status: StatusCode::CONFLICT,
        message: "Zed authorization session changed or is already being completed",
    }
}

async fn decrypt_zed(login: Arc<zed::Login>, raw: String) -> Result<zed::Credentials, ApiError> {
    if raw.len() > 8192 {
        return Err(ApiError::bad("Invalid Zed callback"));
    }
    tokio::task::spawn_blocking(move || login.decrypt(&raw))
        .await
        .ok()
        .flatten()
        .ok_or(ApiError::bad(
            "Invalid Zed callback or encrypted access token",
        ))
}

fn close(running: &mut Running) {
    if let Some(listener) = running.listener.take() {
        let _ = listener.close.send(());
    }
}

async fn receive(State((callback, port)): State<(Callback, u16)>, request: Request) -> Response {
    if !callback_origin(request.headers(), port) {
        return (StatusCode::FORBIDDEN, "Cross-origin callback rejected").into_response();
    }
    let raw = request.uri().query().unwrap_or_default();
    let query = parameters(raw);
    if callback.kind == Kind::Zed {
        let registration = {
            let running = callback.running.lock().await;
            running
                .session
                .as_ref()
                .filter(|session| {
                    session.port == port
                        && session.created.elapsed() < TTL
                        && matches!(session.stage, Stage::Pending)
                        && callback.kind.state_matches(&query, &session.id)
                })
                .and_then(|session| {
                    Some(RegisteredZed {
                        id: session.id.clone(),
                        port,
                        login: session.zed.as_ref()?.clone(),
                    })
                })
        };
        let success = if let Some(registration) = registration {
            callback
                .complete_zed(registration, raw.to_owned())
                .await
                .is_ok()
        } else {
            false
        };
        return result_page(success);
    }
    let id = {
        let mut running = callback.running.lock().await;
        let Some(session) = running.session.as_mut().filter(|session| {
            session.port == port
                && session.created.elapsed() < TTL
                && matches!(session.stage, Stage::Pending)
        }) else {
            return result_page(false);
        };
        if !callback.kind.state_matches(&query, &session.id) {
            session.stage = Stage::Finished(
                json!({"status":"error","error":format!("{} callback state mismatch", callback.kind.name())}),
            );
            close(&mut running);
            return result_page(false);
        }
        session.stage = Stage::Exchanging;
        session.id.clone()
    };
    let result = match callback.kind {
        Kind::Trae
            if query_value(&query, &["refreshToken", "refresh_token", "RefreshToken"])
                .is_some() =>
        {
            trae::exchange(&callback.storage, raw).await
        }
        Kind::Trae => Err(ApiError::failed("Trae callback missing refreshToken")),
        Kind::Windsurf => windsurf::exchange_callback(&callback.storage, raw, &id).await,
        Kind::Zed => unreachable!(),
    };
    let success = result.is_ok();
    callback.finish(port, &id, &result).await;
    result_page(success)
}

pub(super) fn parameters(query: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

pub(super) fn query_value<'a>(
    query: &'a HashMap<String, String>,
    names: &[&str],
) -> Option<&'a str> {
    names.iter().find_map(|name| {
        query
            .get(*name)
            .map(|value| js_trim(value))
            .filter(|value| !value.is_empty())
    })
}

fn callback_origin(headers: &HeaderMap, port: u16) -> bool {
    let host_ok = headers
        .get(header::HOST)
        .and_then(|raw| raw.to_str().ok())
        .and_then(|raw| raw.split_once(':'))
        .is_some_and(|(host, number)| {
            matches!(host, "localhost" | "127.0.0.1") && number.parse::<u16>().ok() == Some(port)
        });
    host_ok
        && headers.get(header::ORIGIN).is_none_or(|raw| {
            raw.to_str()
                .ok()
                .and_then(|raw| Url::parse(raw).ok())
                .is_some_and(|origin| {
                    origin.scheme() == "http"
                        && matches!(origin.host_str(), Some("localhost" | "127.0.0.1"))
                })
        })
}

fn result_page(success: bool) -> Response {
    (
        [
            (header::CONTENT_TYPE,"text/html; charset=utf-8"),
            (header::CACHE_CONTROL,"no-store"),
            (header::REFERRER_POLICY,"no-referrer"),
            (header::CONTENT_SECURITY_POLICY,"default-src 'none'; base-uri 'none'; frame-ancestors 'none'"),
        ],
        if success {
            "<!doctype html><title>Authorization complete</title><p>You can close this window.</p>"
        } else {
            "<!doctype html><title>Authorization failed</title><p>Return to 9Router to check the sign-in.</p>"
        },
    ).into_response()
}
