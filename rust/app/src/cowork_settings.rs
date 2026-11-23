use std::fs;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde_json::{Map, Value, json};
use url::Url;
use uuid::Uuid;

use super::{CliTools, PendingFile, in_config, persist_batch, persist_private_batch, read};
use crate::cli::without_trailing_commas;
use crate::provider_index::js_trim;
use crate::settings::reply;
use crate::{catalog_guard, key_identity, mcp_bridge};

const INFERENCE_FIELDS: [&str; 4] = [
    "inferenceProvider",
    "inferenceGatewayBaseUrl",
    "inferenceGatewayApiKey",
    "inferenceModels",
];
const LOCAL_ONLY: &str = "Cowork local connectors require a direct loopback connection";
const LOCAL_CONNECTORS: &str = "localConnectorNames";

fn loopback_address(bound: SocketAddr) -> Option<SocketAddr> {
    if bound.ip().to_canonical().is_loopback() {
        return Some(bound);
    }
    bound.ip().is_unspecified().then(|| {
        let address = if bound.is_ipv4() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        };
        SocketAddr::new(address, bound.port())
    })
}

fn local_preset(endpoint: &str, name: &str) -> bool {
    name == mcp_bridge::PLUGIN
        && Url::parse(endpoint).is_ok_and(|url| {
            url.scheme() == "http"
                && url.path() == format!("/api/mcp/{name}/sse")
                && url.host().is_some_and(|host| match host {
                    url::Host::Ipv4(ip) => ip.is_loopback(),
                    url::Host::Ipv6(ip) => ip.to_canonical().is_loopback(),
                    url::Host::Domain(name) => name == "localhost",
                })
        })
}

pub(super) fn add_routes(router: Router<CliTools>) -> Router<CliTools> {
    router.route(
        "/api/cli-tools/cowork-settings",
        get(status).post(apply).delete(reset),
    )
}

pub(super) async fn host_status() -> Option<Value> {
    tokio::task::spawn_blocking(check).await.ok()?.ok()
}

struct Paths {
    third_party: Vec<PathBuf>,
    first_party: Vec<PathBuf>,
    installs: Vec<PathBuf>,
    desktop: PathBuf,
}

