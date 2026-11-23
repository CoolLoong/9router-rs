use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Extension, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};
use serde_json::{Map, Value, json};
use serde_yaml::{Mapping as YamlMap, Value as Yaml};

use crate::cli::Tool;
use crate::cli_connect::write_private;
use crate::observed_response::into_http;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::storage::{Storage, is_active};
use crate::{catalog_guard, dashboard_session};

#[path = "cli_tool_json.rs"]
mod json_tools;

#[path = "copilot_settings.rs"]
mod copilot_settings;
#[path = "cowork_discovery.rs"]
mod cowork_discovery;
#[path = "cowork_settings.rs"]
mod cowork_settings;
#[path = "deepseek_tui_settings.rs"]
mod deepseek_tui_settings;
#[path = "devin_settings.rs"]
mod devin_settings;
pub(crate) use devin_settings::runtime_binary as devin_binary;
#[path = "cli_tool_editors.rs"]
mod editors;
#[path = "grok_build_settings.rs"]
mod grok_build_settings;
#[path = "hermes_settings.rs"]
mod hermes_settings;
#[path = "jcode_settings.rs"]
mod jcode_settings;
#[path = "kilo_settings.rs"]
mod kilo_settings;
#[path = "cli_tools_mitm.rs"]
pub(crate) mod mitm;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostTool {
    Forge,
    Crush,
    Smelt,
    CodeWhale,
    Omp,
}

impl HostTool {
    const ALL: [Self; 5] = [
        Self::Forge,
        Self::Crush,
        Self::Smelt,
        Self::CodeWhale,
        Self::Omp,
    ];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Forge => "forge",
            Self::Crush => "crush",
            Self::Smelt => "smelt",
            Self::CodeWhale => "codewhale",
            Self::Omp => "omp",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Forge => "ForgeCode",
            Self::Crush => "Crush",
            Self::Smelt => "Smelt",
            Self::CodeWhale => "CodeWhale",
            Self::Omp => "Oh My Pi",
        }
    }

    fn file(self, home: &Path, xdg: Option<&Path>) -> PathBuf {
        match self {
            Self::Forge => home.join(".forge/config.toml"),
            Self::CodeWhale => home.join(".codewhale/config.toml"),
            Self::Smelt => home.join(".smelt/config.json"),
            Self::Crush => Tool::Crush.files(home, xdg).remove(0),
            Self::Omp => Tool::Omp.files(home, xdg).remove(0),
        }
    }

    fn applied(self) -> String {
        if self == Self::Omp {
            "Oh My Pi settings applied! Run 'omp' and all 9Router models appear under 9router in /model.".into()
        } else {
            format!("{} settings applied successfully!", self.name())
        }
    }

    fn removed(self) -> String {
        if self == Self::Smelt {
            "Smelt 9Router settings removed".into()
        } else {
            format!("9Router removed from {}", self.name())
        }
    }

    fn configured(self, raw: &str, config: &Value) -> bool {
        match self {
            Self::Forge | Self::CodeWhale => {
                raw.contains("managed by 9Router") || raw.contains("localhost:20128")
            }
            Self::Omp => raw.contains("9router:") || raw.contains("localhost:20128"),
            Self::Crush => {
                truthy(&config["providers"]["9router"]["base_url"])
                    || config["providers"].as_object().is_some_and(|providers| {
                        providers.values().any(|provider| {
                            provider["base_url"]
                                .as_str()
                                .is_some_and(|base| base.contains("20128"))
                        })
                    })
            }
            Self::Smelt => {
                config["_managedBy"] == "9router"
                    || config["baseUrl"]
                        .as_str()
                        .is_some_and(|base| base.contains("20128"))
            }
        }
    }

    fn status(self) -> Result<Value, String> {
        let path = config_path(self)?;
        let raw = fs::read_to_string(&path).unwrap_or_default();
        let installed = executable(self.id())
            || fs::metadata(&path).is_ok()
            || self == Self::Omp && fs::metadata(path.with_file_name("agent.db")).is_ok();
        if !installed {
            let name = if self == Self::Omp {
                self.name().to_owned()
            } else {
                format!("{} CLI", self.name())
            };
            return Ok(
                json!({"installed":false,"config":null,"message":format!("{name} is not installed")}),
            );
        }
        let config = match self {
            Self::Forge | Self::CodeWhale => toml::from_str::<toml::Table>(&raw)
                .ok()
                .filter(|_| !raw.is_empty())
                .and_then(|table| serde_json::to_value(table).ok())
                .unwrap_or_default(),
            Self::Crush | Self::Smelt => serde_json::from_str(&raw).unwrap_or_default(),
            Self::Omp => Value::Null,
        };
        let configured = self.configured(&raw, &config);
        Ok(if self == Self::Omp {
            let base = serde_yaml::from_str::<Yaml>(&raw).ok().and_then(|config| {
                config["providers"]["9router"]["baseUrl"]
                    .as_str()
                    .map(str::to_owned)
            });
            json!({"installed":true,"has9Router":configured,"configPath":path,"routerConfig":{"baseUrl":base}})
        } else {
            json!({"installed":true,"config":config,"has9Router":configured,"configPath":path})
        })
    }
}

