use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use regex::Regex;
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::{Mutex, oneshot, watch};

use crate::{catalog_guard, cli_connect, dashboard_session, settings, storage::Storage};

#[path = "cloudflared_install.rs"]
mod install;
#[path = "cloudflare_legacy_state.rs"]
mod legacy;

const UNVERIFIED: &str = "Existing Quick Tunnel ownership could not be verified";
const WORKER: &str = "https://abc-tunnel.us";
const FIXTURE: &str = "http://127.0.0.1:52989/cloudflare-quick";
static TUNNEL_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)https://([a-z0-9-]+)\.trycloudflare\.com").unwrap());

#[derive(Clone)]
pub struct Manager(Arc<Tunnel>);

struct Tunnel {
    storage: Storage,
    origin_port: u16,
    http: reqwest::Client,
    operation: Mutex<()>,
    running: Mutex<Option<Worker>>,
    epoch: watch::Sender<u64>,
    installer: install::Installer,
    closing: AtomicBool,
}

struct Worker {
    urls: watch::Receiver<Option<String>>,
    stop: oneshot::Sender<()>,
    done: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct StateData {
    manager: Manager,
    tailscale: crate::tailscale_api::Daemon,
}

impl Manager {
    pub fn new(storage: Storage, origin_port: u16) -> Self {
        Self(Arc::new(Tunnel {
            storage,
            origin_port,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("quick tunnel HTTP client"),
            operation: Mutex::new(()),
            running: Mutex::new(None),
            epoch: watch::channel(0).0,
            installer: install::Installer::new(),
            closing: AtomicBool::new(false),
        }))
    }

    pub async fn shutdown(&self) {
        self.0.closing.store(true, Ordering::Release);
        self.0.cancel();
        let _update = self.0.operation.lock().await;
        self.0.stop_native().await;
    }

    pub fn watch(&self) {
        let weak = Arc::downgrade(&self.0);
        let delay = if fixture() {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(30)
        };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(delay).await;
                let Some(tunnel) = weak.upgrade() else {
                    return;
                };
                if tunnel.closing.load(Ordering::Acquire) {
                    return;
                }
                let manager = Manager(tunnel);
                manager.resume().await;
            }
        });
    }

    async fn resume(&self) {
        let tunnel = &self.0;
        let Ok(_operation) = tunnel.operation.try_lock() else {
            return;
        };
        let Some(config) = tunnel.storage.read_settings().await else {
            return;
        };
        let should_resume = config["tunnelEnabled"] == true
            && !tunnel
                .running
                .lock()
                .await
                .as_ref()
                .is_some_and(|worker| !worker.done.is_finished());
        if !should_resume {
            return;
        }
        let manageable = tunnel.prepare_change(&config).await.is_ok();
        if manageable && self.activate(tunnel.epoch.subscribe()).await.is_err() {
            crate::console_logs::record("WARN", "Quick tunnel restart failed");
        }
    }
}

pub fn router(manager: Manager, tailscale: crate::tailscale_api::Daemon) -> Router {
    let state = StateData { manager, tailscale };
    Router::new()
        .route("/api/tunnel/status", get(status))
        .route("/api/tunnel/enable", post(enable))
        .route("/api/tunnel/disable", post(disable))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<StateData>, request: Request, next: Next) -> Response {
    let storage = &state.manager.0.storage;
    let cli = catalog_guard::valid_cli_token(storage, request.headers());
    let authenticated = dashboard_session::authenticated(storage, request.headers()).await;
    let local_only = request.uri().path() != "/api/tunnel/status";
    if local_only && !cli && !(authenticated && catalog_guard::is_direct_loopback(&request)) {
        return rejected(StatusCode::FORBIDDEN, "Local only: CLI token required");
    }
    if authenticated {
        next.run(request).await
    } else {
        rejected(StatusCode::UNAUTHORIZED, "Unauthorized")
    }
}