impl Paths {
    fn home() -> Result<Self, String> {
        let home = std::env::home_dir().ok_or("Unable to locate the home directory")?;
        let (local, roaming, installs) = match std::env::consts::OS {
            "macos" => {
                let base = home.join("Library/Application Support");
                let installs = vec![
                    PathBuf::from("/Applications/Claude.app"),
                    home.join("Applications/Claude.app"),
                ];
                (base.clone(), base, installs)
            }
            "windows" => {
                let local = env_path("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData/Local"));
                let roaming = env_path("APPDATA").unwrap_or_else(|| home.join("AppData/Roaming"));
                let programs =
                    env_path("ProgramFiles").unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
                let installs = vec![
                    local.join("AnthropicClaude"),
                    programs.join("Claude"),
                    programs.join("AnthropicClaude"),
                ];
                (local, roaming, installs)
            }
            _ => {
                let base = home.join(".config");
                (base.clone(), base, Vec::new())
            }
        };
        let roots = |name| {
            let mut roots = vec![local.join(name)];
            if local != roaming {
                roots.push(roaming.join(name));
            }
            roots
        };
        Ok(Self {
            third_party: roots("Claude-3p"),
            first_party: roots("Claude"),
            installs,
            desktop: roaming.join("Claude/claude_desktop_config.json"),
        })
    }

    fn library(&self, third_party_only: bool) -> PathBuf {
        self.third_party
            .iter()
            .chain(self.first_party.iter().filter(|_| !third_party_only))
            .map(|root| root.join("configLibrary"))
            .find(|library| library.is_dir())
            .unwrap_or_else(|| self.third_party[0].join("configLibrary"))
    }

    fn installed(&self) -> bool {
        self.third_party
            .iter()
            .chain(&self.first_party)
            .chain(&self.installs)
            .any(|path| path.exists())
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn id(value: &Value) -> Option<&str> {
    value.as_str().filter(|id| {
        !id.is_empty()
            && id.len() <= 100
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    })
}

type Document = (Option<Vec<u8>>, Map<String, Value>);

fn document(path: &Path) -> Result<Document, String> {
    match fs::symlink_metadata(path) {
        Ok(info) if info.is_file() => (),
        Ok(_) => return Err("Cowork configuration must be a regular file".into()),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok((None, Map::new())),
        Err(error) => return Err(error.to_string()),
    }
    let raw = read(path)?;
    let object = match &raw {
        Some(bytes) => serde_json::from_slice(&without_trailing_commas(bytes))
            .map_err(|_| "Cannot parse existing Cowork configuration")?,
        None => Map::new(),
    };
    Ok((raw, object))
}

fn open_readonly(path: &Path) -> Option<Map<String, Value>> {
    let (raw, fields) = document(path).ok()?;
    raw.map(|_| fields)
}

fn safe_url(raw: &Value) -> Option<String> {
    let mut url = Url::parse(raw.as_str()?).ok()?;
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
        return None;
    }
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

fn check() -> Result<Value, String> {
    let paths = Paths::home()?;
    if !paths.installed() {
        return Ok(json!({
            "installed": false,
            "config": null,
            "message": "Claude Desktop (Cowork mode) not detected",
        }));
    }
    let library = paths.library(false);
    let meta = open_readonly(&library.join("_meta.json")).unwrap_or_default();
    let applied = meta.get("appliedId").and_then(id);
    let path = applied.map(|applied| library.join(format!("{applied}.json")));
    let config = path
        .as_deref()
        .and_then(open_readonly)
        .map(Value::Object)
        .unwrap_or_default();
    let provider = config["inferenceProvider"].as_str();
    let base = safe_url(&config["inferenceGatewayBaseUrl"]);
    let models: Vec<_> = config["inferenceModels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().or_else(|| item["name"].as_str()))
        .filter(|name| !name.is_empty())
        .collect();
    let mut plugins = Vec::new();
    let mut local = Vec::new();
    let mut custom = Vec::new();
    for plugin in config["managedMcpServers"].as_array().into_iter().flatten() {
        let (Some(name), Some(endpoint)) = (plugin["name"].as_str(), safe_url(&plugin["url"]))
        else {
            continue;
        };
        let transport = plugin["transport"].as_str();
        let bridge = endpoint.contains("/api/mcp/");
        if local_preset(&endpoint, name)
            && transport == Some("sse")
            && plugin["headers"]["x-9r-cli-token"]
                .as_str()
                .is_some_and(|token| !token.is_empty())
        {
            local.push(name);
        } else if bridge || plugin["custom"] == true {
            custom.push(json!({"name":name,"url":endpoint,"transport":transport,"custom":true}));
        } else {
            let mut tool_names = Vec::new();
            let prefix = format!("{name}-");
            for tool in plugin["toolPolicy"]
                .as_object()
                .into_iter()
                .flat_map(|policy| policy.keys())
            {
                let tool = tool.trim_start_matches(&prefix);
                if !tool.is_empty() && !tool_names.contains(&tool) {
                    tool_names.push(tool);
                }
            }
            plugins.push(json!({
                "name":name,"url":endpoint,"transport":transport,"oauth":plugin["oauth"] == true,
                "toolNames":tool_names,
            }));
        }
    }
    let safe_config = (!config.is_null()).then(|| {
        json!({
            "inferenceProvider":provider,"inferenceGatewayBaseUrl":base,"inferenceModels":models,
        })
    });
    Ok(json!({
        "installed":true,"config":safe_config,"has9Router":provider == Some("gateway") && base.is_some(),
        "configPath":path,
        "cowork":{
            "appliedId":applied,"baseUrl":base,"models":models,"provider":provider,"plugins":plugins,
            "localPlugins":local,"customPlugins":custom,
        },
        "defaultPlugins":[],"localStdioPlugins":[],
    }))
}

async fn status(State(state): State<CliTools>, request: Request) -> Response {
    let local = catalog_guard::is_direct_loopback(&request);
    let eligible = local
        && loopback_address(state.bound).is_some()
        && key_identity::cli_token(state.storage.root()).is_some();
    match in_config(state.editing, check).await {
        Ok(mut status) => {
            if status["installed"] == true {
                if eligible {
                    status["localStdioPlugins"] =
                        json!([{"name":mcp_bridge::PLUGIN,"title":"Browser MCP"}]);
                }
                if !local {
                    status["cowork"]["localPlugins"] = json!([]);
                }
            }
            reply(StatusCode::OK, status, true)
        }
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read cowork settings",
        ),
    }
}

async fn apply(State(state): State<CliTools>, request: Request) -> Response {
    let failure = "Failed to apply cowork settings";
    let directly_local = catalog_guard::is_direct_loopback(&request);
    let Some(body) = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Map<String, Value>>(&bytes).ok())
    else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, failure);
    };
    let get_string = |field| {
        body.get(field)
            .and_then(Value::as_str)
            .filter(|str| !js_trim(str).is_empty())
    };
    let (Some(base), Some(key)) = (get_string("baseUrl"), get_string("apiKey")) else {
        return error(StatusCode::BAD_REQUEST, "baseUrl and apiKey are required");
    };
    let Ok(url) = Url::parse(base) else {
        return error(StatusCode::BAD_REQUEST, "Invalid Cowork gateway URL");
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return error(StatusCode::BAD_REQUEST, "Invalid Cowork gateway URL");
    }
    let models: Vec<_> = body
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|model| !js_trim(model).is_empty())
        .map(|name| json!({"name":name}))
        .collect();
    if models.is_empty() {
        return error(StatusCode::BAD_REQUEST, "At least one model is required");
    }
    let local = match body.get("localPlugins").filter(|value| !value.is_null()) {
        Some(_) if !directly_local => return error(StatusCode::FORBIDDEN, LOCAL_ONLY),
        Some(value) => match value.as_array().map(Vec::as_slice) {
            Some([]) => Some(Vec::new()),
            Some([name]) if name == mcp_bridge::PLUGIN => {
                let (Some(address), Some(token)) = (
                    loopback_address(state.bound),
                    key_identity::cli_token(state.storage.root()),
                ) else {
                    return error(
                        StatusCode::CONFLICT,
                        "The local Cowork bridge is unavailable",
                    );
                };
                Some(vec![json!({
                    "name":mcp_bridge::PLUGIN,
                    "url":format!("http://{address}/api/mcp/{}/sse", mcp_bridge::PLUGIN),
                    "transport":"sse",
                    "headers":{"x-9r-cli-token":token},
                })])
            }
            _ => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "Invalid Cowork local connector preset",
                );
            }
        },
        None => None,
    };
    let connectors = match connector_selection(&body, state.bound.port()) {
        Ok(connectors) => connectors,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let base = base.to_owned();
    let key = key.to_owned();
    match in_config(state.editing, move || {
        configure(base, key, models, connectors, local)
    })
    .await
    {
        Ok(Some(value)) => reply(StatusCode::OK, value, true),
        Ok(None) => error(
            StatusCode::CONFLICT,
            "Claude Desktop is configured for another deployment mode",
        ),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, failure),
    }
}

