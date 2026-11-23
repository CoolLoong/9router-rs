use std::fs;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path as FilePath, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use url::Url;

use crate::catalog_guard::{is_direct_loopback, valid_cli_token};
use crate::dashboard_session;
use crate::settings::reply;
use crate::storage::Storage;

#[cfg(any(windows, target_os = "macos"))]
#[path = "tailscale_desktop_install.rs"]
mod install_desktop;
#[cfg(target_os = "linux")]
#[path = "tailscale_install_linux.rs"]
mod install_linux;
#[cfg(target_os = "linux")]
#[path = "tailscale_owned_daemon.rs"]
mod owned_daemon;
#[cfg(target_os = "linux")]
pub(crate) use owned_daemon::{Daemon, daemon_entry};
#[cfg(not(target_os = "linux"))]
#[derive(Clone, Default)]
pub(crate) struct Daemon;

const SYSTEM_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";
const HTTPS_PORT: &str = "443";
const HTTPS_FLAG: &str = "--https=443";
const ROOT_FLAG: &str = "--set-path=/";
const PROBE: Duration = Duration::from_millis(1500);
const CHANGE: Duration = Duration::from_secs(30);
const MAX_OUTPUT: u64 = 2 * 1024 * 1024;

#[derive(Clone)]
struct Tailscale {
    storage: Storage,
    listener: SocketAddr,
    operation: Arc<Mutex<()>>,
    daemon: Daemon,
}

pub fn router(storage: Storage, listener: SocketAddr, daemon: Daemon) -> Router {
    let state = Tailscale {
        storage,
        listener,
        operation: Arc::new(Mutex::new(())),
        daemon,
    };
    let fixture = std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1");
    #[cfg(target_os = "linux")]
    let fixture_delay = fixture && state.storage.root().join("bin/tailscaled").is_file();
    #[cfg(not(target_os = "linux"))]
    let fixture_delay = false;
    if !fixture || fixture_delay {
        let monitor = state.clone();
        let interval = if fixture_delay {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(30)
        };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                restore_owned(&monitor).await;
            }
        });
    }
    let router = Router::new()
        .route("/api/tunnel/tailscale-check", get(check))
        .route("/api/tunnel/tailscale-enable", post(enable))
        .route("/api/tunnel/tailscale-disable", post(disable));
    #[cfg(target_os = "linux")]
    let router = router.route(
        "/api/tunnel/tailscale-install",
        post(install_linux::install),
    );
    #[cfg(any(windows, target_os = "macos"))]
    let router = router.route(
        "/api/tunnel/tailscale-install",
        post(install_desktop::install),
    );
    router
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<Tailscale>, request: Request, next: Next) -> Response {
    if valid_cli_token(&state.storage, request.headers())
        || is_direct_loopback(&request)
            && dashboard_session::authenticated(&state.storage, request.headers()).await
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

fn error(status: StatusCode, message: &'static str) -> Response {
    reply(status, json!({"error":message}), false)
}

fn conflict() -> Response {
    error(
        StatusCode::CONFLICT,
        "Tailscale HTTPS port 443 contains a configuration that 9Router cannot verify as its own",
    )
}

fn unavailable() -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Could not verify the Tailscale system daemon",
    )
}

fn joined(url: impl Into<String>) -> Response {
    reply(
        StatusCode::OK,
        json!({"success":true,"tunnelUrl":url.into()}),
        false,
    )
}

fn removed() -> Response {
    reply(StatusCode::OK, json!({"success":true}), false)
}

fn custom_socket(storage: &Storage) -> PathBuf {
    storage.root().join("tailscale/tailscaled.sock")
}

fn owner_path(storage: &Storage) -> PathBuf {
    storage.root().join("tailscale/native-funnel.json")
}

#[cfg(target_os = "linux")]
enum OperationError {
    Persistence,
    Busy,
}

#[cfg(target_os = "linux")]
impl OperationError {
    fn response(self) -> Response {
        match self {
            Self::Persistence => persistence_error(),
            Self::Busy => error(
                StatusCode::CONFLICT,
                "Another gateway Funnel operation is in progress",
            ),
        }
    }
}

