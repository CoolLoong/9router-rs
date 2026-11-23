use std::fs::{self, DirBuilder, Permissions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value, json};
use tempfile::NamedTempFile;

use crate::chat_output::js_text;
use crate::cli::{Tool, without_trailing_commas};
use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::provider_connections::{object, truthy};
use crate::storage::Storage;

const BASE_URL: &str = "ANTHROPIC_BASE_URL";
const TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";
const AUTO_COMPACT_WINDOW: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
const MODEL_ENV_KEYS: [&str; 4] = [
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
];
const RESET_KEYS: [&str; 4] = [BASE_URL, TOKEN, "API_TIMEOUT_MS", AUTO_COMPACT_WINDOW];
static EDITING: Mutex<()> = Mutex::new(());

#[derive(Clone)]
struct ClaudeState {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    let state = ClaudeState { storage };
    Router::new()
        .route(
            "/api/cli-tools/claude-settings",
            get(get_settings)
                .post(update_settings)
                .delete(remove_settings),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<ClaudeState>, request: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, request.headers()).await {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, "Accept-Encoding")],
        axum::Json(json!({"error":"Unauthorized"})),
    )
        .into_response()
}

async fn get_settings() -> Response {
    match host_status().await {
        Some(value) => reply(StatusCode::OK, value),
        None => failed("Failed to check claude settings"),
    }
}

async fn update_settings(request: Request) -> Response {
    const ERROR: &str = "Failed to update claude settings";
    let Ok(bytes) = to_bytes(request.into_body(), 1024 * 1024).await else {
        return failed(ERROR);
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return failed(ERROR);
    };
    if body.is_null() {
        return failed(ERROR);
    }
    let env = &body["env"];
    if !env.is_object() && !env.is_array() {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"Invalid env object"}),
        );
    }

    match edit_on_host(move |paths| apply(paths, body)).await {
        Some(value) => reply(StatusCode::OK, value),
        None => failed(ERROR),
    }
}

async fn remove_settings() -> Response {
    match edit_on_host(reset).await {
        Some(value) => reply(StatusCode::OK, value),
        None => failed("Failed to reset claude settings"),
    }
}

pub(crate) async fn host_status() -> Option<Value> {
    edit_on_host(|paths| Ok(status(paths))).await
}

async fn edit_on_host(
    operation: impl FnOnce(&Paths) -> io::Result<Value> + Send + 'static,
) -> Option<Value> {
    tokio::task::spawn_blocking(move || {
        let _editing = EDITING.lock().ok()?;
        let home = std::env::home_dir()?;
        let paths = Paths {
            settings: Tool::Claude.files(&home, None).into_iter().next()?,
            profile: home.join(".claude.json"),
        };
        operation(&paths).ok()
    })
    .await
    .ok()?
}

struct Paths {
    settings: PathBuf,
    profile: PathBuf,
}

fn status(paths: &Paths) -> Value {
    if !installed() && !paths.settings.exists() {
        return json!({"installed":false,"settings":null,"message":"Claude CLI is not installed"});
    }
    let mut settings = read_status_file(&paths.settings);
    let profile = read_status_file(&paths.profile);
    let configured = truthy(&settings["env"][BASE_URL]);
    if let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) {
        for name in [TOKEN, "ANTHROPIC_API_KEY", "ANTHROPIC_CUSTOM_HEADERS"] {
            env.shift_remove(name);
        }
    }
    json!({
        "installed":true,
        "settings":settings,
        "has9Router":configured,
        "exaMcpEnabled":truthy(&profile["mcpServers"]["exa"]),
        "settingsPath":paths.settings,
    })
}

fn read_status_file(path: &Path) -> Value {
    ConfigFile::open(path)
        .ok()
        .and_then(|file| file.bytes)
        .and_then(|bytes| parse_jsonc(&bytes).ok())
        .unwrap_or_default()
}