fn rejected(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::VARY, "Accept-Encoding")],
        axum::Json(json!({"error":message})),
    )
        .into_response()
}

async fn status(State(state): State<StateData>) -> Response {
    let tunnel = &state.manager.0;
    let Some(config) = tunnel.storage.read_settings().await else {
        return failed("Could not read tunnel settings");
    };
    let tailscale_enabled = config["tailscaleEnabled"] == true;
    let tailscale = if tailscale_enabled {
        crate::tailscale_api::owned_status(&tunnel.storage, &state.tailscale).await
    } else {
        None
    };
    let tailscale = tailscale.unwrap_or_else(|| json!({
        "enabled":false,"settingsEnabled":tailscale_enabled,
        "tunnelUrl":config["tailscaleUrl"].as_str().unwrap_or_default(),"running":false,"loggedIn":false,
    }));
    let current = tunnel.state();
    let running = tunnel
        .running
        .lock()
        .await
        .as_ref()
        .is_some_and(|worker| !worker.done.is_finished());
    let settings_enabled = config["tunnelEnabled"] == true;
    let short = current["shortId"].as_str().unwrap_or_default();
    settings::reply(
        StatusCode::OK,
        json!({
            "tunnel":{
                "enabled":settings_enabled && running,
                "settingsEnabled":settings_enabled,
                "tunnelUrl":current["tunnelUrl"].as_str().unwrap_or_default(),
                "shortId":short,
                "publicUrl":if short.is_empty() { String::new() } else { public_url(short) },
                "running":settings_enabled && running,
            },
            "tailscale":tailscale,
            "download":tunnel.installer.status(),
        }),
        false,
    )
}

async fn enable(State(state): State<StateData>) -> Response {
    let tunnel = &state.manager.0;
    let mut cancel = tunnel.epoch.subscribe();
    let generation = *cancel.borrow_and_update();
    let _update = tunnel.operation.lock().await;
    if *cancel.borrow() != generation {
        return failed("tunnel cancelled");
    }
    let Some(config) = tunnel.storage.read_settings().await else {
        return failed("Could not read tunnel settings");
    };
    if tunnel.prepare_change(&config).await.is_err() {
        return rejected(StatusCode::CONFLICT, UNVERIFIED);
    }
    match state.manager.activate(cancel).await {
        Ok((short, url, reused)) => enabled(&short, &url, reused),
        Err(error) => failed(error),
    }
}

impl Manager {
    async fn activate(
        &self,
        mut cancel: watch::Receiver<u64>,
    ) -> Result<(String, String, bool), &'static str> {
        let tunnel = &self.0;
        if tunnel.closing.load(Ordering::Acquire) {
            return Err("tunnel cancelled");
        }
        if tunnel
            .running
            .lock()
            .await
            .as_ref()
            .is_some_and(|worker| !worker.done.is_finished())
        {
            let data = tunnel.state();
            if let (Some(short), Some(url)) = (data["shortId"].as_str(), data["tunnelUrl"].as_str())
            {
                return Ok((short.into(), url.into(), true));
            }
        }
        let binary = if let Some(binary) = tunnel.installed() {
            binary
        } else if fixture() {
            return Err("Missing simulated cloudflared binary");
        } else {
            tunnel
                .installer
                .ensure(&tunnel.storage.root().join("bin"), &mut cancel)
                .await?
        };
        tunnel.stop_native().await;
        let current = tunnel.state();
        let short = short_id(&current).ok_or("Could not generate tunnel ID")?;
        let mut worker = spawn(&binary, tunnel.origin_port)?;
        let activate = async {
            let url = wait_url(&mut worker.urls).await?;
            tunnel.register(&short, &url).await?;
            tunnel.health(&short).await?;
            Ok::<_, &'static str>(url)
        };
        let activated = tokio::select! {
            result = activate => result,
            _ = cancel.changed() => Err("tunnel cancelled"),
        };
        let url = match activated {
            Ok(url) => url,
            Err(error) => {
                worker.stop().await;
                return Err(error);
            }
        };
        if worker.done.is_finished() {
            worker.stop().await;
            return Err("cloudflared exited before the tunnel became ready");
        }
        if let Err(error) = tunnel.persist(&short, Some(&url)).await {
            worker.stop().await;
            return Err(error);
        }
        let updates = worker.urls.clone();
        *tunnel.running.lock().await = Some(worker);
        monitor(self, short.clone(), url.clone(), updates, cancel);
        Ok((short, url, false))
    }
}