#[cfg(target_os = "linux")]
fn claim_operation(storage: &Storage) -> Result<rustix::fd::OwnedFd, OperationError> {
    use std::os::unix::fs::DirBuilderExt;

    use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};
    let directory = storage.root().join("tailscale");
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(OperationError::Persistence),
    }
    if !fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(OperationError::Persistence);
    }
    let file = open(
        directory.join("native-funnel.lock"),
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|_| OperationError::Persistence)?;
    let mode = rustix::fs::fstat(&file)
        .map_err(|_| OperationError::Persistence)?
        .st_mode;
    if rustix::fs::FileType::from_raw_mode(mode) != rustix::fs::FileType::RegularFile {
        return Err(OperationError::Persistence);
    }
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(rustix::io::Errno::AGAIN) => Err(OperationError::Busy),
        Err(_) => Err(OperationError::Persistence),
    }
}

fn command_path(name: &str) -> Option<PathBuf> {
    let mut directories: Vec<_> = if cfg!(windows) {
        vec![PathBuf::from(r"C:\Program Files\Tailscale")]
    } else {
        [
            "/usr/local/bin",
            "/opt/homebrew/bin",
            "/usr/sbin",
            "/usr/bin",
            "/bin",
            "/snap/bin",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    };
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path));
    }
    directories
        .into_iter()
        .map(|directory| directory.join(name))
        .find(|path| executable(path))
}

fn executable(path: &FilePath) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    metadata.is_file()
}

fn binary(storage: &Storage) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "tailscale.exe"
    } else {
        "tailscale"
    };
    let local = storage.root().join("bin").join(name);
    if executable(&local) {
        return Some(local);
    }
    #[cfg(any(windows, target_os = "macos"))]
    if let Some(path) = install_desktop::installed() {
        return Some(path);
    }
    command_path(name)
}

fn exists_workspace_socket(storage: &Storage) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        fs::symlink_metadata(custom_socket(storage))
            .is_ok_and(|metadata| metadata.file_type().is_socket())
    }
    #[cfg(not(unix))]
    {
        let _ = storage;
        false
    }
}

struct Output {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn invoke(
    binary: &FilePath,
    socket: Option<&FilePath>,
    args: &[&str],
    wait: Duration,
) -> Option<Output> {
    let mut command = Command::new(binary);
    #[cfg(target_os = "macos")]
    command.env("TAILSCALE_BE_CLI", "1");
    if let Some(socket) = socket {
        command.arg("--socket").arg(socket);
    }
    let mut child = command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?.take(MAX_OUTPUT + 1);
    let mut stderr = child.stderr.take()?.take(MAX_OUTPUT + 1);
    let (mut output, mut errors) = (Vec::new(), Vec::new());
    let result = tokio::time::timeout(wait, async {
        tokio::try_join!(
            child.wait(),
            stdout.read_to_end(&mut output),
            stderr.read_to_end(&mut errors)
        )
    })
    .await;
    let success = match result {
        Ok(Ok((status, bytes, error_bytes))) => {
            if bytes as u64 > MAX_OUTPUT || error_bytes as u64 > MAX_OUTPUT {
                return None;
            }
            status.success()
        }
        _ => {
            let _ = child.kill().await;
            false
        }
    };
    Some(Output {
        success,
        stdout: output,
        stderr: errors,
    })
}

fn system_socket() -> Option<&'static FilePath> {
    cfg!(target_os = "linux").then_some(FilePath::new(SYSTEM_SOCKET))
}

