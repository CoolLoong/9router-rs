use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::stream;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::{dashboard_session, settings, storage::Storage};

const CAPACITY: usize = 200;
const PING: Duration = Duration::from_secs(25);
static LOG: OnceLock<Log> = OnceLock::new();
static TERMINAL_LOG: AtomicBool = AtomicBool::new(false);

struct Log {
    entries: Mutex<Entries>,
    events: broadcast::Sender<Event>,
}

#[derive(Default)]
struct Entries {
    lines: VecDeque<String>,
    closing: bool,
}

#[derive(Clone)]
enum Event {
    Line(String),
    Clear,
    Stop,
}

fn log() -> &'static Log {
    LOG.get_or_init(|| Log {
        entries: Mutex::default(),
        events: broadcast::channel(CAPACITY).0,
    })
}

pub fn record(level: &str, message: &str) {
    let book = log();
    let mut entries = book
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if entries.closing {
        return;
    }
    let timestamp = chrono::Utc::now().format("%H:%M:%S%.3f");
    let line = format!("{timestamp} [{level}] {message}");
    if entries.lines.len() == CAPACITY {
        entries.lines.pop_front();
    }
    entries.lines.push_back(line.clone());
    let _ = book.events.send(Event::Line(line.clone()));
    drop(entries);
    if TERMINAL_LOG.load(Ordering::Relaxed) {
        print_terminal(&line);
    }
}

pub fn enable_terminal() {
    TERMINAL_LOG.store(true, Ordering::Relaxed);
}

pub(crate) fn print_terminal(line: &str) {
    let clean = line.replace(char::is_control, " ");
    let _ = writeln!(std::io::stderr().lock(), "{clean}");
}

pub async fn request(req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str)
        .to_owned();
    let method = match *req.method() {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    };
    let response = next.run(req).await;
    let status = response.status();
    let level = if status.is_client_error() || status.is_server_error() {
        "WARN"
    } else {
        "INFO"
    };
    record(level, &format!("{method} {route} {}", status.as_u16()));
    response
}

pub fn inference(provider: &str, model: &str, step: &str, failed: bool) {
    let text = |value: &str| {
        value
            .chars()
            .take(160)
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>()
    };
    record(
        if failed { "WARN" } else { "INFO" },
        &format!("{step} {}/{}", text(provider), text(model)),
    );
}

pub fn shutdown() {
    let book = log();
    let mut entries = book
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    entries.closing = true;
    let _ = book.events.send(Event::Stop);
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/translator/console-logs", get(list).delete(clear))
        .route("/api/translator/console-logs/stream", get(events))
        .route_layer(middleware::from_fn_with_state(storage, authorize))
}

async fn authorize(State(storage): State<Storage>, request: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&storage, request.headers()).await {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::VARY, "Accept-Encoding")],
            axum::Json(json!({"error":"Unauthorized"})),
        )
            .into_response()
    }
}

fn snapshot() -> (bool, Value) {
    let book = log();
    let buffer = book
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    (buffer.closing, json!(&buffer.lines))
}

async fn list() -> Response {
    settings::reply(
        StatusCode::OK,
        json!({"success":true,"logs":snapshot().1}),
        false,
    )
}

async fn clear() -> Response {
    let book = log();
    let mut entries = book
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    entries.lines.clear();
    let _ = book.events.send(Event::Clear);
    settings::reply(StatusCode::OK, json!({"success":true}), false)
}

async fn events() -> Response {
    let book = log();
    let (subscriber, initial, closing) = {
        let buffer = book
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let initial =
            (!buffer.lines.is_empty()).then(|| event(&json!({"type":"init","logs":&buffer.lines})));
        (book.events.subscribe(), initial, buffer.closing)
    };
    let input = stream::unfold(
        (subscriber, initial, closing),
        |(mut subscriber, initial, closing)| async move {
            if closing {
                return None;
            }
            if let Some(initial) = initial {
                return Some((Ok::<_, Infallible>(initial), (subscriber, None, false)));
            }
            let frame = match tokio::time::timeout(PING, subscriber.recv()).await {
                Ok(Ok(Event::Line(line))) => event(&json!({"type":"line","line":line})),
                Ok(Ok(Event::Clear)) => event(&json!({"type":"clear"})),
                Ok(Ok(Event::Stop) | Err(broadcast::error::RecvError::Closed)) => return None,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    subscriber = subscriber.resubscribe();
                    let (closing, logs) = snapshot();
                    if closing {
                        return None;
                    }
                    event(&json!({"type":"init","logs":logs}))
                }
                Err(_) => ": ping\n\n".into(),
            };
            Some((Ok(frame), (subscriber, None, false)))
        },
    );
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE.as_str(), "text/event-stream"),
            (header::CACHE_CONTROL.as_str(), "no-cache, no-transform"),
            (header::CONNECTION.as_str(), "keep-alive"),
            (header::VARY.as_str(), "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding"),
            ("x-accel-buffering", "no"),
        ],
        Body::from_stream(input),
    ).into_response()
}

fn event(data: &Value) -> String {
    format!("data: {data}\n\n")
}