#[derive(Clone)]
struct CliTools {
    storage: Storage,
    editing: Arc<Mutex<()>>,
    mitm: Arc<tokio::sync::Mutex<mitm::Runtime>>,
    bound: std::net::SocketAddr,
}

const STATUS_TOOLS: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "droid",
    "openclaw",
    "hermes",
    "cowork",
    "cline",
    "kilo",
    "deepseek-tui",
    "jcode",
    "grok-build",
    "devin",
    "pi",
    "omp",
    "crush",
    "forge",
    "smelt",
    "codewhale",
];

pub fn router(storage: Storage, bound: std::net::SocketAddr) -> (Router, mitm::Shutdown) {
    let state = CliTools {
        storage,
        editing: Arc::new(Mutex::new(())),
        mitm: Arc::default(),
        bound,
    };
    let shutdown = mitm::Shutdown {
        storage: state.storage.clone(),
        runtime: state.mitm.clone(),
    };
    let mut router = Router::new().route("/api/cli-tools/all-statuses", get(all_statuses));
    for tool in HostTool::ALL {
        router = router.route(
            &format!("/api/cli-tools/{}-settings", tool.id()),
            get(status)
                .post(apply)
                .delete(restore)
                .layer(Extension(tool)),
        );
    }
    let router = json_tools::add_routes(router);
    let router = editors::add_routes(router);
    let router = kilo_settings::add_routes(router);
    let router = devin_settings::add_routes(router);
    let router = copilot_settings::add_routes(router);
    let router = cowork_discovery::add_routes(router);
    let router = cowork_settings::add_routes(router);
    let router = deepseek_tui_settings::add_routes(router);
    let router = hermes_settings::add_routes(router);
    let router = jcode_settings::add_routes(router);
    let router = mitm::add_routes(router);
    let router = grok_build_settings::add_routes(router)
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state);
    (router, shutdown)
}

pub(crate) async fn tool_status(id: &str) -> Option<Value> {
    let status = tool_status_details(id).await?;
    Some(json!({
        "installed":status["installed"].as_bool(),
        "has9Router":status["has9Router"].as_bool(),
    }))
}

async fn tool_status_details(id: &str) -> Option<Value> {
    if let Some(tool) = HostTool::ALL.into_iter().find(|tool| tool.id() == id) {
        tokio::task::spawn_blocking(move || tool.status())
            .await
            .ok()?
            .ok()
    } else {
        match id {
            "codex" => crate::codex_settings::host_status().await,
            "droid" => crate::droid_settings::host_status().await,
            "claude" => crate::claude_settings::host_status().await,
            "opencode" | "cline" => editors::tool_status(id).await,
            "kilo" => kilo_settings::host_status().await,
            "devin" => devin_settings::host_status().await,
            "copilot" => copilot_settings::host_status().await,
            "cowork" => cowork_settings::host_status().await,
            "deepseek-tui" => deepseek_tui_settings::host_status().await,
            "grok-build" => grok_build_settings::host_status().await,
            "hermes" => hermes_settings::host_status().await,
            "jcode" => jcode_settings::host_status().await,
            _ => json_tools::tool_status(id).await,
        }
    }
}