async fn json_command(
    binary: &FilePath,
    socket: Option<&FilePath>,
    args: &[&str],
) -> Option<Value> {
    let output = invoke(binary, socket, args, PROBE).await?;
    if !output.success {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

async fn workspace_status(state: &Tailscale, bin: &FilePath) -> Option<Value> {
    if !exists_workspace_socket(&state.storage) {
        return None;
    }
    status(bin, Some(&custom_socket(&state.storage))).await
}

impl Daemon {
    async fn userspace_route(&self) -> Option<DaemonRoute> {
        #[cfg(target_os = "linux")]
        return self.socket().await.map(DaemonRoute::Userspace);
        #[cfg(not(target_os = "linux"))]
        return None;
    }
}

fn online(node: &Value) -> bool {
    node["BackendState"] == "Running" && node["Self"]["Online"] == true
}

async fn check(State(state): State<Tailscale>) -> Response {
    #[cfg(target_os = "linux")]
    let has_cached_password = state
        .storage
        .read_settings()
        .await
        .and_then(|settings| install_linux::cached_password(&settings))
        .is_some();
    #[cfg(not(target_os = "linux"))]
    let has_cached_password = false;
    let candidate = binary(&state.storage);
    let (system, workspace) = if let Some(bin) = &candidate {
        let workspace = async {
            if let Some(route) = state.daemon.userspace_route().await {
                status(bin, route.socket()).await
            } else {
                workspace_status(&state, bin).await
            }
        };
        tokio::join!(system_status(bin), workspace)
    } else {
        (None, None)
    };
    let customs = workspace.is_some();
    let system_running = system
        .as_ref()
        .is_some_and(|node| node["BackendState"] == "Running");
    let logged_in = workspace.as_ref().or(system.as_ref()).is_some_and(online);
    let platform = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(windows) {
        "win32"
    } else {
        std::env::consts::OS
    };
    #[cfg(target_os = "macos")]
    let brew_available = install_desktop::brew_available();
    #[cfg(not(target_os = "macos"))]
    let brew_available = false;
    reply(
        StatusCode::OK,
        json!({
            "installed":candidate.is_some(),"loggedIn":logged_in,"platform":platform,
            "brewAvailable":brew_available,
            "daemonRunning":customs || system_running,"customDaemonRunning":customs,
            "systemDaemonRunning":system_running,"hasCachedPassword":has_cached_password,
        }),
        false,
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Backend {
    System,
    Userspace,
}

enum DaemonRoute {
    System,
    #[cfg(target_os = "linux")]
    Userspace(PathBuf),
}

impl DaemonRoute {
    fn backend(&self) -> Backend {
        match self {
            Self::System => Backend::System,
            #[cfg(target_os = "linux")]
            Self::Userspace(_) => Backend::Userspace,
        }
    }

    fn socket(&self) -> Option<&FilePath> {
        match self {
            Self::System => system_socket(),
            #[cfg(target_os = "linux")]
            Self::Userspace(socket) => Some(socket),
        }
    }
}

#[derive(PartialEq)]
struct Owner {
    node: String,
    hostname: String,
    target: String,
    backend: Backend,
}

impl Owner {
    fn url(&self) -> String {
        format!("https://{}", self.hostname)
    }

    fn authority(&self) -> String {
        format!("{}:{HTTPS_PORT}", self.hostname)
    }

    fn current_node(node: &Value) -> Option<(&str, String)> {
        let node_id = node["Self"]["ID"]
            .as_str()
            .filter(|value| !value.is_empty())?;
        Some((node_id, hostname(node["Self"]["DNSName"].as_str()?)?))
    }

    fn belongs_to(&self, node: &Value) -> bool {
        Self::current_node(node).is_some_and(|(id, name)| id == self.node && name == self.hostname)
    }

    fn parse(value: &Value) -> Option<Self> {
        let target = value["target"].as_str()?;
        let url = Url::parse(target).ok()?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]"));
        if value["version"] != 1
            || !loopback
            || url.scheme() != "http"
            || url.path() != "/"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return None;
        }
        Some(Self {
            node: value["node"]
                .as_str()
                .filter(|node| !node.is_empty())?
                .into(),
            hostname: hostname(value["hostname"].as_str()?)?,
            target: target.into(),
            backend: match value.get("backend") {
                None => Backend::System,
                Some(value) if value == "userspace" => Backend::Userspace,
                Some(_) => return None,
            },
        })
    }
}

fn hostname(value: &str) -> Option<String> {
    let host = value
        .strip_suffix('.')
        .unwrap_or(value)
        .to_ascii_lowercase();
    if host.len() > 253
        || !host.ends_with(".ts.net")
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return None;
    }
    Some(host)
}

fn read_owner(storage: &Storage) -> io::Result<Option<Owner>> {
    let file = owner_path(storage);
    let bytes = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| Owner::parse(&value))
        .map(Some)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid Tailscale owner record"))
}