fn configure(
    base: String,
    key: String,
    models: Vec<Value>,
    connectors: Option<Vec<Value>>,
    local: Option<Vec<Value>>,
) -> Result<Option<Value>, String> {
    let paths = Paths::home()?;
    let (desktop_raw, mut desktop) = document(&paths.desktop)?;
    let bootstrapped = match desktop.get("deploymentMode") {
        None | Some(Value::Null) => true,
        Some(mode) if mode == "3p" => false,
        Some(_) => return Ok(None),
    };
    let library = paths.library(true);
    let meta_path = library.join("_meta.json");
    let state_path = library.join(".9router-inference");
    let (meta_raw, mut meta) = document(&meta_path)?;
    let (state_raw, mut owned) = document(&state_path)?;
    let profile_id = if state_raw.is_some() {
        owned
            .get("profileId")
            .and_then(id)
            .ok_or("Malformed Cowork profile state")?
            .to_owned()
    } else {
        Uuid::new_v4().to_string()
    };
    let profile_path = library.join(format!("{profile_id}.json"));
    let (profile_raw, mut profile) = document(&profile_path)?;
    let applied = meta.get("appliedId").and_then(id);
    if applied != Some(profile_id.as_str()) {
        owned.insert("previousAppliedId".into(), json!(applied));
    }
    owned.insert("profileId".into(), json!(profile_id));
    let entries = meta.entry("entries").or_insert_with(|| json!([]));
    let entries = entries
        .as_array_mut()
        .ok_or("Malformed Cowork profile entries")?;
    if !entries.iter().any(|entry| entry["id"] == profile_id) {
        entries.push(json!({"id":profile_id,"name":"9Router"}));
    }
    meta.insert("appliedId".into(), json!(profile_id));
    for (field, contents) in INFERENCE_FIELDS.into_iter().zip([
        json!("gateway"),
        json!(base),
        json!(key),
        Value::Array(models),
    ]) {
        profile.insert(field.into(), contents);
    }
    configure_connectors(&mut profile, &mut owned, connectors, local)?;
    let local_names = owned
        .get(LOCAL_CONNECTORS)
        .cloned()
        .unwrap_or_else(|| json!([]));
    let profile = PendingFile::new(profile_path.clone(), profile_raw, profile)?;
    let state_file = PendingFile::new(state_path, state_raw, owned)?;
    let meta_file = PendingFile::new(meta_path, meta_raw, meta)?;
    let mut writes = vec![state_file];
    if bootstrapped {
        desktop.insert("deploymentMode".into(), json!("3p"));
        writes.push(PendingFile::new(paths.desktop, desktop_raw, desktop)?);
    }
    if profile.previous.is_none() {
        writes.push(profile);
        writes.push(meta_file);
        persist_batch(&writes)?;
    } else {
        writes.push(meta_file);
        persist_private_batch(&writes, &profile)?;
    }
    Ok(Some(json!({
        "success":true,"bootstrapped":bootstrapped,
        "message":if bootstrapped {
            "Cowork enabled (3p mode set). Quit & reopen Claude Desktop."
        } else {
            "Cowork settings applied. Quit & reopen Claude Desktop."
        },
        "configPath":profile_path,"skipApprovals":null,"localMcp":{"applied":local_names,"via":"3p-sse-bridge"},
    })))
}

