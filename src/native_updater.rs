use std::fs::{self, File};
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use reqwest::Client;
use rustix::fs::{FlockOperation, Mode, OFlags, flock};
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use serde_json::{Value, json};
use tempfile::{Builder, TempDir};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};
use url::Url;

use crate::cli_lifecycle::{child_owns_listener, inode};
use crate::dashboard_session::has_session;
use crate::native_update_source::{self as source, SOURCE};
use crate::storage::Storage;

const REPLACEMENT_WAIT: Duration = Duration::from_secs(12);
const CHILD_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
pub(crate) use source::CHILD;

#[derive(Clone)]
pub(crate) struct Updater {
    storage: Storage,
    shutdown: Arc<Notify>,
    staged: Arc<Mutex<Option<Staged>>>,
    persistent: bool,
}

struct Staged {
    release: source::Release,
    install: PathBuf,
    image: [u64; 2],
    directory: TempDir,
    _lock: File,
}

impl Updater {
    pub(crate) fn new(storage: Storage, shutdown: Arc<Notify>, persistent: bool) -> Self {
        Self {
            storage,
            shutdown,
            staged: Arc::default(),
            persistent,
        }
    }

    pub(crate) fn router(&self) -> Router {
        Router::new()
            .route("/api/version/update", post(update))
            .with_state(self.clone())
    }

