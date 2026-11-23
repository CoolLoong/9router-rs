use std::collections::VecDeque;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use super::{MAX_PROFILES, read_bounded};

const RETRY_DELAY: Duration = Duration::from_secs(30);
static RECENT_RENEWALS: Mutex<VecDeque<([u8; 32], Instant)>> = Mutex::new(VecDeque::new());

pub(super) async fn read(
    file: &Path,
    error: &'static str,
) -> Result<(String, Value), &'static str> {
    let file = file.to_owned();
    let snapshot = tokio::task::spawn_blocking(move || read_bounded(&file))
        .await
        .map_err(|_| error)?
        .ok_or(error)?;
    let token = serde_json::from_str(&snapshot).map_err(|_| error)?;
    Ok((snapshot, token))
}

pub(super) async fn stage(file: &Path, error: &'static str) -> Result<NamedTempFile, &'static str> {
    let directory = file.parent().ok_or(error)?.to_owned();
    tokio::task::spawn_blocking(move || NamedTempFile::new_in(directory))
        .await
        .map_err(|_| error)?
        .map_err(|_| error)
}

pub(super) async fn lock(file: &Path, error: &'static str) -> Result<File, &'static str> {
    let target = file.to_owned();
    let sidecar = tokio::task::spawn_blocking(move || {
        let sidecar = target.with_extension("9router-lock");
        if !regular_file(&target) || sidecar.symlink_metadata().is_ok_and(|info| !info.is_file()) {
            return Err(error);
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(sidecar).map_err(|_| error)
    })
    .await
    .map_err(|_| error)??;
    loop {
        match sidecar.try_lock() {
            Ok(()) => return Ok(sidecar),
            Err(TryLockError::WouldBlock) => tokio::time::sleep(Duration::from_millis(25)).await,
            Err(TryLockError::Error(_)) => return Err(error),
        }
    }
}

pub(super) fn persist(
    mut staged: NamedTempFile,
    file: &Path,
    before: &str,
    token: Value,
    error: &'static str,
) -> Result<Value, &'static str> {
    serde_json::to_writer(staged.as_file_mut(), &token).map_err(|_| error)?;
    staged.as_file().sync_all().map_err(|_| error)?;
    if !regular_file(file) {
        return Err(error);
    }
    let current = read_bounded(file).ok_or(error)?;
    if current != before {
        return serde_json::from_str(&current).map_err(|_| error);
    }
    staged.persist(file).map_err(|_| error)?;
    Ok(token)
}

fn regular_file(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|info| info.is_file())
}

pub(super) fn mark_attempt(
    path: &Path,
    refresh: &str,
    error: &'static str,
) -> Result<(), &'static str> {
    let mut hash = Sha256::new();
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update([0]);
    hash.update(refresh);
    let key = hash.finalize().into();
    let now = Instant::now();
    let mut recent = RECENT_RENEWALS.lock().map_err(|_| error)?;
    recent.retain(|(_, until)| *until > now);
    if recent.iter().any(|(candidate, _)| *candidate == key) {
        return Err(error);
    }
    recent.truncate(MAX_PROFILES - 1);
    recent.push_front((key, now + RETRY_DELAY));
    Ok(())
}
