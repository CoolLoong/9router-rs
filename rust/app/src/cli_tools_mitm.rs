use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Map, Value, json};
use url::Url;

use super::{CliTools, reply};
use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::storage::Storage;

#[path = "mitm_certificate.rs"]
pub(crate) mod certificate;
#[path = "mitm_dns.rs"]
mod dns;
#[path = "mitm_lifecycle.rs"]
mod lifecycle;
use lifecycle::Failure as LifecycleError;
pub(crate) use lifecycle::{ListenerConfig, ListenerTask, Runtime, register_listener};

pub(super) const STATUS_PATH: &str = "/api/cli-tools/antigravity-mitm";
pub(super) const ALIAS_PATH: &str = "/api/cli-tools/antigravity-mitm/alias";
const DEFAULT_ROUTER: &str = "http://localhost:20128";

pub(crate) struct Shutdown {
    pub(super) storage: Storage,
    pub(super) runtime: Arc<tokio::sync::Mutex<Runtime>>,
}

impl Shutdown {
    pub(crate) fn watch_recovery(&self) {
        lifecycle::watch_recovery(self.storage.clone(), Arc::downgrade(&self.runtime));
    }

    pub(crate) async fn shutdown(self) {
        if let Err(error) = self.runtime.lock().await.shutdown(&self.storage).await {
            eprintln!("MITM shutdown cleanup failed: {error:?}");
        }
    }
}

pub(super) fn add_routes(router: Router<CliTools>) -> Router<CliTools> {
    router
        .route(
            STATUS_PATH,
            get(status).post(start).delete(stop).patch(patch_status),
        )
        .route(ALIAS_PATH, get(aliases).put(save_aliases))
}

async fn status(State(state): State<CliTools>) -> Response {
    let runtime = state.mitm.lock().await;
    let native_pid = runtime.pid();
    let Some(settings) = state.storage.read_settings().await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to get MITM status",
        );
    };
    let dir = state.storage.root().join("mitm");
    let result = tokio::task::spawn_blocking(move || {
        let pid = native_pid.or_else(|| running_pid(&dir));
        let cert = dir.join(certificate::CA_CERT);
        let cert_exists = cert.is_file();
        let cert_trusted = cert_exists && cert_trusted(&dir, &cert);
        let dns = dns_status(&dir);
        let is_admin = is_admin();
        let has_cached_password = cached_password_saved(&settings["mitmSudoEncrypted"]);
        let needs_sudo = !cfg!(windows) && !is_admin && !has_cached_password && sudo_available();
        let url = if truthy(&settings["mitmRouterBaseUrl"]) {
            js_text(&settings["mitmRouterBaseUrl"])
        } else {
            String::new()
        };
        let url = js_trim(&url);
        json!({
            "running": pid.is_some(),
            "pid": pid,
            "certExists": cert_exists,
            "certTrusted": cert_trusted,
            "dnsStatus": dns,
            "hasCachedPassword": has_cached_password,
            "isWin": cfg!(windows),
            "needsSudoPassword": needs_sudo,
            "isAdmin": is_admin,
            "mitmRouterBaseUrl": if url.is_empty() { DEFAULT_ROUTER } else { url },
        })
    })
    .await;
    match result {
        Ok(value) => reply(StatusCode::OK, value),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to get MITM status",
        ),
    }
}