    pub(crate) async fn apply(&self, address: SocketAddr, arguments: &[String]) -> io::Result<()> {
        let Some(staged) = self.staged.lock().await.take() else {
            return Ok(());
        };
        let Staged {
            release,
            install,
            image,
            directory,
            _lock,
        } = staged;
        let parent = install.parent().ok_or_else(invalid_installation)?;
        let candidate = directory.path().join("candidate");
        let original_version = compiled_version();
        if inode(&install)? != image {
            return Err(invalid_installation());
        }
        if !release.available() {
            self.launch(&install, image, &original_version, address, arguments)
                .await?;
            return Err(io::Error::other(
                "The staged release expired before shutdown completed",
            ));
        }
        let replacement = match inode(&candidate) {
            Ok(image) => image,
            Err(error) => {
                self.launch(&install, image, &original_version, address, arguments)
                    .await?;
                return Err(error);
            }
        };
        let recovery = directory.keep();
        let previous = recovery.join("previous");
        if let Err(error) = fs::rename(&candidate, &install) {
            self.launch(&install, image, &original_version, address, arguments)
                .await?;
            return Err(error);
        }
        let version = release.version.to_string();
        let completed = match sync_directory(parent).and_then(|_| sync_directory(&recovery)) {
            Ok(()) => {
                self.launch(&install, replacement, &version, address, arguments)
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = completed {
            if inode(&install).ok() != Some(replacement) {
                return Err(io::Error::other(format!(
                    "Native installation changed. Recovery files: {}",
                    recovery.display()
                )));
            }
            if let Err(rollback) = fs::rename(&previous, &install)
                .and_then(|_| sync_directory(parent))
                .and_then(|_| sync_directory(&recovery))
            {
                return Err(io::Error::other(format!(
                    "Native update rollback failed: {rollback}. Recovery files: {}",
                    recovery.display()
                )));
            }
            self.launch(&install, image, &original_version, address, arguments)
                .await?;
            let _ = fs::remove_dir(&recovery);
            return Err(io::Error::other(format!(
                "Native update failed. The previous executable was restored. {error}"
            )));
        }
        let _ = fs::remove_dir_all(&recovery);
        Ok(())
    }

    async fn launch(
        &self,
        install: &Path,
        expected: [u64; 2],
        version: &str,
        address: SocketAddr,
        arguments: &[String],
    ) -> io::Result<()> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(1))
            .build()
            .map_err(io::Error::other)?;
        let probe = if address.ip().is_unspecified() {
            let ip = if address.is_ipv4() {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            } else {
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            };
            SocketAddr::new(ip, address.port())
        } else {
            address
        };
        let health = format!("http://{probe}/api/health");
        let executable = File::from(rustix::fs::open(
            install,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?);
        let meta = executable.metadata()?;
        if [meta.dev(), meta.ino()] != expected {
            return Err(invalid_installation());
        }
        let mut child = Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()))
            .args(arguments)
            .arg("--no-browser")
            .env("DATA_DIR", self.storage.root())
            .env(CHILD, "1")
            .spawn()?;
        let pid = child.id().ok_or_else(invalid_installation)?;
        let elapsed = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(io::Error::other(format!(
                    "Replacement process exited: {status}"
                )));
            }
            if child_owns_listener(self.storage.root(), pid, install, address, version)
                && let Ok(response) = client.get(&health).send().await
                && response.status().is_success()
                && response
                    .json::<Value>()
                    .await
                    .is_ok_and(|value| value["ok"] == true)
                && child_owns_listener(self.storage.root(), pid, install, address, version)
            {
                return Ok(());
            }
            if elapsed.elapsed() >= REPLACEMENT_WAIT {
                stop_child(&mut child, pid).await?;
                return Err(io::Error::other(
                    "Replacement process did not become healthy",
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

async fn stop_child(child: &mut Child, pid: u32) -> io::Result<()> {
    let owned = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .and_then(|pid| pidfd_open(pid, PidfdFlags::empty()).ok());
    if let Some(owned) = owned
        && pidfd_send_signal(&owned, Signal::TERM).is_ok()
        && let Ok(result) = tokio::time::timeout(CHILD_SHUTDOWN_WAIT, child.wait()).await
    {
        return result.map(drop);
    }
    child.kill().await
}

pub(crate) fn compiled_version() -> String {
    source::current_version().to_string()
}

async fn update(State(updater): State<Updater>, request: Request) -> Response {
    if !has_session(&updater.storage, request.headers()) {
        return reply(StatusCode::UNAUTHORIZED, false, "Unauthorized");
    }
    if !same_origin(&request) {
        return reply(
            StatusCode::FORBIDDEN,
            false,
            "The native update requires a same-origin request",
        );
    }
    if !updater.persistent {
        return reply(
            StatusCode::CONFLICT,
            false,
            "Native update requires a persistent installation",
        );
    }
    let source = match &*SOURCE {
        Ok(Some(source)) => source,
        Ok(None) => {
            return reply(
                StatusCode::SERVICE_UNAVAILABLE,
                false,
                "A signed native release source is not configured",
            );
        }
        Err(error) => return reply(StatusCode::SERVICE_UNAVAILABLE, false, error.message()),
    };
    let Ok(mut slot) = updater.staged.try_lock() else {
        return reply(
            StatusCode::CONFLICT,
            false,
            "A native update is already in progress",
        );
    };
    if slot.is_some() {
        return reply(
            StatusCode::CONFLICT,
            false,
            "A native update is already ready to install",
        );
    }
    let release = match source.latest().await {
        Ok(release) if release.available() => release,
        Ok(_) => {
            return reply(
                StatusCode::CONFLICT,
                false,
                "No newer native release is available",
            );
        }
        Err(error) => return reply(StatusCode::BAD_GATEWAY, false, error.message()),
    };
    let staged = match stage(release) {
        Ok(staged) => staged,
        Err(_) => {
            return reply(
                StatusCode::CONFLICT,
                false,
                "The running executable is not in a writable native installation",
            );
        }
    };
    let candidate = staged.directory.path().join("candidate");
    if let Err(error) = source.download(&staged.release, &candidate).await {
        return reply(StatusCode::BAD_GATEWAY, false, error.message());
    }
    if let Err(error) = prepare_executable(&candidate) {
        return reply(StatusCode::BAD_GATEWAY, false, error);
    }
    if preserve_installed(&staged).is_err() {
        return reply(
            StatusCode::CONFLICT,
            false,
            "Could not preserve the running executable for recovery",
        );
    }
    *slot = Some(staged);
    let shutdown = updater.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        shutdown.notify_one();
    });
    reply(
        StatusCode::OK,
        true,
        "Native update verified and staged. This app will restart shortly.",
    )
}

fn preserve_installed(staged: &Staged) -> io::Result<()> {
    let parent = staged.install.parent().ok_or_else(invalid_installation)?;
    if inode(&staged.install)? != staged.image {
        return Err(invalid_installation());
    }
    fs::hard_link(&staged.install, staged.directory.path().join("previous"))?;
    sync_directory(staged.directory.path())?;
    sync_directory(parent)
}

fn stage(release: source::Release) -> io::Result<Staged> {
    let install = std::env::current_exe()?;
    if install.file_name().and_then(|name| name.to_str()) != Some("9router") {
        return Err(invalid_installation());
    }
    let parent = install.parent().ok_or_else(invalid_installation)?;
    let meta = fs::symlink_metadata(&install)?;
    let directory_meta = fs::metadata(parent)?;
    let uid = rustix::process::getuid().as_raw();
    let id = [meta.dev(), meta.ino()];
    if !meta.is_file()
        || meta.uid() != uid
        || meta.mode() & 0o7022 != 0
        || directory_meta.uid() != uid
        || directory_meta.mode() & 0o022 != 0
        || id != inode(Path::new("/proc/self/exe"))?
    {
        return Err(invalid_installation());
    }
    let flags =
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let lock = File::from(rustix::fs::open(
        parent.join(".9router-update.lock"),
        flags,
        Mode::RUSR | Mode::WUSR,
    )?);
    if !lock.metadata()?.is_file() {
        return Err(invalid_installation());
    }
    flock(&lock, FlockOperation::NonBlockingLockExclusive)?;
    let directory = Builder::new()
        .prefix(".9router-update-")
        .tempdir_in(parent)?;
    Ok(Staged {
        release,
        install,
        image: id,
        directory,
        _lock: lock,
    })
}

fn prepare_executable(path: &Path) -> Result<(), &'static str> {
    let failure = "The signed update artifact is not a usable Linux executable";
    let file = File::open(path).map_err(|_| failure)?;
    let mut magic = [0; 4];
    (&file).read_exact(&mut magic).map_err(|_| failure)?;
    if magic != *b"\x7fELF" {
        return Err(failure);
    }
    file.set_permissions(fs::Permissions::from_mode(0o755))
        .map_err(|_| failure)?;
    file.sync_all().map_err(|_| failure)
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

fn invalid_installation() -> io::Error {
    io::Error::other("Native installation or current launcher identity changed")
}

fn same_origin(request: &Request) -> bool {
    let mut origins = request.headers().get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    let Some(origin) = origin.to_str().ok().and_then(|raw| Url::parse(raw).ok()) else {
        return false;
    };
    let Some(host) = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    matches!(origin.scheme(), "https" | "http")
        && origin.username().is_empty()
        && origin.password().is_none()
        && origin.path() == "/"
        && origin.query().is_none()
        && origin.fragment().is_none()
        && Url::parse(&format!("{}://{host}", origin.scheme()))
            .is_ok_and(|expected| expected.origin() == origin.origin())
}

fn reply(status: StatusCode, success: bool, message: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"success":success,"message":message})),
    )
        .into_response()
}