async fn disable(State(state): State<StateData>) -> Response {
    let tunnel = &state.manager.0;
    tunnel.cancel();
    let _update = tunnel.operation.lock().await;
    let Some(config) = tunnel.storage.read_settings().await else {
        return failed("Could not read tunnel settings");
    };
    if tunnel.prepare_change(&config).await.is_err() {
        return rejected(StatusCode::CONFLICT, UNVERIFIED);
    }
    tunnel.stop_native().await;
    let data = tunnel.state();
    match tunnel
        .persist(data["shortId"].as_str().unwrap_or_default(), None)
        .await
    {
        Ok(()) => settings::reply(StatusCode::OK, json!({"success":true}), false),
        Err(error) => failed(error),
    }
}

impl Tunnel {
    fn cancel(&self) {
        self.epoch
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    async fn stop_native(&self) {
        if let Some(worker) = self.running.lock().await.take() {
            worker.stop().await;
        }
    }

    async fn prepare_change(&self, settings: &Value) -> Result<(), ()> {
        if self.running.lock().await.is_some() {
            return Ok(());
        }
        let root = self.storage.root();
        legacy::check_directory(root).map_err(|_| ())?;
        if self.owns_saved_state() {
            return Ok(());
        }
        if settings["tunnelEnabled"] == true {
            let path = self.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|_| ())?;
            if !metadata.is_file() || metadata.len() > 16_384 {
                return Err(());
            }
            let state = self.state();
            let is_original = state.get("manager").is_none()
                && state["shortId"].as_str().is_some_and(valid_short_id)
                && state["tunnelUrl"].as_str().is_some_and(|url| {
                    TUNNEL_URL
                        .find(url)
                        .is_some_and(|matched| matched.as_str() == url)
                })
                && state["tunnelUrl"] == settings["tunnelUrl"];
            if !is_original {
                return Err(());
            }
        }
        legacy::clear_expired_marker(root).map_err(|_| ())
    }

    fn owns_saved_state(&self) -> bool {
        self.state()["manager"] == "rust" && legacy::marker_absent(self.storage.root())
    }

