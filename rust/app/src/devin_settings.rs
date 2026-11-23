use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use axum::{Router, http::StatusCode, response::Response, routing::get};
use serde_json::{Value, json};
use tokio::process::Command;

use super::{CliTools, executable, reply};

pub(super) fn add_routes(router: Router<CliTools>) -> Router<CliTools> {
    router.route("/api/cli-tools/devin-settings", get(status))
}

async fn status() -> Response {
    match host_status().await {
        Some(value) => reply(StatusCode::OK, value),
        None => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"Failed to check devin settings"}),
        ),
    }
}

pub(super) async fn host_status() -> Option<Value> {
    let home = std::env::home_dir()?;
    let on_path = executable("devin");
    let source = if on_path {
        Some("path".into())
    } else {
        candidates(&home)
            .into_iter()
            .find(|candidate| candidate.is_file())
            .map(|candidate| candidate.to_string_lossy().into_owned())
    };
    let Some(source) = source else {
        return Some(json!({
            "installed":false,
            "message":"Devin CLI is not installed. Install it from https://cli.devin.ai and run `devin auth login`.",
            "installUrl":"https://cli.devin.ai",
        }));
    };
    let version = if on_path {
        let mut command = Command::new("devin");
        let output = tokio::time::timeout(
            Duration::from_secs(3),
            command
                .arg("--version")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .ok()
        .and_then(Result::ok);
        output
            .filter(|output| output.status.success())
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .lines()
                    .next()
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
            })
    } else {
        None
    };
    Some(json!({
        "installed":true,"source":source,"version":version,
        "message":"Devin CLI detected. Make sure `devin auth login` has been run.",
    }))
}

fn candidates(home: &Path) -> Vec<PathBuf> {
    if cfg!(windows) {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Local"));
        vec![
            local.join("devin/cli/bin/devin.exe"),
            home.join(".local/bin/devin.exe"),
            home.join("scoop/shims/devin.exe"),
            local.join("Programs/devin/devin.exe"),
        ]
    } else {
        vec![
            home.join(".local/share/devin/bin/devin"),
            home.join(".devin/bin/devin"),
            home.join(".local/bin/devin"),
            "/opt/homebrew/bin/devin".into(),
            "/usr/local/bin/devin".into(),
            "/usr/bin/devin".into(),
        ]
    }
}

pub(crate) fn runtime_binary() -> Option<PathBuf> {
    let resolve = |candidate: PathBuf| {
        if !candidate.is_absolute() || !candidate.is_file() {
            return None;
        }
        let path = candidate.canonicalize().ok()?;
        if cfg!(windows)
            && !path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            return None;
        }
        Some(path)
    };
    if let Ok(binary) = std::env::var("CLI_DEVIN_BIN")
        && !binary.trim().is_empty()
    {
        return resolve(PathBuf::from(binary.trim()));
    }
    std::env::home_dir()
        .into_iter()
        .flat_map(|home| candidates(&home))
        .find_map(resolve)
        .or_else(|| {
            let path = std::env::var_os("PATH")?;
            let name = if cfg!(windows) { "devin.exe" } else { "devin" };
            std::env::split_paths(&path).find_map(|directory| resolve(directory.join(name)))
        })
}
