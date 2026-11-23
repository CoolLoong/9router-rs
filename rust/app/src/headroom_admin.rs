use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path as FilePath, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(windows, feature = "native-package")
))]
use axum::routing::post;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use crate::catalog_guard::{is_direct_loopback, valid_cli_token};
use crate::dashboard_session;
use crate::settings::{effective, reply};
use crate::storage::Storage;

#[path = "headroom_extras.rs"]
mod extras;
#[cfg(target_os = "linux")]
#[path = "headroom_process.rs"]
mod process;
#[cfg(all(windows, feature = "native-package"))]
#[path = "headroom_process_windows.rs"]
mod process;
#[cfg(target_os = "macos")]
#[path = "headroom_process_macos.rs"]
mod process;

const DEFAULT_URL: &str = "http://localhost:8787";
const PROXY_PREFIX: &str = "/api/headroom/proxy";
static FETCH_LINK: OnceLock<Regex> = OnceLock::new();

#[derive(Clone)]
struct HeadroomAdmin {
    storage: Storage,
    http: reqwest::Client,
    extras: Arc<tokio::sync::Mutex<()>>,
}

pub fn router(storage: Storage) -> Router {
    let state = HeadroomAdmin {
        storage,
        extras: Arc::new(tokio::sync::Mutex::new(())),
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("Headroom administration HTTP client"),
    };
    let router = Router::new()
        .route("/api/headroom/status", get(status))
        .route(
            "/api/headroom/extras",
            get(extras::status)
                .post(extras::install)
                .delete(extras::uninstall),
        )
        .route(
            "/api/headroom/proxy/{*path}",
            get(proxy)
                .head(proxy)
                .post(proxy)
                .put(proxy)
                .patch(proxy)
                .delete(proxy)
                .options(proxy),
        );
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        all(windows, feature = "native-package")
    ))]
    let router = router
        .route("/api/headroom/start", post(start))
        .route("/api/headroom/stop", post(stop))
        .route("/api/headroom/restart", post(restart));
    router.with_state(state)
}

fn denied(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [(header::VARY, "Accept-Encoding")],
        axum::Json(json!({"error": message})),
    )
        .into_response()
}

fn failed(message: &'static str) -> Response {
    reply(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":message}),
        false,
    )
}

async fn base_url(storage: &Storage) -> Option<String> {
    let settings = effective(storage.read_settings().await?);
    Some(configured_base(&settings))
}

