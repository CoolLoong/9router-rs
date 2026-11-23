use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};

use super::{commandcode::local_authorized, nonempty, reply};
use crate::provider_connections::truthy;
use crate::storage::Storage;

const TOKEN_FILE: &str = "kiro-auth-token.json";
const ARN_PREFIX: &str = "arn:aws:codewhisperer:";
const CACHE_MISSING: &str = "AWS SSO cache not found. Please login to Kiro IDE first.";
const TOKEN_MISSING: &str =
    "Kiro token not found in AWS SSO cache. Please login to Kiro IDE first.";

pub(super) async fn read(State(storage): State<Storage>, request: Request) -> Response {
    if !local_authorized(&storage, &request) {
        return reply(
            StatusCode::FORBIDDEN,
            absent("Local only: CLI token required"),
            true,
        );
    }
    match tokio::task::spawn_blocking(find).await {
        Ok(found) => reply(StatusCode::OK, found, true),
        Err(_) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            absent("Unable to read Kiro credentials"),
            true,
        ),
    }
}

fn find() -> Value {
    let Some(home) = std::env::home_dir() else {
        return absent(CACHE_MISSING);
    };
    let cache = home.join(".aws/sso/cache");
    let Ok(files) = fs::read_dir(&cache) else {
        return absent(CACHE_MISSING);
    };
    let mut files = files
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|file| file.ends_with(".json"))
        .collect::<Vec<_>>();
    files.sort();
    let found = std::iter::once(TOKEN_FILE)
        .chain(
            files
                .iter()
                .map(String::as_str)
                .filter(|name| *name != TOKEN_FILE),
        )
        .find_map(|name| {
            let tokens = document(&cache.join(name))?;
            nonempty(&tokens["refreshToken"]).filter(|token| token.starts_with("aorAAAAAG"))?;
            Some((name, tokens))
        });
    let Some((source, tokens)) = found else {
        return absent(TOKEN_MISSING);
    };
    let client_record = nonempty(&tokens["clientIdHash"])
        .filter(|hash| {
            hash.len() <= 128
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .and_then(|hash| document(&cache.join(format!("{hash}.json"))))
        .filter(|data| {
            nonempty(&data["clientId"]).is_some() && nonempty(&data["clientSecret"]).is_some()
        })
        .unwrap_or_default();
    let optional = |name: &str| tokens.get(name).filter(|value| truthy(value));
    let profile = profile_arn(&home);
    json!({
        "found":true,"refreshToken":tokens["refreshToken"],"source":source,
        "clientId":client_record["clientId"],"clientSecret":client_record["clientSecret"],
        "region":optional("region"),"authMethod":optional("authMethod"),"profileArn":profile,
    })
}

fn profile_arn(home: &Path) -> Option<String> {
    let appdata = std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .map_or_else(|| home.join("AppData/Roaming"), PathBuf::from);
    [appdata, home.join(".config")].iter().find_map(|path| {
        let profile = document(&path.join("Kiro/User/globalStorage/kiro.kiroagent/profile.json"))?;
        let arn = nonempty(&profile["arn"])?;
        Some(
            arn.strip_prefix(ARN_PREFIX)
                .and_then(|rest| rest.split_once(':'))
                .map_or_else(
                    || arn.to_owned(),
                    |(_, rest)| format!("{ARN_PREFIX}us-east-1:{rest}"),
                ),
        )
    })
}

fn document(path: &Path) -> Option<Value> {
    const LIMIT: u64 = 1024 * 1024;
    if !fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > LIMIT {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn absent(reason: &str) -> Value {
    json!({"found":false,"error":reason})
}