async fn all_statuses() -> Response {
    let entries = futures_util::future::join_all(STATUS_TOOLS.iter().map(|&id| async move {
        let mut status = tool_status_details(id).await.unwrap_or_default();
        omit_secrets(&mut status);
        (id.to_owned(), status)
    }))
    .await;
    let mut response = reply(StatusCode::OK, Value::Object(entries.into_iter().collect()));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn omit_secrets(settings: &mut Value) {
    match settings {
        Value::Object(fields) => {
            fields.retain(|key, _| {
                let key = key.to_ascii_lowercase().replace(['_', '-'], "");
                !matches!(
                    key.as_str(),
                    "authorization" | "cookie" | "password" | "secret" | "token"
                ) && !key.ends_with("apikey")
                    && !key.ends_with("authtoken")
                    && !key.ends_with("accesstoken")
                    && !key.ends_with("refreshtoken")
            });
            fields.values_mut().for_each(omit_secrets);
        }
        Value::Array(entries) => entries.iter_mut().for_each(omit_secrets),
        _ => (),
    }
}

async fn authorize(State(state): State<CliTools>, request: Request, next: Next) -> Response {
    let cowork = request.uri().path() == "/api/cli-tools/cowork-settings";
    if cowork || matches!(request.uri().path(), mitm::STATUS_PATH | mitm::ALIAS_PATH) {
        let deny = |status, message| {
            let value = json!({"error":message});
            if cowork {
                crate::settings::reply(status, value, true)
            } else {
                (
                    status,
                    [(header::VARY, "Accept-Encoding")],
                    axum::Json(value),
                )
                    .into_response()
            }
        };
        let headers = request.headers();
        if catalog_guard::valid_cli_token(&state.storage, headers) {
            return next.run(request).await;
        }
        if !catalog_guard::is_direct_loopback(&request)
            || !dashboard_session::authenticated(&state.storage, headers).await
        {
            return deny(StatusCode::FORBIDDEN, "Local only: CLI token required");
        }
        if !dashboard_session::has_session(&state.storage, headers) {
            return deny(StatusCode::UNAUTHORIZED, "Unauthorized");
        }
        return next.run(request).await;
    }
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

fn reply(status: StatusCode, value: Value) -> Response {
    into_http(Ok(json!({
        "status":status.as_u16(),
        "headers":{"content-type":"application/json"},
        "body":value.to_string(),
    })))
}

fn failure(status: StatusCode, message: impl Into<String>) -> Response {
    reply(status, json!({"error":{"message":message.into()}}))
}

fn finish(result: Result<Value, String>) -> Response {
    match result {
        Ok(value) => reply(StatusCode::OK, value),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

async fn in_config<T: Send + 'static>(
    editing: Arc<Mutex<()>>,
    operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(move || {
        let _lock = editing
            .lock()
            .map_err(|_| "CLI settings lock unavailable")?;
        operation()
    })
    .await
    .unwrap_or_else(|_| Err("CLI settings operation failed".into()))
}

async fn status(State(state): State<CliTools>, Extension(tool): Extension<HostTool>) -> Response {
    finish(in_config(state.editing, move || tool.status()).await)
}

async fn apply(
    State(state): State<CliTools>,
    Extension(tool): Extension<HostTool>,
    request: Request,
) -> Response {
    let Ok(bytes) = to_bytes(request.into_body(), 128 * 1024 * 1024).await else {
        return failure(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return failure(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    if !truthy(&body["baseUrl"]) {
        return failure(StatusCode::BAD_REQUEST, "baseUrl is required");
    }
    let Some(base) = body["baseUrl"].as_str() else {
        return failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "baseUrl.endsWith is not a function",
        );
    };
    if truthy(&body["apiKey"]) && !body["apiKey"].is_string() {
        return failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "callerKey.trim is not a function",
        );
    }
    let base = base.to_owned();
    let key = resolve_key(&state.storage, body["apiKey"].as_str()).await;
    let model = body.get("model").filter(|value| truthy(value)).cloned();
    finish(
        in_config(state.editing, move || {
            let path = config_path(tool)?;
            let previous = read(&path)?;
            let normalized = if base.ends_with("/v1") {
                base.clone()
            } else {
                format!("{base}/v1")
            };
            let encoded = match tool {
                HostTool::Forge | HostTool::CodeWhale => {
                    apply_toml(tool, previous.as_deref(), &normalized, &key, model)?
                }
                HostTool::Crush | HostTool::Smelt => {
                    apply_json(tool, previous.as_deref(), &normalized, &key, model)?
                }
                HostTool::Omp => apply_yaml(previous.as_deref(), &normalized, &key)?,
            };
            if tool == HostTool::Omp {
                update_omp(
                    &path,
                    previous.as_deref(),
                    Some(encoded.as_bytes()),
                    Some((&key, &base)),
                )?;
            } else {
                persist(&path, Some(encoded.as_bytes()))?;
            }
            Ok(json!({"success":true,"message":tool.applied(),"configPath":path}))
        })
        .await,
    )
}

async fn restore(State(state): State<CliTools>, Extension(tool): Extension<HostTool>) -> Response {
    finish(
        in_config(state.editing, move || {
            let path = config_path(tool)?;
            let previous = read(&path)?;
            let Some(raw) = previous.as_deref() else {
                if tool == HostTool::Omp {
                    update_omp(&path, None, None, None)?;
                    return Ok(json!({"success":true,"message":tool.removed()}));
                }
                return Ok(json!({"success":true,"message":"No config file to reset"}));
            };

            match tool {
                HostTool::Forge | HostTool::CodeWhale => {
                    let Ok(mut parsed) = parse_toml(Some(raw)) else {
                        return Ok(json!({"success":true,"message":"No config file to reset"}));
                    };
                    parsed.remove("openai");
                    if parsed.is_empty() {
                        persist(&path, None)?;
                    } else {
                        let encoded =
                            toml::to_string(&parsed).map_err(|error| error.to_string())?;
                        persist(&path, Some(encoded.as_bytes()))?;
                    }
                }
                HostTool::Crush | HostTool::Smelt => {
                    let Ok(mut parsed) = parse_json(Some(raw)) else {
                        return Ok(json!({"success":true,"message":"No config file to reset"}));
                    };
                    if tool == HostTool::Crush {
                        if let Some(providers) =
                            parsed.get_mut("providers").and_then(Value::as_object_mut)
                            && providers.get("9router").is_some_and(truthy)
                        {
                            providers.shift_remove("9router");
                            if providers.is_empty() {
                                parsed.shift_remove("providers");
                            }
                            let encoded = serialize_json(parsed)?;
                            persist(&path, Some(encoded.as_bytes()))?;
                        }
                    } else {
                        for name in ["baseUrl", "apiKey", "model", "_managedBy"] {
                            parsed.shift_remove(name);
                        }
                        if parsed.is_empty() {
                            persist(&path, None)?;
                        } else {
                            let encoded = serialize_json(parsed)?;
                            persist(&path, Some(encoded.as_bytes()))?;
                        }
                    }
                }
                HostTool::Omp => {
                    let mut root = parse_yaml(Some(raw))?;
                    if let Some(providers) =
                        root.get_mut("providers").and_then(Yaml::as_mapping_mut)
                    {
                        providers.remove("9router");
                        if providers.is_empty() {
                            root.remove("providers");
                        }
                    }
                    let updated = if root.is_empty() {
                        None
                    } else {
                        Some(serde_yaml::to_string(&root).map_err(|error| error.to_string())?)
                    };
                    update_omp(
                        &path,
                        Some(raw),
                        updated.as_deref().map(str::as_bytes),
                        None,
                    )?;
                }
            }
            Ok(json!({"success":true,"message":tool.removed()}))
        })
        .await,
    )
}

async fn resolve_key(storage: &Storage, configured: Option<&str>) -> String {
    if let Some(key) = configured
        .map(js_trim)
        .filter(|key| !key.is_empty() && *key != "sk_9router")
    {
        return key.to_owned();
    }
    storage
        .run(false, |db| {
            let mut statement =
                db.prepare("SELECT key,isActive FROM apiKeys ORDER BY createdAt ASC")?;
            let mut keys = statement.query([])?;
            while let Some(row) = keys.next()? {
                if is_active(row.get_ref(1)?) {
                    return row.get(0);
                }
            }
            Ok(String::new())
        })
        .await
        .unwrap_or_default()
}

fn config_path(tool: HostTool) -> Result<PathBuf, String> {
    let home = std::env::home_dir().ok_or("Unable to locate the home directory")?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    Ok(tool.file(&home, xdg.as_deref()))
}

pub(super) fn executable(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::metadata(dir.join(name))
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            }
            #[cfg(windows)]
            {
                let extensions =
                    std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
                extensions
                    .split(';')
                    .any(|extension| dir.join(format!("{name}{extension}")).is_file())
            }
            #[cfg(not(any(unix, windows)))]
            dir.join(name).is_file()
        })
    })
}

fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn persist(path: &Path, content: Option<&[u8]>) -> Result<(), String> {
    let result = match content {
        Some(bytes) => write_private(path, bytes, path.exists()),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    };
    result.map_err(|error| error.to_string())
}

struct PendingFile {
    path: PathBuf,
    previous: Option<Vec<u8>>,
    next: Vec<u8>,
}

impl PendingFile {
    fn new(
        path: PathBuf,
        previous: Option<Vec<u8>>,
        config: Map<String, Value>,
    ) -> Result<Self, String> {
        let next = serialize_json(config)?.into_bytes();
        Ok(Self {
            path,
            previous,
            next,
        })
    }
}

fn persist_batch(writes: &[PendingFile]) -> Result<(), String> {
    persist_batch_with_tail(writes, None)
}

fn persist_private_batch(writes: &[PendingFile], private: &PendingFile) -> Result<(), String> {
    persist_batch_with_tail(writes, Some(private))
}

fn persist_batch_with_tail(
    writes: &[PendingFile],
    private: Option<&PendingFile>,
) -> Result<(), String> {
    for (index, update) in writes.iter().chain(private).enumerate() {
        let result = if index == writes.len() {
            write_private(&update.path, &update.next, false).map_err(|error| error.to_string())
        } else {
            persist(&update.path, Some(&update.next))
        };
        if let Err(error) = result {
            for written in writes[..index].iter().rev() {
                let _ = persist(&written.path, written.previous.as_deref());
            }
            return Err(error);
        }
    }
    Ok(())
}