fn installed() -> bool {
    let installed = crate::cli_tools::executable("claude");
    #[cfg(windows)]
    let installed = installed
        || std::env::var_os("APPDATA").is_some_and(|appdata| {
            let path = PathBuf::from(appdata).join("npm");
            let extensions =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
            extensions
                .split(';')
                .any(|extension| path.join(format!("claude{extension}")).is_file())
        });
    installed
}

fn apply(paths: &Paths, mut body: Value) -> io::Result<Value> {
    let mut env = object(body["env"].take());
    if let Some(value) = env.get_mut(BASE_URL).filter(|value| truthy(value)) {
        let base = value.as_str().ok_or(ErrorKind::InvalidData)?;
        if !base.ends_with("/v1") {
            *value = json!(format!("{base}/v1"));
        }
    }

    let settings_file = ConfigFile::open(&paths.settings)?;
    let mut settings = settings_file.settings()?;
    let mut current_env = object(settings.get_mut("env").map(Value::take).unwrap_or_default());
    if current_env.get(TOKEN).is_some_and(truthy) {
        env.shift_remove(TOKEN);
    }
    for name in MODEL_ENV_KEYS {
        if env.get(name).is_some_and(Value::is_null) {
            env.shift_remove(name);
            current_env.shift_remove(name);
        }
    }
    current_env.extend(env);
    if truthy(&body["autoCompactWindow"]) {
        current_env.insert(
            AUTO_COMPACT_WINDOW.into(),
            json!(js_text(&body["autoCompactWindow"])),
        );
    } else {
        current_env.shift_remove(AUTO_COMPACT_WINDOW);
    }
    settings.insert("hasCompletedOnboarding".into(), json!(true));
    settings.insert("env".into(), Value::Object(current_env));

    let profile_file = ConfigFile::open(&paths.profile)?;
    let mut profile = profile_file.settings()?;
    update_exa(&mut profile, truthy(&body["exaMcpEnabled"]));
    persist_pair(settings_file, settings, profile_file, profile)?;
    Ok(success("Settings updated successfully"))
}

fn reset(paths: &Paths) -> io::Result<Value> {
    let settings_file = ConfigFile::open(&paths.settings)?;
    if settings_file.bytes.is_none() {
        return Ok(success("No settings file to reset"));
    }
    let mut settings = settings_file.settings()?;
    let mut clear_env = false;
    if let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) {
        for name in RESET_KEYS.into_iter().chain(MODEL_ENV_KEYS) {
            env.shift_remove(name);
        }
        clear_env = env.is_empty();
    }
    if clear_env {
        settings.shift_remove("env");
    }

    let profile_file = ConfigFile::open(&paths.profile)?;
    let mut profile = profile_file.settings()?;
    update_exa(&mut profile, false);
    persist_pair(settings_file, settings, profile_file, profile)?;
    Ok(success("Settings reset successfully"))
}

fn update_exa(profile: &mut Map<String, Value>, enabled: bool) {
    if enabled {
        let mut servers = object(
            profile
                .get_mut("mcpServers")
                .map(Value::take)
                .unwrap_or_default(),
        );
        servers.insert(
            "exa".into(),
            json!({"type":"http","url":"https://mcp.exa.ai/mcp"}),
        );
        profile.insert("mcpServers".into(), Value::Object(servers));
    } else if let Some(servers) = profile.get_mut("mcpServers").and_then(Value::as_object_mut) {
        servers.shift_remove("exa");
        if servers.is_empty() {
            profile.shift_remove("mcpServers");
        }
    }
}

struct ConfigFile {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    mode: Option<Permissions>,
}

impl ConfigFile {
    fn open(path: &Path) -> io::Result<Self> {
        match fs::symlink_metadata(path) {
            Ok(_) => {
                let path = path.canonicalize()?;
                let metadata = fs::metadata(&path)?;
                if !metadata.is_file() {
                    return Err(ErrorKind::InvalidData.into());
                }
                Ok(Self {
                    bytes: Some(fs::read(&path)?),
                    path,
                    mode: Some(metadata.permissions()),
                })
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self {
                path: path.into(),
                bytes: None,
                mode: None,
            }),
            Err(error) => Err(error),
        }
    }