async fn start(State(state): State<CliTools>, request: Request) -> Response {
    if !lifecycle::available() {
        return error(
            StatusCode::NOT_IMPLEMENTED,
            "MITM is unavailable on this platform",
        );
    }
    if !crate::catalog_guard::is_direct_loopback(&request) {
        return error(
            StatusCode::FORBIDDEN,
            "MITM requires a direct local connection",
        );
    }
    let Ok(bytes) = to_bytes(request.into_body(), 128 * 1024 * 1024).await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to start MITM server",
        );
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to start MITM server",
        );
    };
    let Some(api_key) = body["apiKey"]
        .as_str()
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
    else {
        return error(StatusCode::BAD_REQUEST, "Missing apiKey");
    };
    let url_update = match body
        .get("mitmRouterBaseUrl")
        .filter(|value| !value.is_null())
        .map(normalize_router_url)
        .transpose()
    {
        Ok(update) => update,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let result = tokio::spawn(async move {
        let mut runtime = state.mitm.lock().await;
        let router_base = if let Some((url, _)) = &url_update {
            url.clone()
        } else {
            let settings = state
                .storage
                .read_settings()
                .await
                .ok_or(LifecycleError::State)?;
            let configured = normalize_router_url(&settings["mitmRouterBaseUrl"]);
            configured
                .map(|(url, _)| url)
                .or_else(|_| Url::parse(DEFAULT_ROUTER).map_err(|_| "Invalid MITM router URL"))
                .map_err(|_| LifecycleError::State)?
        };
        runtime
            .start(
                &state.storage,
                router_base,
                url_update.map(|(_, url)| url),
                api_key,
            )
            .await
    })
    .await;
    match result {
        Ok(Ok(pid)) => reply(
            StatusCode::OK,
            json!({"success":true,"running":true,"pid":pid}),
        ),
        Ok(Err(failure)) => lifecycle_error(failure),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to start MITM server",
        ),
    }
}

fn normalize_router_url(input: &Value) -> Result<(Url, String), &'static str> {
    let raw = if input.is_null() {
        String::new()
    } else {
        js_text(input)
    };
    let trimmed = js_trim(&raw);
    let text = if trimmed.is_empty() {
        DEFAULT_ROUTER
    } else {
        trimmed.trim_end_matches('/')
    };
    let url = Url::parse(text).map_err(|_| "Invalid MITM router URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("MITM router URL must use http or https");
    }
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Invalid MITM router URL");
    }
    Ok((url, text.to_owned()))
}

async fn stop(State(state): State<CliTools>, request: Request) -> Response {
    if !lifecycle::available() {
        return error(
            StatusCode::NOT_IMPLEMENTED,
            "MITM is unavailable on this platform",
        );
    }
    if !crate::catalog_guard::is_direct_loopback(&request) {
        return error(
            StatusCode::FORBIDDEN,
            "MITM requires a direct local connection",
        );
    }
    let result =
        tokio::spawn(async move { state.mitm.lock().await.stop(&state.storage).await }).await;
    match result {
        Ok(Ok(())) => reply(StatusCode::OK, json!({"success":true,"running":false})),
        Ok(Err(failure)) => lifecycle_error(failure),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to stop MITM server",
        ),
    }
}

fn lifecycle_error(failure: LifecycleError) -> Response {
    match failure {
        LifecycleError::Unavailable => {
            error(StatusCode::NOT_IMPLEMENTED, "MITM listener is unavailable")
        }
        LifecycleError::ShuttingDown => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "MITM controller is shutting down",
        ),
        LifecycleError::AlreadyRunning => {
            error(StatusCode::CONFLICT, "MITM server is already running")
        }
        LifecycleError::ForeignController => {
            error(StatusCode::CONFLICT, "MITM is managed by another process")
        }
        LifecycleError::PortBusy => reply(
            StatusCode::CONFLICT,
            json!({"error":"Port 443 is already in use", "code":"PORT_443_BUSY", "portOwner":null}),
        ),
        LifecycleError::PortPermission => error(
            StatusCode::FORBIDDEN,
            "The operating system does not permit this gateway to bind the local MITM port",
        ),
        LifecycleError::Bind => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not bind the local MITM listener",
        ),
        LifecycleError::Certificate => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not prepare the local Root CA",
        ),
        LifecycleError::Configure => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not configure the local MITM listener",
        ),
        LifecycleError::State => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not update MITM state",
        ),
        LifecycleError::Dns(failure) => dns_error(failure),
    }
}

