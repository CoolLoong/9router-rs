use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

use crate::dashboard_session;
use crate::settings::reply;
use crate::storage::Storage;

const ERROR_AGE: Duration = Duration::from_secs(10);
const EXTERNAL_POLL: Duration = Duration::from_secs(2);
const PING: Duration = Duration::from_secs(25);

static ACTIVITY: OnceLock<Activity> = OnceLock::new();

#[derive(Clone, Copy)]
enum Event {
    Pending,
    Usage,
}

struct Activity {
    details: Mutex<Details>,
    events: broadcast::Sender<Event>,
    shutdown: watch::Sender<bool>,
}

#[derive(Default)]
struct Details {
    accounts: BTreeMap<(String, String, String, String), u64>,
    error: Option<(String, Instant)>,
}

pub struct Pending {
    key: Option<(String, String, String, String)>,
}

fn activity() -> &'static Activity {
    ACTIVITY.get_or_init(|| Activity {
        details: Mutex::default(),
        events: broadcast::channel(64).0,
        shutdown: watch::channel(false).0,
    })
}

pub fn begin(account: &Value, provider: &str, model: &str) -> Pending {
    crate::console_logs::inference(provider, model, "Started", false);
    let id = account["id"].as_str().unwrap_or_default();
    let display = ["name", "email"]
        .iter()
        .find_map(|key| account[*key].as_str().filter(|text| !text.is_empty()))
        .unwrap_or(id);
    let key = (
        id.to_owned(),
        provider.to_owned(),
        model.to_owned(),
        display.to_owned(),
    );
    let book = activity();
    if let Ok(mut metrics) = book.details.lock() {
        *metrics.accounts.entry(key.clone()).or_default() += 1;
    }
    let _ = book.events.send(Event::Pending);
    Pending { key: Some(key) }
}

impl Pending {
    pub fn fail(mut self) {
        self.finish(true);
    }

    pub fn respond(self, response: Response) -> Response {
        let stream = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        if !stream {
            return response;
        }
        let (parts, body) = response.into_parts();
        let state = (self, body.into_data_stream());
        let wire = stream::unfold(state, |(mut pending, mut input)| async move {
            let frame = input.next().await?;
            if frame.is_err() {
                pending.finish(true);
            }
            Some((frame, (pending, input)))
        });
        Response::from_parts(parts, Body::from_stream(wire))
    }

    fn finish(&mut self, failed: bool) {
        let Some(key) = self.key.take() else { return };
        crate::console_logs::inference(
            &key.1,
            &key.2,
            if failed { "Failed" } else { "Finished" },
            failed,
        );
        let book = activity();
        if let Ok(mut metrics) = book.details.lock() {
            if let Some(count) = metrics.accounts.get_mut(&key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    metrics.accounts.remove(&key);
                }
            }
            if failed {
                metrics.error = Some((key.1.to_ascii_lowercase(), Instant::now()));
            }
        }
        let _ = book.events.send(Event::Pending);
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.finish(false);
    }
}

pub fn apply(value: &mut Value) {
    let Ok(book) = activity().details.lock() else {
        return;
    };
    let accounts: Vec<_> = book.accounts.iter().map(|((_, provider, model, name), count)| {
        json!({ "model": model, "provider": provider, "account": name, "count": count })
    }).collect();
    let error = book
        .error
        .as_ref()
        .filter(|(_, at)| at.elapsed() < ERROR_AGE)
        .map(|(provider, _)| provider.as_str())
        .unwrap_or("");
    if value.is_object() {
        value["activeRequests"] = json!(accounts);
        value["errorProvider"] = json!(error);
    }
}

pub fn usage_committed() {
    let _ = activity().events.send(Event::Usage);
}

pub fn shutdown() {
    activity().shutdown.send_replace(true);
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route("/api/usage/stream", get(live))
        .with_state(storage)
}

async fn live(State(storage): State<Storage>, request: Request) -> Response {
    if !dashboard_session::authenticated(&storage, request.headers()).await {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::VARY, "Accept-Encoding")],
            axum::Json(json!({ "error": "Unauthorized" })),
        )
            .into_response();
    }
    let events = activity().events.subscribe();
    let shutdown = activity().shutdown.subscribe();
    let row_id = usage_position(&storage).await;
    let Ok(stats) = crate::usage_dashboard::statistics(&storage, "all").await else {
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": "Failed to fetch usage stats" }),
            false,
        );
    };
    let first = record(&stats);
    let now = tokio::time::Instant::now();
    let mut external = tokio::time::interval_at(now + EXTERNAL_POLL, EXTERNAL_POLL);
    let mut ping = tokio::time::interval_at(now + PING, PING);
    external.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let state = Feed {
        storage,
        events,
        shutdown,
        stats,
        first: Some(first),
        row_id,
        external,
        ping,
        refresh: false,
    };
    let mut response = Response::new(Body::from_stream(stream::unfold(
        state,
        |mut state| async move {
            let frame = state.next().await?;
            Some((Ok::<_, Infallible>(frame), state))
        },
    )));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert(header::VARY, HeaderValue::from_static(
        "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding",
    ));
    response
}

struct Feed {
    storage: Storage,
    events: broadcast::Receiver<Event>,
    shutdown: watch::Receiver<bool>,
    stats: Value,
    first: Option<Bytes>,
    row_id: Option<i64>,
    external: tokio::time::Interval,
    ping: tokio::time::Interval,
    refresh: bool,
}

impl Feed {
    async fn next(&mut self) -> Option<Bytes> {
        if *self.shutdown.borrow() {
            return None;
        }
        if let Some(first) = self.first.take() {
            return Some(first);
        }
        if std::mem::take(&mut self.refresh) {
            self.row_id = usage_position(&self.storage).await;
            self.stats = crate::usage_dashboard::statistics(&self.storage, "all")
                .await
                .ok()?;
            return Some(record(&self.stats));
        }
        loop {
            tokio::select! {
                _ = self.shutdown.changed() => return None,
                action = self.events.recv() => {
                    match action {
                        Ok(Event::Pending) => (),
                        Ok(Event::Usage) | Err(broadcast::error::RecvError::Lagged(_)) => self.refresh = true,
                        Err(broadcast::error::RecvError::Closed) => return None,
                    }
                    apply(&mut self.stats);
                    return Some(record(&self.stats));
                }
                _ = self.external.tick() => {
                    let new = usage_position(&self.storage).await;
                    if new == self.row_id { continue }
                    self.row_id = new;
                    self.stats = crate::usage_dashboard::statistics(&self.storage, "all").await.ok()?;
                    return Some(record(&self.stats));
                }
                _ = self.ping.tick() => return Some(Bytes::from_static(b": ping\n\n")),
            }
        }
    }
}

fn record(data: &Value) -> Bytes {
    Bytes::from(format!("data: {data}\n\n"))
}

async fn usage_position(storage: &Storage) -> Option<i64> {
    storage
        .run(false, |db| {
            db.query_row("SELECT MAX(id) FROM usageHistory", [], |row| row.get(0))
        })
        .await
        .ok()
        .flatten()
}