fn connector_selection(
    body: &Map<String, Value>,
    listener_port: u16,
) -> Result<Option<Vec<Value>>, &'static str> {
    let mut selection = None;
    for (field, custom) in [("plugins", false), ("customPlugins", true)] {
        let Some(selected) = body.get(field).filter(|value| !value.is_null()) else {
            continue;
        };
        let entries = selected.as_array().ok_or("Connectors must be an array")?;
        let selection = selection.get_or_insert_with(Vec::new);
        if selection.len() + entries.len() > 100 {
            return Err("Too many Cowork connectors");
        }
        for entry in entries {
            if ["command", "args", "env", "cwd"]
                .iter()
                .any(|field| entry.get(field).is_some())
            {
                return Err("Cowork connectors do not accept commands");
            }
            let name = entry["name"]
                .as_str()
                .map(js_trim)
                .filter(|name| {
                    !name.is_empty()
                        && name.len() <= 128
                        && !name.chars().any(char::is_control)
                        && !matches!(*name, "__proto__" | "constructor")
                })
                .ok_or("Cowork connector name is required")?;
            let address = entry["url"]
                .as_str()
                .map(js_trim)
                .ok_or("Cowork connector URL is required")?;
            let url = Url::parse(address)
                .ok()
                .filter(|url| {
                    (url.scheme() == "https"
                        || url.scheme() == "http"
                            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
                        && url.has_host()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none()
                })
                .ok_or("Cowork connector URL must use HTTPS or local HTTP")?;
            if url.port_or_known_default() == Some(listener_port)
                && local_preset(url.as_str(), mcp_bridge::PLUGIN)
            {
                return Err("Select the Cowork local connector by its preset name");
            }
            if selection.iter().any(|other: &Value| other["name"] == name) {
                return Err("Cowork connector names must be unique");
            }
            let transport =
                entry["transport"]
                    .as_str()
                    .unwrap_or(if custom { "sse" } else { "http" });
            if !matches!(transport, "http" | "sse") {
                return Err("Cowork connector transport must be http or sse");
            }
            let mut connector = json!({"name":name,"url":url.as_str(),"transport":transport});
            if custom {
                connector["custom"] = json!(true);
            }
            if entry["oauth"] == true {
                connector["oauth"] = json!(true);
            }
            selection.push(connector);
        }
    }
    Ok(selection)
}