    fn installed(&self) -> Option<PathBuf> {
        let name = if cfg!(windows) {
            "cloudflared.exe"
        } else {
            "cloudflared"
        };
        let directory = self.storage.root().join("bin");
        let bundled = directory.join(name);
        if fixture() && bundled.is_file() {
            return Some(bundled);
        }
        if let Some(binary) = self.installer.installed(&directory) {
            return Some(binary);
        }
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|entry| entry.join(name))
                .find(|entry| entry.is_absolute() && entry.is_file())
        })
    }

    fn path(&self) -> PathBuf {
        self.storage.root().join("tunnel/state.json")
    }

    fn state(&self) -> Value {
        shared_state(&self.storage)
    }

    async fn persist(&self, short: &str, url: Option<&str>) -> Result<(), &'static str> {
        let path = self.path();
        let url = url.map(str::to_owned);
        let short = short.to_owned();
        let saved = std::fs::read(&path).ok();
        self.storage.run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let raw: Option<String> = tx.query_row("SELECT data FROM settings WHERE id = 1", [], |row| row.get(0)).optional()?;
            let mut data = raw.and_then(|raw| serde_json::from_str(&raw).ok())
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            data["tunnelEnabled"] = json!(url.is_some());
            data["tunnelUrl"] = json!(url.as_deref().unwrap_or_default());
            tx.execute("INSERT INTO settings(id,data) VALUES (1,?) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
                [data.to_string()])?;
            if url.is_some() || !short.is_empty() || saved.is_some() {
                let mut next = json!({"shortId":short,"tunnelUrl":url});
                if url.is_some() {
                    next["manager"] = json!("rust");
                }
                let next = next.to_string();
                cli_connect::write_private(&path, next.as_bytes(), false).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            }
            if let Err(error) = tx.commit() {
                if let Some(saved) = saved {
                    let _ = cli_connect::write_private(&path, &saved, false);
                } else {
                    let _ = std::fs::remove_file(path);
                }
                return Err(error);
            }
            Ok(())
        }).await.map_err(|_| "Could not save tunnel state")
    }

    async fn register(&self, short: &str, url: &str) -> Result<(), &'static str> {
        let base = if fixture() {
            FIXTURE.to_owned()
        } else {
            std::env::var("TUNNEL_WORKER_URL")
                .ok()
                .filter(|url| !url.is_empty())
                .unwrap_or_else(|| WORKER.to_owned())
        };
        self.http
            .post(format!(
                "{}/api/tunnel/register",
                base.trim_end_matches('/')
            ))
            .json(&json!({"shortId":short,"tunnelUrl":url}))
            .send()
            .await
            .ok()
            .filter(|response| response.status().is_success())
            .map(|_| ())
            .ok_or("Tunnel registration failed")
    }

    async fn health(&self, short: &str) -> Result<(), &'static str> {
        let url = if fixture() {
            format!("{FIXTURE}/api/health")
        } else {
            format!("{}/api/health", public_url(short))
        };
        let interval = if fixture() {
            Duration::from_millis(10)
        } else {
            Duration::from_secs(2)
        };
        let timeout = if fixture() {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(60)
        };
        tokio::time::timeout(timeout, async {
            loop {
                if self
                    .http
                    .get(&url)
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success())
                {
                    return;
                }
                tokio::time::sleep(interval).await;
            }
        })
        .await
        .map_err(|_| "Health check timeout")
    }
}

fn monitor(
    manager: &Manager,
    short: String,
    mut last: String,
    mut updates: watch::Receiver<Option<String>>,
    mut cancel: watch::Receiver<u64>,
) {
    let weak = Arc::downgrade(&manager.0);
    let generation = *cancel.borrow_and_update();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.changed() => return,
                update = updates.changed() => if update.is_err() { return; },
            }
            let Some(url) = updates
                .borrow_and_update()
                .clone()
                .filter(|url| url != &last)
            else {
                continue;
            };
            let Some(tunnel) = weak.upgrade() else {
                return;
            };
            let _operation = tunnel.operation.lock().await;
            if *cancel.borrow() != generation {
                return;
            }
            let updated = tokio::select! {
                result = tunnel.register(&short, &url) => result.is_ok(),
                _ = cancel.changed() => return,
            };
            if updated && tunnel.persist(&short, Some(&url)).await.is_ok() {
                last = url;
            }
        }
    });
}

impl Worker {
    async fn stop(self) {
        let _ = self.stop.send(());
        let mut done = self.done;
        if tokio::time::timeout(Duration::from_secs(3), &mut done)
            .await
            .is_err()
        {
            done.abort();
            let _ = done.await;
        }
    }
}