fn configured_base(settings: &Value) -> String {
    settings["headroomUrl"]
        .as_str()
        .filter(|address| !address.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            std::env::var("HEADROOM_URL")
                .ok()
                .filter(|address| !address.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_URL.into())
}

async fn status(State(state): State<HeadroomAdmin>, headers: HeaderMap) -> Response {
    if !dashboard_session::authenticated(&state.storage, &headers).await {
        return denied(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    let Some(base) = base_url(&state.storage).await else {
        return failed("Failed to read Headroom settings");
    };
    let address = base.strip_suffix('/').unwrap_or(&base);
    let metadata = tokio::task::spawn_blocking(detect);
    let running = state
        .http
        .get(format!("{address}/health"))
        .timeout(Duration::from_millis(1500))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success());
    let local = Url::parse(&base).is_ok_and(|url| status_local(&url));
    let details = metadata.await.unwrap_or_default();
    let installed = details.path.is_some();
    let url = Url::parse(&base).map_or_else(
        |_| base.clone(),
        |mut parsed| {
            if parsed.username().is_empty() && parsed.password().is_none() {
                base.clone()
            } else {
                let _ = parsed.set_username("");
                let _ = parsed.set_password(None);
                parsed.to_string()
            }
        },
    );
    let managed_pid = managed_pid(state.storage.root()).await;
    reply(
        StatusCode::OK,
        json!({
            "installed":installed,"path":details.path,"running":running,
            "python":details.python,"localUrl":local,"canStart":installed && local,
            "version":if installed { details.package.version } else { None },
            "extras":{"code":installed && details.package.code,"ml":installed && details.package.ml},
            "url":url,"managedPid":managed_pid,
        }),
        false,
    )
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(windows, feature = "native-package")
))]
async fn start(State(state): State<HeadroomAdmin>, request: Request) -> Response {
    launch(state, request, false).await
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(windows, feature = "native-package")
))]
async fn restart(State(state): State<HeadroomAdmin>, request: Request) -> Response {
    launch(state, request, true).await
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(windows, feature = "native-package")
))]
async fn launch(state: HeadroomAdmin, request: Request, restart: bool) -> Response {
    if !local_authorized(&state.storage, &request).await {
        return denied(StatusCode::FORBIDDEN, "Local only: CLI token required");
    }
    let Some(settings) = state.storage.read_settings().await.map(effective) else {
        return failed("Failed to read Headroom settings");
    };
    let base = configured_base(&settings);
    let Ok(url) = Url::parse(&base) else {
        return external_proxy();
    };
    if !status_local(&url) {
        return external_proxy();
    }
    let port = url.port().filter(|port| *port != 0).unwrap_or(8787);
    let code_aware = settings["headroomCodeAware"] == true;
    let kompress = settings["headroomKompress"] != false;
    let root = state.storage.root().to_owned();
    let result = tokio::task::spawn_blocking(move || {
        let binary = find_command(&extended_path(search_dirs()), "headroom");
        process::start(&root, binary, port, code_aware, kompress, restart)
    })
    .await;
    process_response(result)
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(windows, feature = "native-package")
))]
async fn stop(State(state): State<HeadroomAdmin>, request: Request) -> Response {
    if !local_authorized(&state.storage, &request).await {
        return denied(StatusCode::FORBIDDEN, "Local only: CLI token required");
    }
    let root = state.storage.root().to_owned();
    process_response(tokio::task::spawn_blocking(move || process::stop(&root)).await)
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(windows, feature = "native-package")
))]
fn external_proxy() -> Response {
    reply(
        StatusCode::BAD_REQUEST,
        json!({
            "error":"External Headroom proxies must be started outside 9Router",
            "code":"EXTERNAL_PROXY",
        }),
        false,
    )
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(windows, feature = "native-package")
))]
fn process_response(
    value: Result<Result<Value, process::Failure>, tokio::task::JoinError>,
) -> Response {
    match value {
        Ok(Ok(body)) => {
            let status = if body["stopped"] == false {
                StatusCode::CONFLICT
            } else {
                StatusCode::OK
            };
            reply(status, body, false)
        }
        Ok(Err(failure)) => reply(
            failure.status,
            json!({"error":failure.message,"code":failure.code}),
            false,
        ),
        Err(_) => failed("Headroom operation failed"),
    }
}

fn local_authorized<'a>(
    storage: &'a Storage,
    request: &'a Request,
) -> impl std::future::Future<Output = bool> + 'a {
    let headers = request.headers();
    let direct = is_direct_loopback(request);
    async move {
        valid_cli_token(storage, headers)
            || direct && dashboard_session::authenticated(storage, headers).await
    }
}

fn viewer_local(url: &Url) -> bool {
    matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
}

fn status_local(url: &Url) -> bool {
    viewer_local(url) || url.host_str() == Some("0.0.0.0")
}

fn hop_header(name: &header::HeaderName, headers: &HeaderMap) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    ) || headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case(name.as_str()))
}