async fn patch_status(State(state): State<CliTools>, request: Request) -> Response {
    let Ok(bytes) = to_bytes(request.into_body(), 128 * 1024 * 1024).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to toggle DNS");
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to toggle DNS");
    };
    if !truthy(&body["tool"]) || !truthy(&body["action"]) {
        return error(StatusCode::BAD_REQUEST, "tool and action required");
    }
    let dir = state.storage.root().join("mitm");
    match body["action"].as_str() {
        Some(action @ ("enable" | "disable")) => {
            if !is_admin() {
                return error(
                    StatusCode::FORBIDDEN,
                    "Administrator required to modify DNS",
                );
            }
            let tool = js_text(&body["tool"]);
            let Some(tool) = dns::Tool::parse(&tool) else {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Unknown tool: {tool}"),
                );
            };
            return change_dns(state, dir, tool, action == "enable").await;
        }
        Some("trust-cert") => (),
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "action must be enable, disable, or trust-cert",
            );
        }
    }
    let runtime = state.mitm.lock().await;
    let result = tokio::task::spawn_blocking({
        let editing = state.editing.clone();
        move || {
            let _editing = editing.lock().unwrap_or_else(|error| error.into_inner());
            trust_root(&dir)
        }
    })
    .await;
    drop(runtime);
    match result {
        Ok(Ok(trusted)) => reply(
            StatusCode::OK,
            json!({"success":true,"certTrusted":trusted}),
        ),
        Ok(Err(TrustError::Permission)) => error(
            StatusCode::FORBIDDEN,
            "Administrator required to trust the local Root CA",
        ),
        Ok(Err(TrustError::Missing)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Root CA not found. Start server first to generate it.",
        ),
        Ok(Err(TrustError::Invalid)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Invalid application Root CA",
        ),
        _ => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Certificate install failed",
        ),
    }
}

async fn change_dns(state: CliTools, dir: PathBuf, tool: dns::Tool, enable: bool) -> Response {
    let runtime = state.mitm.lock().await;
    let native_running = runtime.pid().is_some();
    let editing = state.editing.clone();
    let result = state
        .storage
        .run(true, move |db| {
            let _editing = editing.lock().unwrap_or_else(|error| error.into_inner());
            if enable && !native_running && (lifecycle::available() || running_pid(&dir).is_none())
            {
                return Ok(None);
            }
            Ok(Some(dns::change(db, &dir, is_fixture(), tool, enable)))
        })
        .await;
    match result {
        Ok(Some(Ok(status))) => reply(StatusCode::OK, json!({"success":true,"dnsStatus":status})),
        Ok(None) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "MITM server is not running. Start the server first.",
        ),
        Ok(Some(Err(failure))) => dns_error(failure),
        _ => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save DNS state",
        ),
    }
}

fn dns_error(failure: dns::ChangeError) -> Response {
    match failure {
        dns::ChangeError::Conflict => error(
            StatusCode::CONFLICT,
            "The hosts file contains a conflicting entry or changed during the update",
        ),
        dns::ChangeError::ManagedEntries => error(
            StatusCode::FORBIDDEN,
            "Administrator required to clean managed MITM DNS entries",
        ),
        dns::ChangeError::RecoveryChanged => error(
            StatusCode::CONFLICT,
            "MITM configuration changed during DNS recovery",
        ),
        dns::ChangeError::Flush => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "DNS cache refresh failed and the hosts file was restored",
        ),
        dns::ChangeError::Rollback => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not restore the hosts file after the DNS update failed",
        ),
        dns::ChangeError::File => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not update the hosts file",
        ),
        dns::ChangeError::Settings => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save DNS state",
        ),
    }
}

enum TrustError {
    Permission,
    Missing,
    Invalid,
    Install,
}

fn trust_root(dir: &Path) -> Result<bool, TrustError> {
    let path = dir.join(certificate::CA_CERT);
    if !path.exists() {
        return Err(TrustError::Missing);
    }
    let root = certificate::owned_root(dir, std::time::SystemTime::now())
        .map_err(|_| TrustError::Invalid)?;
    if cert_trusted(dir, &path) {
        return Ok(true);
    }
    if !is_admin() {
        if cfg!(target_os = "linux") && !sudo_available() {
            return Ok(false);
        }
        return Err(TrustError::Permission);
    }
    if !certificate::owned_by_controller(dir) {
        return Err(TrustError::Invalid);
    }
    certificate::install(dir, &root, is_fixture()).map_err(|_| TrustError::Install)?;
    if cert_trusted(dir, &path) {
        Ok(true)
    } else {
        Err(TrustError::Install)
    }
}

