use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use rustix::fd::OwnedFd;
use rustix::fs::{FileType, FlockOperation, Mode, OFlags, flock, open};
use rustix::process::{
    Pid, PidfdFlags, Signal, geteuid, getpid, getppid, pidfd_open, pidfd_send_signal,
    set_parent_process_death_signal,
};
use serde_json::Value;
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use super::{executable, status};
use crate::storage::Storage;

const DAEMON_ENTRY: &str = "--9router-owned-tailscale-daemon";

pub(crate) fn daemon_entry() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new(DAEMON_ENTRY)) {
        return None;
    }
    let _ = exec_daemon(args);
    Some(1)
}

fn exec_daemon(mut args: impl Iterator<Item = OsString>) -> io::Result<()> {
    let parent = args
        .next()
        .and_then(|pid| pid.to_str()?.parse::<i32>().ok())
        .filter(|pid| *pid > 0)
        .and_then(Pid::from_raw)
        .ok_or(io::ErrorKind::InvalidInput)?;
    set_parent_process_death_signal(Some(Signal::KILL))?;
    if getppid() != Some(parent) {
        return Err(io::Error::from_raw_os_error(
            rustix::io::Errno::SRCH.raw_os_error(),
        ));
    }
    let root = PathBuf::from(args.next().ok_or(io::ErrorKind::InvalidInput)?);
    let runtime = PathBuf::from(args.next().ok_or(io::ErrorKind::InvalidInput)?);
    if !root.is_absolute() || args.next().is_some() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let profile = profile(&root)?;
    if runtime.parent() != Some(profile.as_path())
        || !fs::symlink_metadata(&runtime).is_ok_and(private_directory)
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let stdin = io::stdin();
    let lock = fs::symlink_metadata(profile.join("daemon.lock"))?;
    let fd = rustix::fs::fstat(&stdin)?;
    if !lock.is_file()
        || !owned(&lock)
        || lock.mode() & 0o077 != 0
        || lock.dev() != fd.st_dev
        || lock.ino() != fd.st_ino
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    flock(&stdin, FlockOperation::NonBlockingLockExclusive)?;
    let binary = daemon_binary(&root)?;
    let state = state_file(&profile)?;
    let lease = rustix::io::fcntl_dupfd_cloexec(&stdin, 3)?;
    rustix::io::fcntl_setfd(&lease, rustix::io::FdFlags::empty())?;
    Err(std::process::Command::new(binary)
        .arg(flag("--socket=", &runtime.join("ts.sock")))
        .arg(flag("--statedir=", &profile))
        .arg(flag("--state=", &state))
        .arg("--tun=userspace-networking")
        .current_dir(&profile)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .exec())
}

#[derive(Clone, Default)]
pub(crate) struct Daemon(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    active: Option<Active>,
    closed: bool,
}

struct Active {
    child: Child,
    _lease: OwnedFd,
    _runtime: TempDir,
    socket: PathBuf,
}

impl Daemon {
    pub(super) async fn socket(&self) -> Option<PathBuf> {
        let mut state = self.0.lock().await;
        let active = state.active.as_mut()?;
        if active.child.try_wait().ok()?.is_some() {
            state.active = None;
            return None;
        }
        is_socket(&active.socket).then(|| active.socket.clone())
    }

    pub(super) async fn ensure(&self, storage: &Storage, cli: &Path) -> Option<(PathBuf, Value)> {
        let mut state = self.0.lock().await;
        if state.closed {
            return None;
        }
        if let Some(active) = state.active.as_mut()
            && active.child.try_wait().ok()?.is_some()
        {
            state.active = None;
        }
        let active = match state.active.as_mut() {
            Some(active) => active,
            None => state.active.insert(Active::spawn(storage).ok()?),
        };
        let node = tokio::time::timeout(Duration::from_secs(6), active.ready(cli))
            .await
            .ok()
            .flatten();
        if let Some(node) = node {
            return Some((active.socket.clone(), node));
        }
        if let Some(active) = state.active.take() {
            active.stop().await;
        }
        None
    }

    pub(crate) async fn shutdown(&self) {
        let mut state = self.0.lock().await;
        state.closed = true;
        if let Some(active) = state.active.take() {
            active.stop().await;
        }
    }
}

