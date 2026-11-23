use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Path, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::stream;
use regex::Regex;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::catalog_guard::{is_direct_loopback, valid_cli_token};
use crate::chat_input::js_space;
use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::settings::reply;
use crate::storage::Storage;

pub(crate) const PLUGIN: &str = "browsermcp";
const PACKAGE: &str = "@browsermcp/mcp@latest";
const MAX_SESSIONS: usize = 4;
const FRAME_LIMIT: usize = 8 * 1024 * 1024;
const QUEUE_LIMIT: usize = 4;
const EXIT_GRACE: Duration = Duration::from_millis(250);
const VARY: &str = "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding";

#[derive(Clone, Default)]
pub struct Manager {
    registry: Arc<Mutex<Registry>>,
}

#[derive(Default)]
struct Registry {
    closing: bool,
    sessions: HashMap<Uuid, Session>,
}

struct Session {
    input: mpsc::Sender<Vec<u8>>,
    stop: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

enum Failure {
    Closed,
    Full,
    Spawn,
    Missing,
}

struct Feed {
    endpoint: Option<Bytes>,
    output: mpsc::Receiver<Bytes>,
    cancel: watch::Receiver<bool>,
    stop: watch::Sender<bool>,
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

impl Manager {
    pub async fn shutdown(&self) {
        let jobs = {
            let mut registry = self
                .registry
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            registry.closing = true;
            registry
                .sessions
                .values()
                .map(|session| (session.stop.clone(), session.done.clone()))
                .collect::<Vec<_>>()
        };
        for (stop, _) in &jobs {
            stop.send_replace(true);
        }
        for (_, mut done) in jobs {
            let _ = done.wait_for(|finished| *finished).await;
        }
    }

    fn open(&self) -> Result<Feed, Failure> {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if registry.closing {
            return Err(Failure::Closed);
        }
        if registry.sessions.len() >= MAX_SESSIONS {
            return Err(Failure::Full);
        }
        let mut command = Command::new(if cfg!(windows) { "npx.cmd" } else { "npx" });
        command
            .args(["-y", PACKAGE])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(target_os = "linux")]
        command.process_group(0);
        let mut child = command.spawn().map_err(|_| Failure::Spawn)?;
        let stdin = child.stdin.take().ok_or(Failure::Spawn)?;
        let stdout = child.stdout.take().ok_or(Failure::Spawn)?;
        let sid = Uuid::new_v4();
        let (input, incoming) = mpsc::channel(QUEUE_LIMIT);
        let (outgoing, output) = mpsc::channel(QUEUE_LIMIT);
        let (stop, cancel) = watch::channel(false);
        let (finished, done) = watch::channel(false);
        registry.sessions.insert(
            sid,
            Session {
                input,
                stop: stop.clone(),
                done,
            },
        );
        let manager = self.clone();
        let worker_cancel = cancel.clone();
        tokio::spawn(async move {
            worker(child, stdin, stdout, incoming, outgoing, worker_cancel).await;
            finished.send_replace(true);
            manager
                .registry
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .sessions
                .remove(&sid);
        });
        let endpoint = Bytes::from(format!(
            "event: endpoint\ndata: /api/mcp/{PLUGIN}/message?sessionId={sid}\n\n"
        ));
        Ok(Feed {
            endpoint: Some(endpoint),
            output,
            cancel,
            stop,
        })
    }

    fn send(&self, id: Uuid, frame: Vec<u8>) -> Result<(), Failure> {
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if registry.closing {
            return Err(Failure::Closed);
        }
        let session = registry
            .sessions
            .get(&id)
            .filter(|session| !*session.stop.borrow())
            .ok_or(Failure::Missing)?;
        session.input.try_send(frame).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => Failure::Full,
            mpsc::error::TrySendError::Closed(_) => Failure::Missing,
        })
    }
}

pub fn router(storage: Storage, manager: Manager) -> Router {
    Router::new()
        .route("/api/mcp/{plugin}/sse", get(sse))
        .route("/api/mcp/{plugin}/message", post(message))
        .route_layer(middleware::from_fn_with_state(storage, authorize))
        .with_state(manager)
}

async fn authorize(State(storage): State<Storage>, request: Request, next: Next) -> Response {
    if valid_cli_token(&storage, request.headers())
        || is_direct_loopback(&request)
            && dashboard_session::authenticated(&storage, request.headers()).await
    {
        return next.run(request).await;
    }
    (
        StatusCode::FORBIDDEN,
        [(header::VARY, "Accept-Encoding")],
        axum::Json(json!({
            "error":"Local only: CLI token required",
        })),
    )
        .into_response()
}