fn parse_toml(raw: Option<&[u8]>) -> Result<toml::Table, String> {
    raw.map_or_else(
        || Ok(toml::Table::new()),
        |raw| {
            let text =
                std::str::from_utf8(raw).map_err(|_| "Existing TOML configuration is not UTF-8")?;
            toml::from_str(text).map_err(|_| "Cannot parse existing TOML configuration".into())
        },
    )
}

fn apply_toml(
    tool: HostTool,
    raw: Option<&[u8]>,
    base: &str,
    key: &str,
    model: Option<Value>,
) -> Result<String, String> {
    let mut root = parse_toml(raw)?;
    let model = toml::Value::try_from(model.unwrap_or_else(|| json!("provider/model-id")))
        .map_err(|error| error.to_string())?;
    let mut provider = toml::Table::new();
    provider.insert("base_url".into(), toml::Value::String(base.into()));
    provider.insert("api_key".into(), toml::Value::String(key.into()));
    provider.insert("model".into(), model);
    root.insert("openai".into(), toml::Value::Table(provider));
    let text = toml::to_string(&root).map_err(|error| error.to_string())?;
    let name = if tool == HostTool::Forge {
        "Forge"
    } else {
        "CodeWhale"
    };
    Ok(format!("# {name} config — managed by 9Router\n\n{text}"))
}

fn parse_json(raw: Option<&[u8]>) -> Result<Map<String, Value>, String> {
    raw.map_or_else(
        || Ok(Map::new()),
        |raw| {
            serde_json::from_slice::<Map<String, Value>>(raw)
                .map_err(|_| "Cannot parse existing JSON configuration".into())
        },
    )
}

fn serialize_json(settings: Map<String, Value>) -> Result<String, String> {
    serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())
}