async fn proxy(
    State(state): State<HeadroomAdmin>,
    Path(path): Path<String>,
    request: Request,
) -> Response {
    if !local_authorized(&state.storage, &request).await {
        return denied(StatusCode::FORBIDDEN, "Local only: CLI token required");
    }
    let Some(base) = base_url(&state.storage).await else {
        return failed("Failed to read Headroom settings");
    };
    let Ok(mut target) = Url::parse(&base) else {
        return failed("Invalid URL");
    };
    if !matches!(target.scheme(), "http" | "https") {
        return failed("Headroom URL must use http or https");
    }
    target.set_path(&format!("/{path}"));
    target.set_query(request.uri().query());
    target.set_fragment(None);
    let local = viewer_local(&target);
    let dashboard = path == "dashboard";
    let (parts, body) = request.into_parts();
    let forward_body = !matches!(parts.method, Method::GET | Method::HEAD);
    let mut forward = state.http.request(parts.method, target);
    for (name, value) in &parts.headers {
        if name == header::HOST
            || name.as_str().starts_with("x-9r-")
            || hop_header(name, &parts.headers)
            || dashboard && name == header::ACCEPT_ENCODING
            || !local
                && matches!(
                    name.as_str(),
                    "cookie" | "authorization" | "x-api-key" | "x-goog-api-key"
                )
        {
            continue;
        }
        forward = forward.header(name, value);
    }
    if dashboard {
        forward = forward.header(header::ACCEPT_ENCODING, "identity");
    }
    if forward_body {
        forward = forward.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }
    let Ok(response) = forward.send().await else {
        return failed("fetch failed");
    };
    let rewrite = dashboard
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/html"));
    let mut builder = Response::builder().status(response.status());
    for (name, value) in response.headers() {
        if hop_header(name, response.headers())
            || !local && name == header::SET_COOKIE
            || rewrite && matches!(*name, header::CONTENT_LENGTH | header::CONTENT_ENCODING)
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    let body = if rewrite {
        let Ok(text) = response.text().await else {
            return failed("fetch failed");
        };
        let matcher = FETCH_LINK.get_or_init(|| {
            Regex::new(r"fetch\('(/(?:stats-history|transformations/feed|stats|health))")
                .expect("fixed Headroom fetch patterns")
        });
        Body::from(
            matcher
                .replace_all(&text, format!("fetch('{PROXY_PREFIX}$1"))
                .into_owned(),
        )
    } else {
        Body::from_stream(response.bytes_stream())
    };
    builder
        .body(body)
        .unwrap_or_else(|_| failed("fetch failed"))
}

#[derive(Default)]
struct Installed {
    path: Option<PathBuf>,
    python: Option<String>,
    python_bin: Option<PathBuf>,
    package: Package,
}

#[derive(Default)]
struct Package {
    installed: bool,
    version: Option<String>,
    code: bool,
    ml: bool,
}

fn search_dirs() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = if cfg!(windows) {
        ["LOCALAPPDATA", "APPDATA"]
            .into_iter()
            .filter_map(std::env::var_os)
            .flat_map(|home| {
                ["313", "312", "311", "310"].map(|version| {
                    PathBuf::from(&home)
                        .join("Programs")
                        .join("Python")
                        .join(format!("Python{version}"))
                        .join("Scripts")
                })
            })
            .collect()
    } else {
        let mut paths: Vec<_> = [
            "/usr/local/bin",
            "/opt/homebrew/bin",
            "/Library/Frameworks/Python.framework/Versions/3.13/bin",
            "/Library/Frameworks/Python.framework/Versions/3.12/bin",
            "/Library/Frameworks/Python.framework/Versions/3.11/bin",
            "/Library/Frameworks/Python.framework/Versions/3.10/bin",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        paths.extend(std::env::home_dir().map(|home| home.join(".local/bin")));
        paths.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        paths
    };
    paths.dedup();
    paths
}

fn is_executable(path: &FilePath) -> bool {
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

fn find_command(directories: &[PathBuf], name: &str) -> Option<PathBuf> {
    let filename = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    directories
        .iter()
        .map(|dir| dir.join(&filename))
        .find(|path| is_executable(path))
}

fn run_metadata(path: &FilePath, args: &[&str], timeout: Duration) -> Option<String> {
    let output = tempfile::tempfile().ok()?;
    let mut child = Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone().ok()?))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                use std::io::{Seek, SeekFrom};
                let mut output = output;
                output.seek(SeekFrom::Start(0)).ok()?;
                let mut content = String::new();
                output
                    .take(16 * 1024 * 1024)
                    .read_to_string(&mut content)
                    .ok()?;
                return Some(content);
            }
            Ok(Some(_)) => return None,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn python_version(text: &str) -> bool {
    text.split_whitespace().any(|part| {
        let mut parts = part.split('.');
        let Some(major) = parts.next().and_then(|part| part.parse::<u64>().ok()) else {
            return false;
        };
        let Some(minor) = parts.next().and_then(|part| part.parse::<u64>().ok()) else {
            return false;
        };
        major > 3 || major == 3 && minor >= 10
    })
}