fn error(status: StatusCode, text: impl Into<String>) -> Response {
    reply(status, json!({"error":text.into()}), false)
}

fn rejected(failure: Failure) -> Response {
    match failure {
        Failure::Closed => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "MCP bridge is shutting down",
        ),
        Failure::Full => error(
            StatusCode::TOO_MANY_REQUESTS,
            "MCP bridge capacity exceeded",
        ),
        Failure::Spawn => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not start MCP plugin",
        ),
        Failure::Missing => error(StatusCode::NOT_FOUND, "Unknown or closed MCP session"),
    }
}

async fn sse(
    State(manager): State<Manager>,
    Path(plugin): Path<String>,
    request: Request,
) -> Response {
    if plugin != PLUGIN {
        return into_http(Ok(json!({
            "status":404,"headers":{"content-type":"text/plain;charset=UTF-8"},
            "body":format!("Unknown plugin: {plugin}"),
        })));
    }
    if request.method() == Method::HEAD {
        return sse_response(Body::empty());
    }
    match manager.open() {
        Ok(feed) => {
            let frames = stream::unfold(feed, |mut feed| async move {
                let frame = tokio::select! {
                    biased;
                    _ = cancelled(&mut feed.cancel) => None,
                    frame = async {
                        if let Some(endpoint) = feed.endpoint.take() {
                            Some(endpoint)
                        } else {
                            feed.output.recv().await
                        }
                    } => frame,
                };
                frame.map(|frame| (Ok::<Bytes, io::Error>(frame), feed))
            });
            sse_response(Body::from_stream(frames))
        }
        Err(failure) => rejected(failure),
    }
}

fn sse_response(body: Body) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header(header::CONNECTION, "keep-alive")
        .header("x-accel-buffering", "no")
        .header(header::VARY, VARY)
        .body(body)
        .expect("fixed MCP event-stream headers")
}

async fn message(
    State(manager): State<Manager>,
    Path(plugin): Path<String>,
    request: Request,
) -> Response {
    if plugin != PLUGIN {
        return error(StatusCode::NOT_FOUND, format!("Unknown plugin: {plugin}"));
    }
    let mut query =
        url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
            .filter_map(|(key, value)| (key == "sessionId").then_some(value));
    let id = query.next().and_then(|value| Uuid::parse_str(&value).ok());
    let Some(id) = id.filter(|_| query.next().is_none()) else {
        return error(StatusCode::BAD_REQUEST, "A valid sessionId is required");
    };
    let Ok(body) = to_bytes(request.into_body(), FRAME_LIMIT).await else {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "MCP frame exceeds the size limit",
        );
    };
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    let Ok(mut wire) = serde_json::to_vec(&value) else {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    if wire.len() >= FRAME_LIMIT {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "MCP frame exceeds the size limit",
        );
    }
    wire.push(b'\n');
    match manager.send(id, wire) {
        Ok(()) => into_http(Ok(json!({"status":202,"headers":{},"body":""}))),
        Err(failure) => rejected(failure),
    }
}

async fn cancelled(receiver: &mut watch::Receiver<bool>) {
    let _ = receiver.wait_for(|cancelled| *cancelled).await;
}

async fn worker(
    mut child: Child,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    mut input: mpsc::Receiver<Vec<u8>>,
    output: mpsc::Sender<Bytes>,
    mut stop: watch::Receiver<bool>,
) {
    let mut reader = BufReader::new(stdout);
    tokio::select! {
        biased;
        _ = cancelled(&mut stop) => {}
        _ = send_frames(&mut reader, &output) => {}
        _ = write_frames(&mut stdin, &mut input) => {}
    }
    drop(output);
    drop(stdin);
    drop(reader);
    reap(&mut child).await;
}

async fn reap(child: &mut Child) {
    #[cfg(target_os = "linux")]
    if let Some(group) = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(rustix::process::Pid::from_raw)
    {
        tokio::time::sleep(EXIT_GRACE).await;
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        let _ = child.start_kill();
        let _ = child.wait().await;
        return;
    }
    if tokio::time::timeout(EXIT_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

async fn write_frames(
    writer: &mut (impl AsyncWrite + Unpin),
    input: &mut mpsc::Receiver<Vec<u8>>,
) -> io::Result<()> {
    while let Some(frame) = input.recv().await {
        writer.write_all(&frame).await?;
    }
    Ok(())
}

async fn send_frames(
    reader: &mut (impl AsyncBufRead + Unpin),
    output: &mpsc::Sender<Bytes>,
) -> io::Result<()> {
    let mut frame = Vec::new();
    while read_frame(reader, &mut frame).await? {
        let text = String::from_utf8_lossy(&frame);
        let line = text.trim_matches(js_space);
        if let Some(line) = filter_frame(line) {
            if line.len() > FRAME_LIMIT {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "MCP frame too large",
                ));
            }
            if output
                .send(Bytes::from(format!("event: message\ndata: {line}\n\n")))
                .await
                .is_err()
            {
                return Ok(());
            }
        }
        frame.clear();
    }
    Ok(())
}