fn spawn(binary: &Path, port: u16) -> Result<Worker, &'static str> {
    let work = tempfile::Builder::new()
        .prefix("9router-cloudflare-")
        .tempdir()
        .map_err(|_| "Could not create tunnel configuration")?;
    let config = work.path().join("config.yml");
    std::fs::write(&config, b"{}\n").map_err(|_| "Could not create tunnel configuration")?;
    let protocol = std::env::var("TUNNEL_TRANSPORT_PROTOCOL")
        .or_else(|_| std::env::var("CLOUDFLARED_PROTOCOL"))
        .map(|value| value.trim().to_ascii_lowercase())
        .ok()
        .filter(|value| matches!(value.as_str(), "http2" | "quic" | "auto"))
        .unwrap_or_else(|| "http2".into());
    let mut command = Command::new(binary);
    command
        .arg("tunnel")
        .arg("--url")
        .arg(format!("http://127.0.0.1:{port}"))
        .arg("--config")
        .arg(config)
        .args(["--no-autoupdate", "--retries", "99"])
        .env("TUNNEL_TRANSPORT_PROTOCOL", protocol)
        .current_dir(work.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| "Could not start cloudflared")?;
    let (reported, urls) = watch::channel(None);
    let stdout = tokio::spawn(read_urls(
        child.stdout.take().expect("cloudflared stdout"),
        reported.clone(),
    ));
    let stderr = tokio::spawn(read_urls(
        child.stderr.take().expect("cloudflared stderr"),
        reported,
    ));
    let (stop, stopped) = oneshot::channel();
    let done = tokio::spawn(async move {
        tokio::select! {
            _ = stopped => { let _ = child.kill().await; }
            _ = child.wait() => {}
        }
        stdout.abort();
        stderr.abort();
        drop(work);
    });
    Ok(Worker { urls, stop, done })
}

async fn read_urls(mut pipe: impl AsyncRead + Unpin, found: watch::Sender<Option<String>>) {
    let mut bytes = [0u8; 2048];
    let mut tail = Vec::new();
    while let Ok(size @ 1..) = pipe.read(&mut bytes).await {
        tail.extend_from_slice(&bytes[..size]);
        let text = String::from_utf8_lossy(&tail);
        if let Some(host) = TUNNEL_URL
            .captures_iter(&text)
            .filter_map(|part| part.get(1))
            .map(|part| part.as_str().to_ascii_lowercase())
            .filter(|host| host != "api")
            .last()
        {
            found.send_replace(Some(format!("https://{host}.trycloudflare.com")));
        }
        if tail.len() > 1024 {
            tail.drain(..tail.len() - 1024);
        }
    }
}

async fn wait_url(urls: &mut watch::Receiver<Option<String>>) -> Result<String, &'static str> {
    let timeout = if fixture() {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(90)
    };
    tokio::time::timeout(timeout, urls.wait_for(Option::is_some))
        .await
        .map_err(|_| "Quick tunnel timed out")?
        .map_err(|_| "cloudflared exited before publishing a tunnel URL")?
        .as_ref()
        .cloned()
        .ok_or("cloudflared returned no tunnel URL")
}

fn fixture() -> bool {
    std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

pub(crate) fn shared_state(storage: &Storage) -> Value {
    std::fs::read(storage.root().join("tunnel/state.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

pub(crate) fn short_id(current: &Value) -> Option<String> {
    current["shortId"]
        .as_str()
        .filter(|id| valid_short_id(id))
        .map(str::to_owned)
        .or_else(new_short_id)
}

fn new_short_id() -> Option<String> {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).ok()?;
    let alphabet = b"abcdefghijklmnpqrstuvwxyz23456789";
    Some(
        bytes
            .iter()
            .map(|byte| alphabet[*byte as usize % alphabet.len()] as char)
            .collect(),
    )
}

fn valid_short_id(id: &str) -> bool {
    id.len() == 6
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn public_url(short: &str) -> String {
    format!("https://r{short}.abc-tunnel.us")
}

fn enabled(short: &str, url: &str, reused: bool) -> Response {
    let mut result =
        json!({"success":true,"tunnelUrl":url,"shortId":short,"publicUrl":public_url(short)});
    if reused {
        result["alreadyRunning"] = json!(true);
    }
    settings::reply(StatusCode::OK, result, false)
}

fn failed(message: &str) -> Response {
    settings::reply(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":message}),
        false,
    )
}
