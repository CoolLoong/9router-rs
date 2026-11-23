use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::StreamExt;
use provider_capabilities_rs::port::model_capabilities;
use serde_json::{Map, Value, json};
use tempfile::NamedTempFile;

use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::provider_connections::truthy;
use crate::storage::Storage;
use crate::thinking_intent::numeric;

const URL: &str = "https://models.dev/api.json";
const FIXTURE: &str = "http://127.0.0.1:52989/models-dev/api.json";
const CATALOG: &str = "model-catalog.json";
const RAW: &str = "model-catalog-raw.json";
const REFRESH: Duration = Duration::from_secs(24 * 60 * 60);
const RETRY: Duration = Duration::from_secs(30 * 60);
const STARTUP: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_BODY: usize = 128 * 1024 * 1024;
const LIMIT_TOLERANCE: f64 = 0.1;
static METADATA: OnceLock<Value> = OnceLock::new();

#[derive(Clone)]
pub struct CatalogSync(Arc<Shared>);

struct Shared {
    storage: Storage,
    client: reqwest::Client,
    file: PathBuf,
    state: Mutex<Runtime>,
    scheduled: AtomicBool,
    fixture: bool,
}

#[derive(Default)]
struct Runtime {
    running: bool,
    last_sync: Option<i64>,
    last_error: Option<String>,
    last_result: Option<Value>,
    etag: Option<String>,
    file_version: Option<Value>,
}

impl CatalogSync {
    pub fn new(storage: Storage) -> Self {
        let file = storage.root().join(CATALOG);
        let state = Runtime::restore(&file).unwrap_or_default();
        Self(Arc::new(Shared {
            storage,
            client: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .redirect(crate::upstream::redirects())
                .build()
                .expect("model catalog client"),
            file,
            state: Mutex::new(state),
            scheduled: AtomicBool::new(false),
            fixture: std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1"),
        }))
    }