    fn settings(&self) -> io::Result<Map<String, Value>> {
        self.bytes.as_deref().map_or_else(
            || Ok(Map::new()),
            |bytes| match parse_jsonc(bytes)? {
                Value::Object(fields) => Ok(fields),
                _ => Err(ErrorKind::InvalidData.into()),
            },
        )
    }

    fn stage(&self, bytes: &[u8]) -> io::Result<NamedTempFile> {
        let parent = self.path.parent().ok_or(ErrorKind::InvalidInput)?;
        let mut file = NamedTempFile::new_in(parent)?;
        file.write_all(bytes)?;
        if let Some(mode) = &self.mode {
            file.as_file().set_permissions(mode.clone())?;
        }
        file.as_file().sync_all()?;
        Ok(file)
    }

    fn publish(&self, staged: NamedTempFile) -> io::Result<()> {
        if self.bytes.is_some() {
            staged.persist(&self.path)
        } else {
            staged.persist_noclobber(&self.path)
        }
        .map(drop)
        .map_err(|error| error.error)
    }
}

fn persist_pair(
    settings_file: ConfigFile,
    settings: Map<String, Value>,
    profile_file: ConfigFile,
    profile: Map<String, Value>,
) -> io::Result<()> {
    if settings_file.path == profile_file.path {
        return Err(ErrorKind::InvalidInput.into());
    }
    let mut directory = DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(settings_file.path.parent().ok_or(ErrorKind::InvalidInput)?)?;

    let staged_settings = settings_file.stage(&serde_json::to_vec_pretty(&settings)?)?;
    let staged_profile = profile_file.stage(&serde_json::to_vec_pretty(&profile)?)?;
    let backup = settings_file
        .bytes
        .as_deref()
        .map(|bytes| settings_file.stage(bytes))
        .transpose()?;
    settings_file.publish(staged_settings)?;
    if let Err(error) = profile_file.publish(staged_profile) {
        if let Some(backup) = backup {
            settings_file.publish(backup)?;
        } else {
            fs::remove_file(&settings_file.path)?;
        }
        return Err(error);
    }
    Ok(())
}

fn parse_jsonc(raw: &[u8]) -> io::Result<Value> {
    let mut clean = Vec::with_capacity(raw.len());
    let mut quoted = false;
    let mut input = raw.iter().copied().peekable();
    while let Some(byte) = input.next() {
        match byte {
            b'"' => {
                quoted = !quoted;
                clean.push(byte);
            }
            b'\\' if quoted => {
                clean.push(byte);
                clean.extend(input.next());
            }
            b'/' if !quoted && input.next_if_eq(&b'/').is_some() => {
                clean.push(b' ');
                for next in input.by_ref() {
                    if matches!(next, b'\n' | b'\r') {
                        break;
                    }
                }
            }
            b'/' if !quoted && input.next_if_eq(&b'*').is_some() => {
                clean.push(b' ');
                loop {
                    match input.next() {
                        Some(b'*') if input.next_if_eq(&b'/').is_some() => break,
                        Some(_) => (),
                        None => return Err(ErrorKind::InvalidData.into()),
                    }
                }
            }
            _ => clean.push(byte),
        }
    }
    serde_json::from_slice(&without_trailing_commas(&clean)).map_err(Into::into)
}

fn success(message: &str) -> Value {
    json!({"success":true,"message":message})
}

fn failed(message: &str) -> Response {
    reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error":message}))
}

fn reply(status: StatusCode, value: Value) -> Response {
    into_http(Ok(json!({
        "status":status.as_u16(),
        "headers":{"content-type":"application/json"},
        "body":value.to_string()
    })))
}