fn configure_connectors(
    profile: &mut Map<String, Value>,
    owned: &mut Map<String, Value>,
    remote: Option<Vec<Value>>,
    local: Option<Vec<Value>>,
) -> Result<(), String> {
    let mut previous = Vec::new();
    let mut incoming = Vec::new();
    for (field, configured) in [("connectorNames", remote), (LOCAL_CONNECTORS, local)] {
        let Some(configured) = configured else {
            continue;
        };
        if let Some(old) = owned
            .remove(field)
            .and_then(|entry| entry.as_array().cloned())
        {
            previous.extend(old);
        }
        if !configured.is_empty() {
            owned.insert(
                field.into(),
                json!(
                    configured
                        .iter()
                        .map(|entry| &entry["name"])
                        .collect::<Vec<_>>()
                ),
            );
        }
        incoming.extend(configured);
    }
    if incoming.is_empty() && previous.is_empty() {
        return Ok(());
    }
    let existing = match profile.get("managedMcpServers") {
        Some(current) => current
            .as_array()
            .ok_or("Invalid Cowork connector list")?
            .as_slice(),
        None => &[],
    };
    let mut retained = existing
        .iter()
        .filter(|entry| !previous.contains(&entry["name"]))
        .cloned()
        .collect::<Vec<_>>();
    for mut change in incoming {
        if retained.iter().any(|entry| entry["name"] == change["name"]) {
            return Err("A separately configured Cowork connector uses this name".into());
        }
        if let Some(prior) = existing
            .iter()
            .find(|entry| entry["name"] == change["name"] && entry["url"] == change["url"])
            && let Some(approval) = prior.get("toolPolicy")
        {
            change["toolPolicy"] = approval.clone();
        }
        retained.push(change);
    }
    if retained.is_empty() {
        profile.remove("managedMcpServers");
    } else {
        profile.insert("managedMcpServers".into(), Value::Array(retained));
    }
    Ok(())
}

async fn reset(State(state): State<CliTools>, request: Request) -> Response {
    let directly_local = catalog_guard::is_direct_loopback(&request);
    match in_config(state.editing, move || reset_profile(directly_local)).await {
        Ok(Some(value)) => reply(StatusCode::OK, value, true),
        Ok(None) => error(StatusCode::FORBIDDEN, LOCAL_ONLY),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to reset cowork settings",
        ),
    }
}

fn reset_profile(directly_local: bool) -> Result<Option<Value>, String> {
    let library = Paths::home()?.library(true);
    let state_path = library.join(".9router-inference");
    let (state_raw, mut owned) = document(&state_path)?;
    let Some(state_raw) = state_raw else {
        return Ok(Some(
            json!({"success":true,"message":"No active config to reset"}),
        ));
    };
    let had_local = owned.contains_key(LOCAL_CONNECTORS);
    if had_local && !directly_local {
        return Ok(None);
    }
    let profile_id = owned
        .get("profileId")
        .and_then(id)
        .ok_or("Malformed Cowork profile state")?;
    let profile_path = library.join(format!("{profile_id}.json"));
    let meta_path = library.join("_meta.json");
    let (profile_raw, mut profile) = document(&profile_path)?;
    let (meta_raw, mut meta) = document(&meta_path)?;
    let mut writes = Vec::new();
    if meta.get("appliedId").and_then(id) == Some(profile_id)
        && let Some(previous) = owned.get("previousAppliedId").and_then(id)
        && meta
            .get("entries")
            .and_then(Value::as_array)
            .is_some_and(|entries| entries.iter().any(|entry| entry["id"] == previous))
        && library.join(format!("{previous}.json")).is_file()
    {
        meta.insert("appliedId".into(), json!(previous));
        writes.push(PendingFile::new(meta_path, meta_raw, meta)?);
    }
    let removed_prior = owned.remove("previousAppliedId").is_some();
    let had_remote = owned.contains_key("connectorNames");
    let removed_connectors = had_remote || had_local;
    if removed_connectors {
        configure_connectors(
            &mut profile,
            &mut owned,
            had_remote.then(Vec::new),
            had_local.then(Vec::new),
        )?;
    }
    if removed_prior || removed_connectors {
        writes.push(PendingFile::new(state_path, Some(state_raw), owned)?);
    }
    let changed = INFERENCE_FIELDS
        .iter()
        .fold(removed_connectors, |changed, field| {
            profile.remove(*field).is_some() || changed
        });
    if changed {
        let update = PendingFile::new(profile_path, profile_raw, profile)?;
        persist_private_batch(&writes, &update)?;
    } else {
        persist_batch(&writes)?;
    }
    Ok(Some(
        json!({"success":true,"message":"Cowork config reset"}),
    ))
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}), true)
}