async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    frame: &mut Vec<u8>,
) -> io::Result<bool> {
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return Ok(false);
        }
        let end = bytes.iter().position(|byte| *byte == b'\n');
        let count = end.unwrap_or(bytes.len());
        if frame.len() + count > FRAME_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP frame too large",
            ));
        }
        frame.extend_from_slice(&bytes[..count]);
        reader.consume(count + usize::from(end.is_some()));
        if end.is_some() {
            return Ok(true);
        }
    }
}

fn filter_frame(line: &str) -> Option<String> {
    let mut value = serde_json::from_str::<Value>(line).ok()?;
    if !value.is_object() && !value.is_array() {
        return None;
    }
    let mut changed = false;
    if let Some(content) = value
        .get_mut("result")
        .and_then(|result| result.get_mut("content"))
        .and_then(Value::as_array_mut)
    {
        for item in content {
            if item["type"] == "text"
                && let Some(text) = item["text"].as_str()
                && text.encode_utf16().count() >= 2000
            {
                let filtered = filter_text(text);
                if filtered != text {
                    changed = true;
                    item["text"] = json!(filtered);
                }
            }
        }
    }
    if changed {
        serde_json::to_string(&value).ok()
    } else {
        Some(line.to_owned())
    }
}

fn filter_text(text: &str) -> String {
    static NOISE: OnceLock<Regex> = OnceLock::new();
    static EMPTY: OnceLock<Regex> = OnceLock::new();
    let noise = NOISE
        .get_or_init(|| Regex::new(r"(?m)^\s*-\s*generic:?\s*$").expect("snapshot noise regex"));
    let empty = EMPTY.get_or_init(|| {
        Regex::new(r#"(?m)^\s*-\s*text:\s*""\s*$"#).expect("snapshot empty-text regex")
    });
    let first = noise.replace_all(text, "");
    let second = empty.replace_all(&first, "");
    let compact = collapse_repeated(&second);
    if compact.encode_utf16().count() <= 50_000 {
        return compact;
    }
    let head = String::from_utf16_lossy(&compact.encode_utf16().take(49_700).collect::<Vec<_>>());
    format!(
        "{head}\n\n... [truncated {} chars by 9router bridge. Page is large; ask user to scroll/navigate to a specific section, or click an element with the refs shown above]",
        text.encode_utf16().count() - 49_700
    )
}

fn collapse_repeated(text: &str) -> String {
    static SIBLING: OnceLock<Regex> = OnceLock::new();
    let pattern = SIBLING.get_or_init(|| {
        Regex::new(r"^(\s*)-\s*([a-zA-Z]+)(?-u:\b)").expect("snapshot sibling regex")
    });
    let lines: Vec<_> = text.split('\n').collect();
    let mut output = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let Some((indent, role)) = signature(pattern, lines[start]) else {
            output.push(lines[start].to_owned());
            start += 1;
            continue;
        };
        let mut siblings = Vec::new();
        let mut end = start;
        while end < lines.len() {
            if signature(pattern, lines[end]) == Some((indent, role)) {
                siblings.push(end);
            } else if !lines[end]
                .strip_prefix(indent)
                .is_some_and(|line| line.starts_with([' ', '\t']))
            {
                break;
            }
            end += 1;
        }
        if end - start >= 30 && siblings.len() > 15 {
            output.extend(
                lines[start..siblings[10]]
                    .iter()
                    .map(|line| line.to_string()),
            );
            output.push(format!(
                "{indent}... [{} similar \"{role}\" items omitted by 9router bridge]",
                end - start - 15
            ));
            output.extend(
                lines[siblings[siblings.len() - 5]..end]
                    .iter()
                    .map(|line| line.to_string()),
            );
        } else {
            output.extend(lines[start..end].iter().map(|line| line.to_string()));
        }
        start = end;
    }
    output.join("\n")
}

fn signature<'a>(pattern: &Regex, line: &'a str) -> Option<(&'a str, &'a str)> {
    let parts = pattern.captures(line)?;
    Some((parts.get(1)?.as_str(), parts.get(2)?.as_str()))
}