fn is_fixture() -> bool {
    std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

fn running_pid(dir: &Path) -> Option<u32> {
    let path = dir.join(".mitm.pid");
    if !path.is_file() {
        return None;
    }
    let pid: u32 = fs::read_to_string(path).ok()?.trim().parse().ok()?;
    (pid > 0 && pid <= i32::MAX as u32 && is_mitm_process(pid)).then_some(pid)
}

#[cfg(target_os = "linux")]
fn is_mitm_process(pid: u32) -> bool {
    let matching = |argument: &str| {
        let argument = Path::new(argument);
        argument.ends_with("mitm/server.js")
            || std::env::var_os("MITM_SERVER_PATH").is_some_and(|path| argument == Path::new(&path))
    };
    fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|command| {
        command
            .split(|byte| *byte == 0)
            .skip(1)
            .any(|argument| std::str::from_utf8(argument).is_ok_and(matching))
    })
}

#[cfg(target_os = "macos")]
fn is_mitm_process(pid: u32) -> bool {
    use std::process::Command;
    Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .is_ok_and(|value| {
            value.status.success()
                && String::from_utf8_lossy(&value.stdout).contains("/mitm/server.js")
        })
}

#[cfg(windows)]
fn is_mitm_process(pid: u32) -> bool {
    std::process::Command::new("tasklist.exe")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .is_ok_and(|value| {
            value.status.success()
                && String::from_utf8_lossy(&value.stdout).lines().any(|line| {
                    let mut fields = line.split(',');
                    let binary = fields.next().unwrap_or_default().trim_matches('"');
                    binary.eq_ignore_ascii_case("node.exe")
                        && fields
                            .next()
                            .and_then(|value| value.trim_matches('"').parse::<u32>().ok())
                            == Some(pid)
                })
        })
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn is_mitm_process(_: u32) -> bool {
    false
}

fn dns_status(dir: &Path) -> Map<String, Value> {
    dns::status(dir, is_fixture())
}

#[cfg(target_os = "linux")]
fn cert_trusted(dir: &Path, cert: &Path) -> bool {
    certificate::linux_trusted(dir, cert, is_fixture())
}

#[cfg(any(target_os = "macos", windows))]
fn cert_fingerprint(cert: &Path) -> Option<String> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    use sha1::{Digest, Sha1};
    let pem = fs::read(cert).ok()?;
    let der = CertificateDer::from_pem_slice(&pem).ok()?;
    Some(format!("{:X}", Sha1::digest(der.as_ref())))
}