    pub fn start_scheduler(&self) {
        if self.0.fixture
            || std::env::var("MODEL_CATALOG_SYNC")
                .is_ok_and(|value| value.eq_ignore_ascii_case("off"))
            || self.0.scheduled.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            let mut delay = STARTUP;
            loop {
                tokio::time::sleep(delay).await;
                delay = if state.sync().await.is_some() {
                    REFRESH
                } else {
                    RETRY
                };
            }
        });
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/api/models/catalog-sync", get(status).post(run))
            .route_layer(middleware::from_fn_with_state(self.clone(), authorize))
            .with_state(self)
    }

    fn state(&self) -> MutexGuard<'_, Runtime> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    async fn sync(&self) -> Option<Value> {
        let ticket = Ticket::acquire(self.clone())?;
        let state = self.clone();
        tokio::spawn(async move {
            let update = state.download(ticket.etag.clone()).await;
            tokio::task::spawn_blocking(move || ticket.publish(update))
                .await
                .ok()
                .flatten()
        })
        .await
        .ok()
        .flatten()
    }

    async fn download(&self, conditional: Option<HeaderValue>) -> Result<Update, String> {
        let endpoint = if self.0.fixture { FIXTURE } else { URL };
        let mut request = self
            .0
            .client
            .get(endpoint)
            .header(header::ACCEPT, "application/json");
        if let Some(etag) = conditional {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        let response = request.send().await.map_err(fetch_failure)?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(Update::Unchanged);
        }
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let mut incoming = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = incoming.next().await {
            let chunk = chunk.map_err(fetch_failure)?;
            if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
                return Err("Model catalog response too large".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
        let data: Value =
            serde_json::from_slice(bytes).map_err(|_| "Invalid model catalog JSON")?;
        let data = data.as_object().ok_or("Invalid model catalog JSON")?;
        let (models, providers) = build(data);
        let count = models.len();
        let provider_count = providers.len();
        let file = json!({"v":version(),"etag":etag,"syncedAt":now(),"models":models,"providers":providers}).to_string();
        let result = json!({"status":"updated","etag":etag,"bytes":file.len(),"models":count,"providers":provider_count});
        Ok(Update::Replace {
            file,
            raw: slim(data).to_string(),
            etag,
            result,
        })
    }

    fn status(&self) -> Value {
        let state = self.state();
        json!({"running":state.running,"lastSync":state.last_sync,"lastError":state.last_error,
               "lastResult":state.last_result,"etag":state.etag,"fileVersion":state.file_version,
               "file":self.0.file,"url":URL,"intervalMs":REFRESH.as_millis(),"catalog":summary(&self.0.file)})
    }
}

impl Runtime {
    fn restore(path: &Path) -> Option<Self> {
        let file = File::open(path).ok()?;
        let modified = file.metadata().ok()?.modified().ok()?;
        let value: Value = serde_json::from_reader(file).ok()?;
        if value.is_null() {
            return None;
        }
        Some(Self {
            etag: value["etag"]
                .as_str()
                .filter(|etag| !etag.is_empty())
                .map(str::to_owned),
            file_version: Some(if truthy(&value["v"]) {
                value["v"].clone()
            } else {
                json!(1)
            }),
            last_sync: Some(chrono::DateTime::<chrono::Utc>::from(modified).timestamp_millis()),
            ..Self::default()
        })
    }
}

fn summary(path: &Path) -> Option<Value> {
    let file = File::open(path).ok()?;
    let bytes = file.metadata().ok()?.len();
    let parsed: Value = serde_json::from_reader(file).ok()?;
    if parsed.is_null() {
        return None;
    }
    let mut fields = Map::new();
    if let Some(time) = parsed.get("syncedAt") {
        fields.insert("syncedAt".into(), time.clone());
    }
    fields.insert("models".into(), json!(property_count(&parsed["models"])));
    fields.insert(
        "providers".into(),
        json!(property_count(&parsed["providers"])),
    );
    fields.insert("bytes".into(), json!(bytes));
    Some(Value::Object(fields))
}

fn property_count(value: &Value) -> usize {
    match value {
        Value::Object(fields) => fields.len(),
        Value::Array(items) => items.len(),
        Value::String(text) => text.encode_utf16().count(),
        _ => 0,
    }
}

struct Ticket {
    owner: CatalogSync,
    etag: Option<HeaderValue>,
    active: bool,
}

impl Ticket {
    fn acquire(owner: CatalogSync) -> Option<Self> {
        let etag = {
            let mut state = owner.state();
            if state.running {
                return None;
            }
            state.running = true;
            state
                .file_version
                .as_ref()
                .and_then(Value::as_f64)
                .filter(|current| *current == version() as f64)
                .and(state.etag.as_deref())
                .and_then(|tag| HeaderValue::from_str(tag).ok())
        };
        Some(Self {
            owner,
            etag,
            active: true,
        })
    }

    fn publish(mut self, outcome: Result<Update, String>) -> Option<Value> {
        let mut inner = self.owner.state();
        let result = match outcome {
            Ok(Update::Unchanged) => Ok(json!({"status":"unchanged"})),
            Ok(Update::Replace {
                file,
                raw,
                etag,
                result,
            }) => match write_pair(self.owner.0.storage.root(), &file, &raw) {
                Ok(()) => {
                    inner.etag = etag;
                    inner.file_version = Some(json!(version()));
                    Ok(result)
                }
                Err(_) => Err("Could not write model catalog files".to_owned()),
            },
            Err(error) => Err(error),
        };
        inner.running = false;
        self.active = false;
        match result {
            Ok(result) => {
                inner.last_error = None;
                inner.last_sync = Some(now());
                inner.last_result = Some(result.clone());
                Some(result)
            }
            Err(error) => {
                inner.last_error = Some(error);
                None
            }
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if self.active {
            self.owner.state().running = false;
        }
    }
}

enum Update {
    Unchanged,
    Replace {
        file: String,
        raw: String,
        etag: Option<String>,
        result: Value,
    },
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn fetch_failure(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "The operation was aborted due to timeout"
    } else {
        "fetch failed"
    }
    .into()
}

fn stage(root: &Path, content: &[u8]) -> io::Result<NamedTempFile> {
    let mut temp = NamedTempFile::new_in(root)?;
    temp.write_all(content)?;
    temp.as_file().sync_all()?;
    Ok(temp)
}

fn write_pair(root: &Path, file: &str, raw: &str) -> io::Result<()> {
    let catalog_path = root.join(CATALOG);
    let raw_path = root.join(RAW);
    let catalog = stage(root, file.as_bytes())?;
    let replacement = stage(root, raw.as_bytes())?;
    let prior = match fs::symlink_metadata(&raw_path) {
        Ok(metadata) if metadata.is_file() => {
            let mut previous = File::open(&raw_path)?;
            let mut copy = NamedTempFile::new_in(root)?;
            io::copy(&mut previous, &mut copy)?;
            copy.as_file().sync_all()?;
            Some(copy)
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid raw catalog path",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    replacement
        .persist(&raw_path)
        .map_err(|error| error.error)?;
    if let Err(error) = catalog.persist(catalog_path) {
        if let Some(prior) = prior {
            prior.persist(&raw_path).map_err(|failure| failure.error)?;
        } else {
            fs::remove_file(raw_path)?;
        }
        return Err(error.error);
    }
    Ok(())
}

fn metadata() -> &'static Value {
    METADATA.get_or_init(|| {
        serde_json::from_str(include_str!("catalog-sync-data.json"))
            .expect("pinned model catalog registry")
    })
}

fn version() -> u64 {
    metadata()["version"]
        .as_u64()
        .expect("model catalog version")
}

fn alias(provider: &str) -> &str {
    metadata()["aliases"][provider].as_str().unwrap_or(provider)
}

fn base_id(model: &str) -> String {
    model
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_lowercase()
}

fn entries(fields: &Map<String, Value>) -> Vec<(&str, &Value)> {
    let mut result: Vec<_> = fields
        .iter()
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    result.sort_by_key(|(key, _)| {
        key.parse::<u32>()
            .ok()
            .filter(|number| *number != u32::MAX && number.to_string() == *key)
            .unwrap_or(u32::MAX)
    });
    result
}

fn model_entries(provider: &Value) -> impl Iterator<Item = (&str, &Value)> {
    provider["models"]
        .as_object()
        .map(entries)
        .into_iter()
        .flatten()
}

fn build(catalog: &Map<String, Value>) -> (Map<String, Value>, Map<String, Value>) {
    let rows = metadata()["rows"].as_array().expect("model registry rows");
    let rows: Vec<_> = rows
        .iter()
        .filter_map(|entry| {
            Some((
                entry["provider"].as_str()?,
                entry["model"].as_str()?,
                &entry["contextLength"],
            ))
        })
        .collect();
    let mut local_by_upstream = HashMap::<_, Vec<_>>::new();
    for (provider, _, _) in &rows {
        let ids = local_by_upstream.entry(alias(provider)).or_default();
        if !ids.contains(provider) {
            ids.push(*provider);
        }
    }
    let mut by_provider = HashMap::new();
    let mut models = Map::new();
    for (provider, descriptor) in entries(catalog) {
        let mut indexed = HashMap::new();
        let mut seen = HashSet::new();
        for (model, data) in model_entries(descriptor) {
            let id = base_id(model);
            indexed.insert(id.clone(), data);
            if !seen.insert(id.clone()) {
                continue;
            }
            let modalities = capabilities(data);
            if !modalities.is_empty() {
                let modalities = Value::Object(modalities);
                let ids = local_by_upstream
                    .get(provider)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                for id_provider in ids {
                    models.insert(format!("{id_provider}:{id}"), modalities.clone());
                }
                if !ids.contains(&provider) {
                    models.insert(format!("{provider}:{id}"), modalities);
                }
            }
        }
        by_provider.insert(provider, indexed);
    }
    let mut overrides = Map::new();
    for (provider, model, explicit_context) in rows {
        let upstream = if catalog.get(provider).is_some_and(truthy) {
            provider
        } else {
            alias(provider)
        };
        let Some(entry) = by_provider
            .get(upstream)
            .and_then(|models| models.get(&base_id(model)))
        else {
            continue;
        };
        let current = model_capabilities(Some(provider), model, None);
        let limits = &entry["limit"];
        let mut delta = Map::new();
        if !truthy(explicit_context) && differs(&limits["context"], &current["contextWindow"]) {
            delta.insert("contextWindow".into(), limits["context"].clone());
        }
        if differs(&limits["output"], &current["maxOutput"]) {
            delta.insert("maxOutput".into(), limits["output"].clone());
        }
        if !delta.is_empty() {
            overrides
                .entry(provider.to_owned())
                .or_insert_with(|| json!({}))[model] = Value::Object(delta);
        }
    }
    (models, overrides)
}

fn capabilities(model: &Value) -> Map<String, Value> {
    model["modalities"]["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let name = match value.as_str()? {
                "image" => "vision",
                "pdf" => "pdf",
                "audio" => "audioInput",
                "video" => "videoInput",
                _ => return None,
            };
            Some((name.to_owned(), Value::Bool(true)))
        })
        .collect()
}

fn differs(upstream: &Value, current: &Value) -> bool {
    numeric(upstream)
        .zip(numeric(current))
        .is_some_and(|(value, baseline)| {
            value > 0.0 && (value - baseline).abs() / baseline > LIMIT_TOLERANCE
        })
}

fn slim(catalog: &Map<String, Value>) -> Value {
    let providers = entries(catalog)
        .into_iter()
        .map(|(provider, descriptor)| {
            let models = model_entries(descriptor)
                .map(|(model, raw)| {
                    let input: Vec<_> = raw["modalities"]["input"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|entry| *entry != "text")
                        .collect();
                    let mut compact = json!({"i":input});
                    for (key, limit) in [("c", "context"), ("o", "output")] {
                        if let Some(value) = raw["limit"].get(limit) {
                            compact[key] = value.clone();
                        }
                    }
                    if truthy(&raw["reasoning"]) {
                        compact["r"] = raw["reasoning"].clone();
                    }
                    (model.to_owned(), compact)
                })
                .collect();
            (provider.to_owned(), Value::Object(models))
        })
        .collect();
    Value::Object(providers)
}

async fn authorize(State(state): State<CatalogSync>, request: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.0.storage, request.headers()).await {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        axum::Json(json!({"error":"Unauthorized"})),
    )
        .into_response()
}

async fn status(State(state): State<CatalogSync>) -> Response {
    match tokio::task::spawn_blocking(move || state.status()).await {
        Ok(value) => reply(StatusCode::OK, value),
        Err(_) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"Could not read model catalog status"}),
        ),
    }
}

async fn run(State(state): State<CatalogSync>) -> Response {
    if let Some(result) = state.sync().await {
        return reply(StatusCode::OK, json!({"success":true,"result":result}));
    }
    let error = state
        .state()
        .last_error
        .clone()
        .unwrap_or_else(|| "sync in progress".into());
    reply(StatusCode::SERVICE_UNAVAILABLE, json!({"error":error}))
}

fn reply(status: StatusCode, value: Value) -> Response {
    into_http(Ok(
        json!({"status":status.as_u16(),"headers":{"content-type":"application/json"},"body":value.to_string()}),
    ))
}
