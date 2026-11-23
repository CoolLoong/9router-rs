use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use chrono::{SecondsFormat, TimeDelta, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, types::ValueRef};
use serde_json::{Value, json};

use super::{body, commandcode::local_authorized, error, nonempty, reply};
use crate::dashboard_session;
use crate::provider_connections::object;
use crate::provider_index::js_trim;
use crate::provider_oauth::{save_imported, token_claims};
use crate::storage::Storage;

const ASSET: &str = "User/globalStorage/state.vscdb";
const ACCESS: &[&str] = &["cursorAuth/accessToken", "cursorAuth/token"];
const MACHINE: &[&str] = &[
    "storage.serviceMachineId",
    "storage.machineId",
    "telemetry.machineId",
];

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
            absent("Failed to read Cursor credentials"),
            true,
        ),
    }
}

pub(super) async fn import(State(storage): State<Storage>, request: Request) -> Response {
    let input = match body(&storage, request).await {
        Ok(input) => input,
        Err((status, message)) => return error(status, message),
    };
    let Some(token) = nonempty(&input["accessToken"]) else {
        return error(StatusCode::BAD_REQUEST, "Access token is required");
    };
    let Some(machine) = nonempty(&input["machineId"]) else {
        return error(StatusCode::BAD_REQUEST, "Machine ID is required");
    };
    let token = js_trim(token);
    let machine = js_trim(machine);
    let format_error = if token.is_empty() {
        Some("Access token is required")
    } else if machine.is_empty() {
        Some("Machine ID is required")
    } else if token.encode_utf16().count() < 50 {
        Some("Invalid token format. Token appears too short.")
    } else {
        let machine_hex = machine.bytes().filter(|byte| *byte != b'-');
        (machine_hex.clone().count() < 32
            || !machine_hex.into_iter().all(|byte| byte.is_ascii_hexdigit()))
        .then_some("Invalid machine ID format. Expected UUID format.")
    };
    if let Some(message) = format_error {
        return error(StatusCode::INTERNAL_SERVER_ERROR, message);
    }
    let claims = token_claims(token);
    let email = nonempty(&claims["email"]).or_else(|| nonempty(&claims["sub"]));
    let user = nonempty(&claims["sub"]).or_else(|| nonempty(&claims["user_id"]));
    let mut details =
        object(json!({"machineId":machine,"authMethod":"imported","provider":"Imported"}));
    if let Some(user) = user {
        details.insert("userId".into(), json!(user));
    }
    let expires = (Utc::now() + TimeDelta::days(1)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let fields = object(json!({
        "accessToken":token,"refreshToken":null,"expiresAt":expires,
        "email":email,"providerSpecificData":details,
    }));
    match save_imported(&storage, "cursor", fields).await {
        Ok(record) => reply(
            StatusCode::OK,
            json!({
                "success":true,"connection":{"id":record["id"],"provider":"cursor","email":record["email"]},
            }),
            true,
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save the provider connection",
        ),
    }
}

pub(super) async fn instructions(State(storage): State<Storage>, request: Request) -> Response {
    if !dashboard_session::authenticated(&storage, request.headers()).await {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    reply(
        StatusCode::OK,
        json!({
            "provider":"cursor","method":"import_token",
            "instructions":{
                "title":"How to get your Cursor token",
                "steps":[
                    "1. Open Cursor IDE and make sure you're logged in",
                    "2. Find the state.vscdb file:",
                    "   - Linux: ~/.config/Cursor/User/globalStorage/state.vscdb",
                    "   - macOS: /Users/<user>/Library/Application Support/Cursor/User/globalStorage/state.vscdb",
                    r"   - Windows: %APPDATA%\Cursor\User\globalStorage\state.vscdb",
                    "3. Open the database with SQLite browser or CLI:",
                    r#"   sqlite3 state.vscdb "SELECT value FROM itemTable WHERE key='cursorAuth/accessToken'""#,
                    "4. Also get the machine ID:",
                    r#"   sqlite3 state.vscdb "SELECT value FROM itemTable WHERE key='storage.serviceMachineId'""#,
                    "5. Paste both values in the form below",
                ],
                "alternativeMethod":[
                    "Or use this one-liner to get both values:",
                    r#"sqlite3 state.vscdb "SELECT key, value FROM itemTable WHERE key IN ('cursorAuth/accessToken', 'storage.serviceMachineId')""#,
                ],
            },
            "requiredFields":[
                {"name":"accessToken","label":"Access Token","description":"From cursorAuth/accessToken in state.vscdb","type":"textarea"},
                {"name":"machineId","label":"Machine ID","description":"From storage.serviceMachineId in state.vscdb","type":"text"},
            ],
        }),
        true,
    )
}

fn find() -> Value {
    let Some(home) = std::env::home_dir() else {
        return absent("Missing home directory");
    };
    let candidates = paths(&home);
    let selected = candidates.iter().find(|path| {
        fs::metadata(path).is_ok_and(|details| details.is_file()) && fs::File::open(path).is_ok()
    });
    let Some(path) = selected else {
        let checked = candidates
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        return absent(&format!(
            "Cursor database not found. Checked locations:\n{checked}\n\nMake sure Cursor IDE is installed and opened at least once."
        ));
    };
    if cfg!(target_os = "linux")
        && !crate::cli_tools::executable("cursor")
        && !home
            .join(".local/share/applications/cursor.desktop")
            .is_file()
    {
        return absent(
            "Cursor config files found but Cursor IDE does not appear to be installed. Skipping auto-import.",
        );
    }
    match read_tokens(path) {
        Some((token, machine)) => json!({"found":true,"accessToken":token,"machineId":machine}),
        None => json!({"found":false,"windowsManual":true,"dbPath":path}),
    }
}

fn paths(home: &Path) -> Vec<PathBuf> {
    let (primary, fallback) = match std::env::consts::OS {
        "macos" => (home.join("Library/Application Support"), None),
        "windows" => (
            env_path("APPDATA").unwrap_or_else(|| home.join("AppData/Roaming")),
            Some(env_path("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData/Local"))),
        ),
        _ => {
            return ["Cursor", "cursor"]
                .iter()
                .map(|name| home.join(".config").join(name).join(ASSET))
                .collect();
        }
    };
    let mut paths = vec![
        primary.join("Cursor").join(ASSET),
        primary.join("Cursor - Insiders").join(ASSET),
    ];
    if let Some(fallback) = fallback {
        paths.push(fallback.join("Cursor").join(ASSET));
        paths.push(fallback.join("Programs/Cursor").join(ASSET));
    }
    paths
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn read_tokens(path: &Path) -> Option<(String, String)> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    db.busy_timeout(Duration::from_secs(2)).ok()?;
    let mut statement = db
        .prepare("SELECT value FROM itemTable WHERE key=? LIMIT 1")
        .ok()?;
    let mut lookup = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            let raw = statement
                .query_row([key], |row| {
                    let value = match row.get_ref(0)? {
                        ValueRef::Text(bytes) | ValueRef::Blob(bytes)
                            if bytes.len() <= 1024 * 1024 =>
                        {
                            std::str::from_utf8(bytes).ok().map(str::to_owned)
                        }
                        _ => None,
                    };
                    Ok(value)
                })
                .optional()
                .ok()
                .flatten()
                .flatten()?;
            let value = serde_json::from_str::<String>(&raw).unwrap_or(raw);
            (!value.is_empty()).then_some(value)
        })
    };
    Some((lookup(ACCESS)?, lookup(MACHINE)?))
}

fn absent(reason: &str) -> Value {
    json!({"found":false,"error":reason})
}