fn record(storage: &Storage, owner: &Owner) -> io::Result<()> {
    let path = owner_path(storage);
    let directory = path.parent().expect("Tailscale state directory");
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    let mut data = json!({
        "version":1,"node":owner.node,"hostname":owner.hostname,"target":owner.target,
    });
    if owner.backend == Backend::Userspace {
        data["backend"] = json!("userspace");
    }
    temporary.write_all(data.to_string().as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path).map_err(|error| error.error)?;
    #[cfg(unix)]
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

fn clear(storage: &Storage) -> io::Result<()> {
    match fs::remove_file(owner_path(storage)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn local_target(listener: SocketAddr) -> Option<String> {
    let address = match listener.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(ip) if ip.is_unspecified() => std::net::Ipv6Addr::LOCALHOST.into(),
        ip => ip.to_canonical(),
    };
    if address != IpAddr::V4(Ipv4Addr::LOCALHOST)
        && address != IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
    {
        return None;
    }
    Some(format!(
        "http://{}",
        SocketAddr::new(address, listener.port())
    ))
}

#[derive(PartialEq)]
enum Endpoint {
    Empty,
    Owned,
    Conflict,
}

fn configured_endpoint(config: &Value, owner: &Owner) -> Endpoint {
    if !config.is_object() && !config.is_null() {
        return Endpoint::Conflict;
    }
    for name in ["TCP", "Web", "AllowFunnel", "Foreground"] {
        if !config[name].is_null() && !config[name].is_object() {
            return Endpoint::Conflict;
        }
    }
    if config["Foreground"]
        .as_object()
        .is_some_and(|jobs| !jobs.is_empty())
    {
        return Endpoint::Conflict;
    }
    let used = |field: &str| {
        config[field]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| {
                key.rsplit_once(':')
                    .is_some_and(|(_, port)| port == HTTPS_PORT)
            })
            .map(|(key, _)| key)
            .collect::<Vec<_>>()
    };
    let web = used("Web");
    let public = used("AllowFunnel");
    let tls = &config["TCP"][HTTPS_PORT];
    if tls.is_null() && web.is_empty() && public.is_empty() {
        return Endpoint::Empty;
    }
    let host = owner.authority();
    if *tls == json!({"HTTPS":true})
        && web.len() == 1
        && web[0] == &host
        && public.len() == 1
        && public[0] == &host
        && config["AllowFunnel"][&host] == true
        && config["Web"][&host] == json!({"Handlers":{"/":{"Proxy":owner.target}}})
    {
        Endpoint::Owned
    } else {
        Endpoint::Conflict
    }
}

fn approval_url(text: &str) -> Option<String> {
    text.split_ascii_whitespace().find_map(|word| {
        let text = word.trim_matches(['\'', '"', '<', '>', '(', ')']);
        let url = Url::parse(text).ok()?;
        (url.scheme() == "https"
            && url.host_str() == Some("login.tailscale.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none())
        .then(|| url.to_string())
    })
}

async fn settings(storage: &Storage, url: Option<String>) -> bool {
    matches!(save_settings(storage, url, None).await, Ok(true))
}

async fn save_settings(
    storage: &Storage,
    url: Option<String>,
    expected_url: Option<String>,
) -> Result<bool, String> {
    storage.run(true, move |db| {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw: Option<String> = tx.query_row("SELECT data FROM settings WHERE id=1", [], |row| row.get(0)).optional()?;
        let mut configuration = raw.and_then(|value| serde_json::from_str::<Value>(&value).ok())
            .filter(Value::is_object).unwrap_or_else(|| json!({}));
        if expected_url.is_some_and(|expected| configuration["tailscaleEnabled"] == true && configuration["tailscaleUrl"] != expected) {
            return Ok(false);
        }
        configuration["tailscaleEnabled"] = json!(url.is_some());
        configuration["tailscaleUrl"] = json!(url.unwrap_or_default());
        tx.execute("INSERT INTO settings(id,data) VALUES (1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
            [configuration.to_string()])?;
        tx.commit()?;
        Ok(true)
    }).await
}

async fn current_config(bin: &FilePath, route: &DaemonRoute) -> Option<Value> {
    json_command(bin, route.socket(), &["serve", "status", "--json"]).await
}

async fn status(bin: &FilePath, socket: Option<&FilePath>) -> Option<Value> {
    json_command(bin, socket, &["status", "--json"])
        .await
        .filter(Value::is_object)
}

async fn system_status(bin: &FilePath) -> Option<Value> {
    status(bin, system_socket()).await
}

async fn set_funnel(
    bin: &FilePath,
    route: &DaemonRoute,
    owner: &Owner,
    disable: bool,
) -> Option<Output> {
    let mut arguments = vec!["funnel", "--bg", HTTPS_FLAG, ROOT_FLAG, &owner.target];
    if disable {
        arguments.push("off");
    }
    invoke(bin, route.socket(), &arguments, CHANGE).await
}

impl Tailscale {
    async fn userspace(&self, bin: &FilePath) -> Option<(DaemonRoute, Value)> {
        #[cfg(target_os = "linux")]
        {
            let (socket, status) = self.daemon.ensure(&self.storage, bin).await?;
            Some((DaemonRoute::Userspace(socket), status))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = bin;
            None
        }
    }

    async fn typed_route(&self, bin: &FilePath, backend: Backend) -> Option<(DaemonRoute, Value)> {
        match backend {
            Backend::System => system_status(bin)
                .await
                .map(|node| (DaemonRoute::System, node)),
            Backend::Userspace => self.userspace(bin).await,
        }
    }
}

fn login_needed(url: String) -> Response {
    reply(
        StatusCode::OK,
        json!({"success":false,"needsLogin":true,"authUrl":url}),
        false,
    )
}

async fn userspace_login(storage: &Storage, bin: &FilePath, route: &DaemonRoute) -> Option<String> {
    let shared = crate::cloudflare_tunnel::shared_state(storage);
    let hostname = format!(
        "--hostname={}",
        crate::cloudflare_tunnel::short_id(&shared)?
    );
    let output = invoke(
        bin,
        route.socket(),
        &["up", "--accept-routes", "--timeout=6s", &hostname],
        Duration::from_secs(8),
    )
    .await?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    approval_url(&text)
}

async fn restore_owned(state: &Tailscale) {
    let Ok(_operation) = state.operation.try_lock() else {
        return;
    };
    #[cfg(target_os = "linux")]
    let Ok(_lease) = claim_operation(&state.storage) else {
        return;
    };
    if !state
        .storage
        .read_settings()
        .await
        .is_some_and(|config| config["tailscaleEnabled"] == true)
    {
        return;
    }
    let Ok(Some(owner)) = read_owner(&state.storage) else {
        return;
    };
    if local_target(state.listener).as_deref() != Some(owner.target.as_str()) {
        return;
    }
    let Some(bin) = binary(&state.storage) else {
        return;
    };
    let Some((route, node)) = state.typed_route(&bin, owner.backend).await else {
        return;
    };
    if !online(&node) || !owner.belongs_to(&node) {
        return;
    }
    let Some(config) = current_config(&bin, &route).await else {
        return;
    };
    if configured_endpoint(&config, &owner) == Endpoint::Empty {
        let _ = set_funnel(&bin, &route, &owner, false).await;
    }
}

async fn enable(State(state): State<Tailscale>) -> Response {
    let _operation = state.operation.lock().await;
    let Some(target) = local_target(state.listener) else {
        return error(
            StatusCode::BAD_REQUEST,
            "The gateway must listen on a loopback address for Tailscale Funnel",
        );
    };
    #[cfg(target_os = "linux")]
    let _lease = match claim_operation(&state.storage) {
        Ok(lease) => lease,
        Err(error) => return error.response(),
    };
    let initial = match read_owner(&state.storage) {
        Ok(owner) => owner,
        Err(_) => return conflict(),
    };
    if initial.as_ref().is_some_and(|owner| owner.target != target) {
        return conflict();
    }
    let Some(bin) = binary(&state.storage) else {
        return unavailable();
    };
    if initial.is_none() && workspace_status(&state, &bin).await.is_some() {
        return conflict();
    }
    let selected = if let Some(owner) = &initial {
        state.typed_route(&bin, owner.backend).await
    } else if state.daemon.userspace_route().await.is_some() {
        state.userspace(&bin).await
    } else if let Some(node) = system_status(&bin).await {
        Some((DaemonRoute::System, node))
    } else {
        state.userspace(&bin).await
    };
    let Some((route, mut node)) = selected else {
        return if cfg!(target_os = "linux")
            && initial
                .as_ref()
                .is_none_or(|owner| owner.backend == Backend::Userspace)
        {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not start the workspace Tailscale daemon",
            )
        } else {
            unavailable()
        };
    };
    if !online(&node) {
        if let Some(url) = node["AuthURL"].as_str().and_then(approval_url) {
            return login_needed(url);
        }
        if route.backend() != Backend::Userspace {
            return error(
                StatusCode::CONFLICT,
                "Log in to the system Tailscale daemon before enabling Funnel",
            );
        }
        let url = userspace_login(&state.storage, &bin, &route).await;
        match status(&bin, route.socket()).await {
            Some(fresh) if online(&fresh) => node = fresh,
            fresh => {
                if let Some(url) = url.or_else(|| {
                    fresh.and_then(|node| node["AuthURL"].as_str().and_then(approval_url))
                }) {
                    return login_needed(url);
                }
                return error(
                    StatusCode::BAD_GATEWAY,
                    "Could not start workspace Tailscale login",
                );
            }
        }
    }
    let Some((node_id, name)) = Owner::current_node(&node) else {
        return unavailable();
    };
    let owner = Owner {
        node: node_id.into(),
        hostname: name,
        target,
        backend: route.backend(),
    };
    if initial.as_ref().is_some_and(|initial| initial != &owner) {
        return conflict();
    }
    let Some(config) = current_config(&bin, &route).await else {
        return unavailable();
    };
    match configured_endpoint(&config, &owner) {
        Endpoint::Conflict => return conflict(),
        Endpoint::Owned if initial.is_none() => return conflict(),
        Endpoint::Owned => {
            let url = owner.url();
            return if settings(&state.storage, Some(url.clone())).await {
                joined(url)
            } else {
                persistence_error()
            };
        }
        Endpoint::Empty => {}
    }
    if record(&state.storage, &owner).is_err() {
        return persistence_error();
    }
    let output = set_funnel(&bin, &route, &owner, false).await;
    let Some((node, config)) = verify(&bin, &route, &owner).await else {
        return unavailable();
    };
    match configured_endpoint(&config, &owner) {
        Endpoint::Owned if online(&node) => {
            let url = owner.url();
            if settings(&state.storage, Some(url.clone())).await {
                joined(url)
            } else {
                if verified_remove(&bin, &route, &owner).await == Some(Endpoint::Empty) {
                    let _ = clear(&state.storage);
                }
                persistence_error()
            }
        }
        Endpoint::Empty => {
            let _ = clear(&state.storage);
            if let Some(output) = output {
                let text = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                if text.to_ascii_lowercase().contains("funnel is not enabled")
                    && let Some(url) = approval_url(&text)
                {
                    return reply(
                        StatusCode::OK,
                        json!({
                            "success":false,"funnelNotEnabled":true,"enableUrl":url,
                        }),
                        false,
                    );
                }
            }
            error(
                StatusCode::BAD_GATEWAY,
                "Tailscale did not publish the gateway Funnel",
            )
        }
        _ => conflict(),
    }
}

async fn verify(bin: &FilePath, route: &DaemonRoute, owner: &Owner) -> Option<(Value, Value)> {
    let (node, config) = tokio::join!(status(bin, route.socket()), current_config(bin, route));
    let node = node?;
    owner.belongs_to(&node).then_some((node, config?))
}

async fn verified_remove(bin: &FilePath, route: &DaemonRoute, owner: &Owner) -> Option<Endpoint> {
    let (_, before) = verify(bin, route, owner).await?;
    let before = configured_endpoint(&before, owner);
    if before != Endpoint::Owned {
        return Some(before);
    }
    let _ = set_funnel(bin, route, owner, true).await;
    let (_, after) = verify(bin, route, owner).await?;
    Some(configured_endpoint(&after, owner))
}

fn persistence_error() -> Response {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not save Tailscale Funnel ownership",
    )
}

async fn disable(State(state): State<Tailscale>) -> Response {
    let _operation = state.operation.lock().await;
    #[cfg(target_os = "linux")]
    let _lease = match claim_operation(&state.storage) {
        Ok(lease) => lease,
        Err(error) => return error.response(),
    };
    let owner = match read_owner(&state.storage) {
        Ok(owner) => owner,
        Err(_) => return conflict(),
    };
    let Some(owner) = owner else {
        let Some(settings) = state.storage.read_settings().await else {
            return persistence_error();
        };
        if settings["tailscaleEnabled"] != true {
            return removed();
        }
        return conflict();
    };
    let route = match owner.backend {
        Backend::System => DaemonRoute::System,
        Backend::Userspace => match state.daemon.userspace_route().await {
            Some(route) => route,
            None => return disable_idle_userspace(&state.storage, &owner).await,
        },
    };
    let Some(bin) = binary(&state.storage) else {
        return unavailable();
    };
    match verified_remove(&bin, &route, &owner).await {
        Some(Endpoint::Empty)
            if settings(&state.storage, None).await && clear(&state.storage).is_ok() =>
        {
            removed()
        }
        Some(Endpoint::Empty) => persistence_error(),
        Some(Endpoint::Conflict) => conflict(),
        Some(Endpoint::Owned) => error(
            StatusCode::BAD_GATEWAY,
            "Could not disable the gateway Funnel",
        ),
        None => unavailable(),
    }
}

#[cfg(target_os = "linux")]
async fn disable_idle_userspace(storage: &Storage, owner: &Owner) -> Response {
    let _daemon = match owned_daemon::lock_idle_profile(storage) {
        Ok(lease) => lease,
        Err(cause) if cause.kind() == io::ErrorKind::WouldBlock => {
            return error(
                StatusCode::CONFLICT,
                "Another process owns the workspace Tailscale daemon",
            );
        }
        Err(_) => return unavailable(),
    };
    match save_settings(storage, None, Some(owner.url())).await {
        Ok(true) => removed(),
        Ok(false) => conflict(),
        Err(_) => persistence_error(),
    }
}

#[cfg(not(target_os = "linux"))]
async fn disable_idle_userspace(_: &Storage, _: &Owner) -> Response {
    unavailable()
}

pub(crate) async fn owned_status(storage: &Storage, daemon: &Daemon) -> Option<Value> {
    let owner = read_owner(storage).ok()??;
    let settings = storage.read_settings().await?;
    let enabled = settings["tailscaleEnabled"] == true;
    let bin = binary(storage)?;
    let route = match owner.backend {
        Backend::System => Some(DaemonRoute::System),
        Backend::Userspace => daemon.userspace_route().await,
    };
    let verified = match route {
        Some(route) => verify(&bin, &route, &owner).await,
        None => None,
    };
    let logged_in = verified.as_ref().is_some_and(|(node, _)| online(node));
    let running = logged_in
        && verified
            .as_ref()
            .is_some_and(|(_, config)| configured_endpoint(config, &owner) == Endpoint::Owned);
    Some(json!({
        "enabled":enabled && running,"settingsEnabled":enabled,
        "tunnelUrl":settings["tailscaleUrl"].as_str().unwrap_or(""),
        "running":running,"loggedIn":logged_in,
    }))
}
