use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

const MAX_FRAME: usize = 64 * 1024;
const CAPABILITY_FILE: &str = "mcode-auth-lease-v1.cap";

pub(super) struct Error {
    pub(super) code: &'static str,
    pub(super) message: &'static str,
}

impl Error {
    fn new(code: &str) -> Self {
        let (code, message) = match code {
            "AUTH_REQUIRED" => ("AUTH_REQUIRED", "Desktop authentication is required."),
            "BROKER_UNAVAILABLE" => (
                "BROKER_UNAVAILABLE",
                "Desktop authentication integration is unavailable.",
            ),
            "CAPABILITY_REJECTED" => (
                "CAPABILITY_REJECTED",
                "Desktop authentication capability was rejected.",
            ),
            "PROTOCOL_MISMATCH" => (
                "PROTOCOL_MISMATCH",
                "Desktop authentication protocol is incompatible.",
            ),
            "INVALID_REQUEST" => (
                "INVALID_REQUEST",
                "Desktop authentication request is invalid.",
            ),
            _ => ("INTERNAL_ERROR", "Desktop authentication request failed."),
        };
        Self { code, message }
    }
}

struct Lease {
    token: String,
    expires_at_ms: i64,
}

type CachedLease = Arc<AsyncMutex<Option<Lease>>>;
static LEASES: LazyLock<Mutex<HashMap<PathBuf, CachedLease>>> = LazyLock::new(Mutex::default);

pub(super) fn data_dir(account: &Value) -> Result<PathBuf, Error> {
    let path = account["providerSpecificData"]["mcodeDataDir"]
        .as_str()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            std::env::var("MINIMAX_DESKTOP_DATA_DIR")
                .ok()
                .map(|path| path.trim().to_owned())
                .filter(|path| !path.is_empty())
        })
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".minimax")))
        .ok_or_else(|| Error::new("BROKER_UNAVAILABLE"))?;
    let absolute = std::path::absolute(path).map_err(|_| Error::new("INVALID_REQUEST"))?;
    normalize(&absolute)
}

pub(super) fn normalize(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_absolute() {
        return Err(Error::new("INVALID_REQUEST"));
    }
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => (),
            component => normalized.push(component),
        }
    }
    Ok(normalized)
}

pub(super) async fn status(path: &Path) -> Result<Value, Error> {
    request(path, "status", Duration::from_secs(5)).await
}

pub(super) async fn lease(path: &Path) -> Result<String, Error> {
    take_lease(path).await.map(|lease| lease.token)
}

pub(crate) async fn credentials(
    account: &Value,
    rejected: Option<&str>,
) -> Result<Value, &'static str> {
    let result = async {
        let path = data_dir(account)?;
        let cell = LEASES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(path.clone())
            .or_default()
            .clone();
        let mut cached = cell.lock().await;
        if cached.as_ref().is_none_or(|current| {
            rejected == Some(current.token.as_str())
                || current
                    .expires_at_ms
                    .saturating_sub(Utc::now().timestamp_millis())
                    <= 60_000
        }) {
            *cached = None;
            *cached = Some(take_lease(&path).await?);
        }
        let mut current = account.clone();
        current["apiKey"] = Value::Null;
        current["accessToken"] = json!(cached.as_ref().map(|lease| lease.token.as_str()));
        Ok::<_, Error>(current)
    }
    .await;
    result.map_err(|error| error.message)
}

async fn take_lease(path: &Path) -> Result<Lease, Error> {
    let result = request(path, "lease", Duration::from_secs(10)).await?;
    let token = result["accessToken"]
        .as_str()
        .filter(|token| {
            !token.is_empty()
                && !token
                    .bytes()
                    .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        })
        .ok_or_else(|| Error::new("PROTOCOL_MISMATCH"))?;
    let expires_at_ms = result["expiresAtMs"]
        .as_i64()
        .filter(|expiry| *expiry > Utc::now().timestamp_millis())
        .ok_or_else(|| Error::new("PROTOCOL_MISMATCH"))?;
    Ok(Lease {
        token: token.to_owned(),
        expires_at_ms,
    })
}

async fn request(path: &Path, method: &str, timeout: Duration) -> Result<Value, Error> {
    tokio::time::timeout(timeout, async {
        let capability = tokio::fs::read_to_string(path.join("run").join(CAPABILITY_FILE))
            .await
            .map_err(|_| Error::new("BROKER_UNAVAILABLE"))?;
        let capability = capability.trim();
        if capability.len() != 43
            || !capability
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(Error::new("BROKER_UNAVAILABLE"));
        }
        let mut body = json!({
            "version":1, "requestId":format!("9r-{method}-{}", Uuid::new_v4()),
            "capability":capability, "method":method,
        });
        if method == "lease" {
            body["minValidityMs"] = json!(300_000);
        }
        let frame = body.to_string();
        let mut socket = connect(path).await?;
        socket
            .write_u32(frame.len() as u32)
            .await
            .map_err(|_| Error::new("BROKER_UNAVAILABLE"))?;
        socket
            .write_all(frame.as_bytes())
            .await
            .map_err(|_| Error::new("BROKER_UNAVAILABLE"))?;
        let length = socket
            .read_u32()
            .await
            .map_err(|_| Error::new("BROKER_UNAVAILABLE"))? as usize;
        if length == 0 || length > MAX_FRAME {
            return Err(Error::new("PROTOCOL_MISMATCH"));
        }
        let mut bytes = vec![0; length];
        socket
            .read_exact(&mut bytes)
            .await
            .map_err(|_| Error::new("BROKER_UNAVAILABLE"))?;
        let response: Value =
            serde_json::from_slice(&bytes).map_err(|_| Error::new("PROTOCOL_MISMATCH"))?;
        if response["ok"] == true && response["result"].is_object() {
            Ok(response["result"].clone())
        } else {
            Err(Error::new(
                response["error"]["code"].as_str().unwrap_or_default(),
            ))
        }
    })
    .await
    .map_err(|_| Error::new("BROKER_UNAVAILABLE"))?
}

#[cfg(unix)]
async fn connect(path: &Path) -> Result<impl AsyncRead + AsyncWrite + Unpin, Error> {
    tokio::net::UnixStream::connect(path.join("run").join("mcode-auth-lease-v1.sock"))
        .await
        .map_err(|_| Error::new("BROKER_UNAVAILABLE"))
}

#[cfg(windows)]
async fn connect(path: &Path) -> Result<impl AsyncRead + AsyncWrite + Unpin, Error> {
    use sha2::{Digest, Sha256};
    use tokio::net::windows::named_pipe::ClientOptions;

    let canonical = path
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_lowercase();
    let digest = Sha256::digest(canonical.as_bytes());
    let id = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    ClientOptions::new()
        .open(format!(r"\\.\pipe\mcode-auth-lease-{id}"))
        .map_err(|_| Error::new("BROKER_UNAVAILABLE"))
}