fn detect() -> Installed {
    const PYTHONS: &[&str] = &[
        "python3.13",
        "python3.12",
        "python3.11",
        "python3.10",
        "python3",
        "python",
    ];
    let dirs = search_dirs();
    let search = extended_path(dirs.clone());
    let path = find_command(&search, "headroom");
    let mut candidates = Vec::new();
    if let Some(parent) = path.as_deref().and_then(FilePath::parent) {
        let names: &[&str] = if cfg!(windows) {
            &["python", "python3"]
        } else {
            &["python3", "python3.13", "python"]
        };
        for name in names {
            if let Some(candidate) = find_command(&[parent.to_path_buf()], name) {
                candidates.push((candidate.to_string_lossy().into_owned(), candidate));
            }
        }
    }
    for dir in &dirs {
        for name in PYTHONS {
            if let Some(candidate) = find_command(std::slice::from_ref(dir), name) {
                candidates.push((candidate.to_string_lossy().into_owned(), candidate));
            }
        }
    }
    for name in PYTHONS {
        if let Some(candidate) = find_command(&search, name) {
            candidates.push((name.to_string(), candidate));
        }
    }
    let mut seen = HashSet::new();
    let mut result = Installed {
        path,
        ..Installed::default()
    };
    for (name, executable) in candidates {
        if !seen.insert(executable.clone())
            || !run_metadata(&executable, &["--version"], Duration::from_secs(2))
                .as_deref()
                .is_some_and(python_version)
        {
            continue;
        }
        result.python.get_or_insert_with(|| name.clone());
        result.python_bin.get_or_insert_with(|| executable.clone());
        let package = inspect_packages(&executable);
        if !package.installed {
            continue;
        }
        result.python = Some(name);
        result.python_bin = Some(executable);
        result.package = package;
        break;
    }
    result
}

fn inspect_packages(executable: &FilePath) -> Package {
    let Some(packages) = run_metadata(
        executable,
        &[
            "-m",
            "pip",
            "list",
            "--format=json",
            "--disable-pip-version-check",
        ],
        Duration::from_secs(8),
    )
    .and_then(|text| serde_json::from_str::<Vec<Value>>(&text).ok()) else {
        return Package::default();
    };
    let Some(package) = packages.iter().find(|entry| {
        entry["name"]
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case("headroom-ai"))
    }) else {
        return Package::default();
    };
    let has = |markers: &[&str]| {
        packages.iter().any(|value| {
            value["name"].as_str().is_some_and(|name| {
                markers
                    .iter()
                    .any(|marker| name.eq_ignore_ascii_case(marker))
            })
        })
    };
    Package {
        installed: true,
        version: package["version"]
            .as_str()
            .filter(|version| !version.is_empty())
            .map(ToOwned::to_owned),
        code: has(extras::CODE),
        ml: has(extras::ML),
    }
}

fn extended_path(mut directories: Vec<PathBuf>) -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path));
    }
    directories
}

#[cfg(any(target_os = "linux", all(windows, feature = "native-package")))]
async fn managed_pid(root: &FilePath) -> Option<u32> {
    process::managed_pid(root)
}

#[cfg(target_os = "macos")]
async fn managed_pid(root: &FilePath) -> Option<u32> {
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || process::managed_pid(&root))
        .await
        .ok()
        .flatten()
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    all(windows, feature = "native-package")
)))]
async fn managed_pid(_: &FilePath) -> Option<u32> {
    None
}
