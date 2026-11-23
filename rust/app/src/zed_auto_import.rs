use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use super::{commandcode::local_authorized, nonempty, reply};
use crate::provider_index::js_trim;
use crate::storage::Storage;

const CREDENTIALS_URL: &str = "https://zed.dev";
const LABEL: &str = "zed-github-account";
const OUTPUT_LIMIT: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(8);

pub(super) async fn read(State(storage): State<Storage>, request: Request) -> Response {
    if !local_authorized(&storage, &request) {
        return reply(
            StatusCode::FORBIDDEN,
            absent("Local only: CLI token required", None),
            true,
        );
    }
    let Some(home) = std::env::home_dir() else {
        return reply(
            StatusCode::OK,
            absent("Unable to locate the home directory", None),
            true,
        );
    };
    let context = tokio::task::spawn_blocking(move || paths(&home)).await;
    let Ok((address, database)) = context else {
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            absent("Failed to read Zed credentials", None),
            true,
        );
    };
    let (user, token) = match search(&address).await {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            return reply(
                StatusCode::OK,
                absent(
                    "Zed IDE session not found in the system keyring. Sign in to Zed, then retry.",
                    Some(&address),
                ),
                true,
            );
        }
        Err(reason) => return reply(StatusCode::OK, absent(reason, Some(&address)), true),
    };
    let system = tokio::task::spawn_blocking(move || system_id(&database))
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    reply(
        StatusCode::OK,
        json!({
            "found":true,"userId":user,"accessToken":token,"systemId":system,"credentialsUrl":address,
        }),
        true,
    )
}

fn paths(home: &Path) -> (String, PathBuf) {
    let xdg = |key: &str, fallback: &str| {
        std::env::var_os(key)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(fallback))
    };
    let settings = xdg("XDG_CONFIG_HOME", ".config").join("zed/settings.json");
    let config = std::fs::metadata(&settings)
        .ok()
        .filter(|meta| meta.is_file() && meta.len() <= OUTPUT_LIMIT)
        .and_then(|_| std::fs::read(settings).ok())
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .unwrap_or_default();
    let base = nonempty(&config["credentials_url"])
        .or_else(|| nonempty(&config["server_url"]))
        .map(|url| url.trim_end_matches('/'))
        .filter(|url| {
            url::Url::parse(url).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.has_host()
                    && url.username().is_empty()
                    && url.password().is_none()
            })
        });
    (
        base.unwrap_or(CREDENTIALS_URL).to_owned(),
        xdg("XDG_DATA_HOME", ".local/share").join("zed/db"),
    )
}

fn system_id(base: &Path) -> Option<String> {
    ["0-global", "0-stable", "0-preview"]
        .iter()
        .find_map(|channel| {
            let db = Connection::open_with_flags(
                base.join(channel).join("db.sqlite"),
                OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .ok()?;
            db.busy_timeout(Duration::from_secs(2)).ok()?;
            let id = db
                .query_row(
                    "SELECT value FROM kv_store WHERE key='system_id' LIMIT 1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()?;
            let id = js_trim(&id);
            (!id.is_empty()).then(|| id.to_owned())
        })
}

async fn search(address: &str) -> Result<Option<(String, String)>, &'static str> {
    let mut child = tokio::process::Command::new("secret-tool")
        .args(["search", "--all", "url", address])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                "secret-tool not found (install libsecret / secret-tools)"
            }
            _ => "Failed to read Linux keyring",
        })?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or("Failed to read Linux keyring")?
        .take(OUTPUT_LIMIT + 1);
    let mut stderr = child
        .stderr
        .take()
        .ok_or("Failed to read Linux keyring")?
        .take(OUTPUT_LIMIT + 1);
    let mut body = Vec::new();
    let mut attrs = Vec::new();
    match tokio::time::timeout(TIMEOUT, async {
        tokio::try_join!(
            stdout.read_to_end(&mut body),
            stderr.read_to_end(&mut attrs)
        )?;
        if body.len() as u64 <= OUTPUT_LIMIT && attrs.len() as u64 <= OUTPUT_LIMIT {
            child.wait().await?;
        }
        Ok::<_, std::io::Error>(())
    })
    .await
    {
        Ok(Ok(())) if body.len() as u64 <= OUTPUT_LIMIT && attrs.len() as u64 <= OUTPUT_LIMIT => (),
        _ => return Err("Failed to read Linux keyring"),
    }
    let stdout = String::from_utf8_lossy(&body);
    let stderr = String::from_utf8_lossy(&attrs);
    Ok(parse_pair(&stdout, &stderr))
}

fn parse_pair(stdout: &str, stderr: &str) -> Option<(String, String)> {
    let fields = |text: &str| {
        text.lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
            .collect::<Vec<_>>()
    };
    let mut fields = fields(stdout);
    let extract = |fields: &[(String, String)], name: &str| {
        let mut values = fields
            .iter()
            .filter(|(field, value)| field == name && !value.is_empty())
            .map(|(_, value)| value);
        let value = values.next()?;
        values.all(|other| value == other).then(|| value.clone())
    };
    fields.extend(
        stderr
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned())),
    );
    if extract(&fields, "label").as_deref() != Some(LABEL) {
        return None;
    }
    extract(&fields, "attribute.username").zip(extract(&fields, "secret"))
}

fn absent(reason: &str, credentials_url: Option<&str>) -> Value {
    json!({"found":false,"error":reason,"credentialsUrl":credentials_url})
}