#[cfg(target_os = "macos")]
fn cert_trusted(_: &Path, cert: &Path) -> bool {
    use std::process::{Command, Stdio};
    const KEYCHAIN: &str = "/Library/Keychains/System.keychain";
    let Some(fingerprint) = cert_fingerprint(cert) else {
        return false;
    };
    let present = Command::new("/usr/bin/security")
        .args([
            "find-certificate",
            "-a",
            "-c",
            "9Router MITM Root CA",
            "-Z",
            KEYCHAIN,
        ])
        .output()
        .is_ok_and(|value| {
            value.status.success()
                && String::from_utf8_lossy(&value.stdout).lines().any(|line| {
                    line.split_once(":").is_some_and(|(name, hash)| {
                        name.eq_ignore_ascii_case("SHA-1 hash")
                            && hash.trim().eq_ignore_ascii_case(&fingerprint)
                    })
                })
        });
    present
        && Command::new("/usr/bin/security")
            .args(["verify-cert", "-c"])
            .arg(cert)
            .args(["-p", "ssl", "-k", KEYCHAIN])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn cert_trusted(_: &Path, cert: &Path) -> bool {
    use std::process::{Command, Stdio};
    cert_fingerprint(cert).is_some_and(|fingerprint| {
        Command::new("certutil.exe")
            .args(["-store", "Root", &fingerprint])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn cert_trusted(_: &Path, _: &Path) -> bool {
    false
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_admin() -> bool {
    rustix::process::getuid().as_raw() == 0
}

#[cfg(windows)]
fn is_admin() -> bool {
    use std::process::{Command, Stdio};
    Command::new("net.exe")
        .arg("session")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn is_admin() -> bool {
    false
}

fn sudo_available() -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join("sudo").is_file())
    })
}

fn cached_password_saved(value: &Value) -> bool {
    value.as_str().is_some_and(|encoded| {
        let fields: Vec<_> = encoded.split(':').collect();
        matches!(&fields[..], [iv, tag, data]
            if iv.len() == 24 && tag.len() == 32 && !data.is_empty() && data.len() % 2 == 0
                && fields.iter().all(|part| part.bytes().all(|byte| byte.is_ascii_hexdigit())))
    })
}

fn read_aliases(db: &Connection, tool: Option<&str>) -> rusqlite::Result<Map<String, Value>> {
    let mut statement = db.prepare(
        "SELECT key,value FROM kv WHERE scope='mitmAlias' AND (?1 IS NULL OR key=?1) ORDER BY rowid",
    )?;
    let rows = statement.query_map([tool], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut aliases = Map::new();
    for row in rows {
        let (key, value) = row?;
        aliases.insert(key, serde_json::from_str(&value).unwrap_or_default());
    }
    Ok(aliases)
}

async fn aliases(State(state): State<CliTools>, request: Request) -> Response {
    let tool = url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
        .find_map(|(key, value)| (key == "tool").then(|| value.into_owned()))
        .filter(|tool| !tool.is_empty());
    match state
        .storage
        .run(false, move |db| {
            let mut aliases = read_aliases(db, tool.as_deref())?;
            Ok(match tool {
                Some(tool) => aliases
                    .shift_remove(&tool)
                    .filter(truthy)
                    .unwrap_or_else(|| json!({})),
                None => Value::Object(aliases),
            })
        })
        .await
    {
        Ok(aliases) => reply(StatusCode::OK, json!({"aliases":aliases})),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch aliases"),
    }
}

async fn save_aliases(State(state): State<CliTools>, request: Request) -> Response {
    let failed = || error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to save aliases");
    let Ok(bytes) = to_bytes(request.into_body(), 128 * 1024 * 1024).await else {
        return failed();
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return failed();
    };
    if body.is_null() {
        return failed();
    }
    let mappings = match &body["mappings"] {
        Value::Object(values) => values.clone(),
        Value::Array(values) => values
            .iter()
            .enumerate()
            .map(|(index, value)| (index.to_string(), value.clone()))
            .collect(),
        _ => return error(StatusCode::BAD_REQUEST, "tool and mappings required"),
    };
    if !truthy(&body["tool"]) {
        return error(StatusCode::BAD_REQUEST, "tool and mappings required");
    }
    let tool = js_text(&body["tool"]);
    let dir = state.storage.root().join("mitm");
    let editing = state.editing.clone();
    let result = state.storage.run(true, move |db| {
        let _editing = editing.lock().unwrap_or_else(|error| error.into_inner());
        if dns_status(&dir).get(&tool) != Some(&Value::Bool(true)) {
                return Ok(AliasUpdate::Inactive(tool));
        }
        let mut filtered = Map::new();
        for (alias, model) in mappings.into_iter().filter(|(_, model)| truthy(model)) {
            let Some(model) = model.as_str().map(js_trim) else { return Ok(AliasUpdate::Invalid); };
            if !model.is_empty() {
                filtered.insert(alias, json!(model));
            }
        }
        let filtered = Value::Object(filtered);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO kv(scope,key,value) VALUES('mitmAlias',?1,?2) ON CONFLICT(scope,key) DO UPDATE SET value=excluded.value",
            [&tool,&filtered.to_string()],
        )?;
        let aliases = read_aliases(&tx, None)?;
        tx.commit()?;
        if write_cache(&dir, &aliases).is_err() {
            eprintln!("Could not refresh MITM alias cache");
        }
        Ok(AliasUpdate::Saved(filtered))
    }).await;
    match result {
        Ok(AliasUpdate::Inactive(tool)) => error(
            StatusCode::FORBIDDEN,
            &format!("DNS must be enabled for {tool} before editing model mappings"),
        ),
        Ok(AliasUpdate::Saved(value)) => {
            reply(StatusCode::OK, json!({"success":true,"aliases":value}))
        }
        _ => failed(),
    }
}

enum AliasUpdate {
    Inactive(String),
    Invalid,
    Saved(Value),
}

fn write_cache(dir: &Path, aliases: &Map<String, Value>) -> io::Result<()> {
    if fs::symlink_metadata(dir).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(file.as_file_mut(), aliases)?;
    file.persist(dir.join("aliases.json"))
        .map_err(|error| error.error)?;
    Ok(())
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}))
}