fn apply_json(
    tool: HostTool,
    raw: Option<&[u8]>,
    base: &str,
    key: &str,
    model: Option<Value>,
) -> Result<String, String> {
    let mut root = parse_json(raw)?;
    if tool == HostTool::Crush {
        let providers = root.entry("providers").or_insert_with(|| json!({}));
        if !truthy(providers) {
            *providers = json!({});
        }
        let providers = providers
            .as_object_mut()
            .ok_or("providers must be a JSON object")?;
        let model = model.unwrap_or_else(|| json!("provider/model-id"));
        providers.insert(
            "9router".into(),
            json!({
                "type":"openai-compat","base_url":base,"api_key":key,
                "models":[{"id":model,"name":model,"context_window":128000}],
            }),
        );
    } else {
        let model = model
            .or_else(|| root.get("model").filter(|value| truthy(value)).cloned())
            .unwrap_or_else(|| json!("provider/model-id"));
        root.insert("baseUrl".into(), json!(base));
        root.insert("apiKey".into(), json!(key));
        root.insert("model".into(), model);
        root.insert("_managedBy".into(), json!("9router"));
    }
    serialize_json(root)
}

fn parse_yaml(raw: Option<&[u8]>) -> Result<YamlMap, String> {
    let Some(raw) = raw else {
        return Ok(YamlMap::new());
    };
    match serde_yaml::from_slice::<Yaml>(raw) {
        Ok(Yaml::Null) => Ok(YamlMap::new()),
        Ok(Yaml::Mapping(root)) => Ok(root),
        _ => Err("Cannot parse existing Oh My Pi YAML configuration".into()),
    }
}

fn apply_yaml(raw: Option<&[u8]>, base: &str, key: &str) -> Result<String, String> {
    let mut root = parse_yaml(raw)?;
    let providers = root
        .entry(Yaml::String("providers".into()))
        .or_insert_with(|| Yaml::Mapping(YamlMap::new()));
    if providers.is_null() || *providers == Yaml::Bool(false) {
        *providers = Yaml::Mapping(YamlMap::new());
    }
    let providers = providers
        .as_mapping_mut()
        .ok_or("providers must be a YAML mapping")?;
    let router = serde_yaml::to_value(json!({
        "baseUrl":base,"apiKey":key,"api":"openai-completions","authHeader":true,
        "disableStrictTools":true,"discovery":{"type":"proxy"},
    }))
    .map_err(|error| error.to_string())?;
    providers.insert("9router".into(), router);
    serde_yaml::to_string(&root).map_err(|error| error.to_string())
}

fn update_omp(
    path: &Path,
    previous: Option<&[u8]>,
    config: Option<&[u8]>,
    credential: Option<(&str, &str)>,
) -> Result<(), String> {
    let mut database = match omp_database(&path.with_file_name("agent.db"))? {
        Some(database) => database,
        None => return persist(path, config),
    };
    let transaction = database
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    save_omp_credential(&transaction, credential).map_err(|error| error.to_string())?;
    persist(path, config)?;
    if let Err(error) = transaction.commit() {
        persist(path, previous)?;
        return Err(error.to_string());
    }
    Ok(())
}

fn omp_database(path: &Path) -> Result<Option<Connection>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let database = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|error| error.to_string())?;
    let table: bool = database.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='auth_credentials' AND type='table')",
        [], |row| row.get(0),
    ).map_err(|error| error.to_string())?;
    Ok(table.then_some(database))
}

fn save_omp_credential(
    db: &Transaction<'_>,
    credential: Option<(&str, &str)>,
) -> rusqlite::Result<()> {
    db.execute(
        "DELETE FROM auth_credentials WHERE provider = '9router'",
        [],
    )?;
    if let Some((key, base)) = credential {
        let now = chrono::Utc::now().timestamp();
        db.execute(
            "INSERT INTO auth_credentials (provider, credential_type, data, disabled_cause, identity_key, created_at, updated_at)
             VALUES ('9router', 'api_key', ?1, NULL, NULL, ?2, ?2)",
            params![json!({"apiKey":key,"baseUrl":base}).to_string(), now],
        )?;
    }
    Ok(())
}