impl Active {
    fn spawn(storage: &Storage) -> io::Result<Self> {
        let root = fs::canonicalize(storage.root())?;
        daemon_binary(&root)?;
        let profile = profile(&root)?;
        state_file(&profile)?;
        let lease = lock_profile(&profile)?;
        let runtime = tempfile::Builder::new()
            .prefix("s")
            .rand_bytes(6)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(&profile)?;
        let socket = runtime.path().join("ts.sock");
        let child = Command::new("/proc/self/exe")
            .arg(DAEMON_ENTRY)
            .arg(getpid().as_raw_pid().to_string())
            .arg(root)
            .arg(runtime.path())
            .stdin(Stdio::from(lease.try_clone()?))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        Ok(Self {
            child,
            _lease: lease,
            _runtime: runtime,
            socket,
        })
    }

    async fn ready(&mut self, cli: &Path) -> Option<Value> {
        loop {
            if self.child.try_wait().ok()?.is_some() {
                return None;
            }
            if is_socket(&self.socket)
                && let Some(node) = status(cli, Some(&self.socket)).await
                && node["BackendState"]
                    .as_str()
                    .is_some_and(|state| !state.is_empty() && state != "NoState")
                && self.child.try_wait().ok()?.is_none()
            {
                return Some(node);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn stop(mut self) {
        if self.child.try_wait().is_ok_and(|status| status.is_some()) {
            return;
        }
        let fd = self
            .child
            .id()
            .and_then(|pid| Pid::from_raw(pid as i32))
            .and_then(|pid| pidfd_open(pid, PidfdFlags::empty()).ok());
        if let Some(fd) = fd {
            let _ = pidfd_send_signal(&fd, Signal::TERM);
        } else {
            let _ = self.child.start_kill();
        }
        if !matches!(
            tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await,
            Ok(Ok(_))
        ) {
            let _ = self.child.kill().await;
        }
    }
}

fn flag(prefix: &str, path: &Path) -> OsString {
    let mut argument = OsString::from(prefix);
    argument.push(path);
    argument
}

fn is_socket(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| owned(&metadata) && metadata.file_type().is_socket())
}

fn owned(metadata: &fs::Metadata) -> bool {
    metadata.uid() == geteuid().as_raw()
}

fn private_directory(metadata: fs::Metadata) -> bool {
    metadata.is_dir() && owned(&metadata) && metadata.mode() & 0o077 == 0
}

pub(super) fn lock_idle_profile(storage: &Storage) -> io::Result<OwnedFd> {
    lock_profile(&profile(&fs::canonicalize(storage.root())?)?)
}

fn lock_profile(profile: &Path) -> io::Result<OwnedFd> {
    let lease = open(
        profile.join("daemon.lock"),
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let stat = rustix::fs::fstat(&lease)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & 0o077 != 0
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    flock(&lease, FlockOperation::NonBlockingLockExclusive)?;
    Ok(lease)
}

fn profile(root: &Path) -> io::Result<PathBuf> {
    let directory = root.join("tailscale");
    let parent = fs::symlink_metadata(&directory)?;
    if !parent.is_dir() || !owned(&parent) || parent.mode() & 0o022 != 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let profile = directory.join("native");
    match DirBuilder::new().mode(0o700).create(&profile) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if !private_directory(fs::symlink_metadata(&profile)?) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(profile)
}

fn state_file(profile: &Path) -> io::Result<PathBuf> {
    let state = profile.join("tailscaled.state");
    match fs::symlink_metadata(&state) {
        Ok(metadata) if metadata.is_file() && owned(&metadata) && metadata.mode() & 0o077 == 0 => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => return Err(io::ErrorKind::PermissionDenied.into()),
        Err(error) => return Err(error),
    }
    Ok(state)
}

fn daemon_binary(root: &Path) -> io::Result<PathBuf> {
    let directory = root.join("bin");
    let local = directory.join("tailscaled");
    let secure = |metadata: &fs::Metadata| metadata.is_file() && metadata.mode() & 0o6022 == 0;
    if fs::symlink_metadata(&directory)
        .is_ok_and(|metadata| metadata.is_dir() && owned(&metadata) && metadata.mode() & 0o022 == 0)
        && fs::symlink_metadata(&local).is_ok_and(|metadata| owned(&metadata) && secure(&metadata))
        && executable(&local)
    {
        return Ok(local);
    }
    if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
        return Err(io::ErrorKind::NotFound.into());
    }
    [
        "/usr/sbin/tailscaled",
        "/usr/bin/tailscaled",
        "/usr/local/bin/tailscaled",
    ]
    .into_iter()
    .map(Path::new)
    .filter_map(|path| fs::canonicalize(path).ok())
    .find(|path| {
        executable(path)
            && fs::metadata(path).is_ok_and(|metadata| metadata.uid() == 0 && secure(&metadata))
    })
    .ok_or_else(|| io::ErrorKind::NotFound.into())
}
