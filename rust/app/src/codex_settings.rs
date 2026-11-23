use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use regex::Regex;
use serde_json::{Value, json};
use toml::{Table, Value as Toml};

use crate::cli::Tool;
use crate::cli_connect::write_private;
use crate::dashboard_session;
use crate::observed_response::into_http;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::storage::Storage;

const VARY: &str = "Accept-Encoding";
const SUFFIX: &str = ".config.toml";

#[derive(Clone)]
struct CodexState {
    storage: Storage,
    editing: Arc<Mutex<()>>,
}

pub fn router(storage: Storage) -> Router {
    let state = CodexState {
        storage,
        editing: Arc::default(),
    };
    Router::new()
        .route(
            "/api/cli-tools/codex-settings",
            get(settings).post(settings).delete(settings),
        )
        .route(
            "/api/cli-tools/codex-profiles",
            get(profiles).post(profiles).delete(profiles),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(State(state): State<CodexState>, request: Request, next: Next) -> Response {
    if dashboard_session::authenticated(&state.storage, request.headers()).await {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::VARY, VARY)],
        axum::Json(json!({"error":"Unauthorized"})),
    )
        .into_response()
}

async fn settings(State(state): State<CodexState>, request: Request) -> Response {
    let method = request.method().clone();
    let message = match method {
        Method::POST => "Failed to update codex settings",
        Method::DELETE => "Failed to reset codex settings",
        _ => "Failed to check codex settings",
    };
    let body = if method == Method::POST {
        match parse_body(request).await {
            Ok(body) => body,
            Err(()) => return reply(failed(message)),
        }
    } else {
        Value::Null
    };
    respond_blocking(state, message, move |path| match method {
        Method::POST => apply_settings(path, &body),
        Method::DELETE => reset_settings(path),
        _ => status(path),
    })
    .await
}

async fn profiles(State(state): State<CodexState>, request: Request) -> Response {
    let method = request.method().clone();
    let message = match method {
        Method::POST => "Failed to save profile",
        Method::DELETE => "Failed to delete profile",
        _ => "Failed to read profiles",
    };
    let body = if method == Method::GET || method == Method::HEAD {
        Value::Null
    } else {
        match parse_body(request).await {
            Ok(body) => body,
            Err(()) => return reply(failed(message)),
        }
    };
    respond_blocking(state, message, move |path| {
        let directory = path.parent().ok_or(())?;
        match method {
            Method::POST => save_profile(directory, &body),
            Method::DELETE => delete_profile(directory, &body),
            _ => list_profiles(directory),
        }
    })
    .await
}

type Reply = (StatusCode, Value);

async fn respond_blocking(
    state: CodexState,
    message: &'static str,
    inspect: impl FnOnce(&Path) -> Result<Reply, ()> + Send + 'static,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let _editor = state.editing.lock().map_err(|_| ())?;
        inspect(&config_path()?)
    })
    .await;
    match result {
        Ok(Ok(result)) => reply(result),
        _ => reply(failed(message)),
    }
}

async fn parse_body(request: Request) -> Result<Value, ()> {
    let raw = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| ())?;
    serde_json::from_slice::<Value>(&raw)
        .ok()
        .filter(|value| !value.is_null())
        .ok_or(())
}

fn config_path() -> Result<PathBuf, ()> {
    let home = std::env::home_dir().ok_or(())?;
    Tool::Codex.files(&home, None).into_iter().next().ok_or(())
}

fn status(path: &Path) -> Result<Reply, ()> {
    if !installed() && !path.exists() {
        return Ok(ok(
            json!({"installed":false,"config":null,"message":"Codex CLI is not installed"}),
        ));
    }
    let content = read(path)?.map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let configured = content.as_deref().is_some_and(|value| {
        value.contains("model_provider = \"9router\"")
            || value.contains("[model_providers.9router]")
    });
    let router = content
        .as_deref()
        .and_then(|text| toml::from_str::<Toml>(text).ok())
        .and_then(|config| {
            let base = config
                .get("model_providers")?
                .get("9router")?
                .get("base_url")?
                .as_str()?;
            let active = config.get("model_provider").and_then(Toml::as_str) == Some("9router");
            let model = active
                .then(|| config.get("model").and_then(Toml::as_str))
                .flatten();
            let subagent = config
                .get("agents")
                .and_then(|agents| agents.get("default_subagent_model"))
                .and_then(Toml::as_str);
            Some(json!({"baseUrl":base,"model":model,"subagentModel":subagent}))
        });
    Ok(ok(
        json!({"installed":true,"config":content,"has9Router":configured,"configPath":path,"routerConfig":router}),
    ))
}

pub(crate) async fn host_status() -> Option<Value> {
    tokio::task::spawn_blocking(|| status(&config_path()?).map(|(_, value)| value))
        .await
        .ok()?
        .ok()
}

fn installed() -> bool {
    let installed = crate::cli_tools::executable("codex");
    #[cfg(windows)]
    let installed = installed
        || std::env::var_os("APPDATA").is_some_and(|appdata| {
            let path = PathBuf::from(appdata).join("npm");
            let extensions =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
            extensions
                .split(';')
                .any(|extension| path.join(format!("codex{extension}")).is_file())
        });
    installed
}

fn apply_settings(path: &Path, body: &Value) -> Result<Reply, ()> {
    if ["baseUrl", "apiKey", "model"]
        .into_iter()
        .any(|field| !truthy(&body[field]))
    {
        return Ok(error(
            StatusCode::BAD_REQUEST,
            "baseUrl, apiKey and model are required",
        ));
    }
    let base = body["baseUrl"].as_str().ok_or(())?;
    let key = crate::chat_output::js_text(&body["apiKey"]);
    let model = Toml::try_from(body["model"].clone()).map_err(|_| ())?;
    let subagent = if truthy(&body["subagentModel"]) {
        Toml::try_from(body["subagentModel"].clone()).map_err(|_| ())?
    } else {
        model.clone()
    };
    let mut config = read(path)
        .ok()
        .flatten()
        .and_then(|bytes| parse_toml(&bytes).ok())
        .unwrap_or_default();
    config.insert("model".into(), model);
    config.insert("model_provider".into(), Toml::String("9router".into()));
    let base = if base.ends_with("/v1") {
        base.to_owned()
    } else {
        format!("{base}/v1")
    };
    let headers = Table::from_iter([(
        "Authorization".into(),
        Toml::String(format!("Bearer {key}")),
    )]);
    let provider = Table::from_iter([
        ("name".into(), Toml::String("9Router".into())),
        ("base_url".into(), Toml::String(base)),
        ("wire_api".into(), Toml::String("responses".into())),
        ("http_headers".into(), Toml::Table(headers)),
    ]);
    table(&mut config, "model_providers")?.insert("9router".into(), Toml::Table(provider));
    let agents = table(&mut config, "agents")?;
    agents.remove("subagent");
    agents.insert("default_subagent_model".into(), subagent);
    save_toml(path, &config)?;
    Ok(ok(
        json!({"success":true,"message":"Codex settings applied successfully!","configPath":path}),
    ))
}

fn reset_settings(path: &Path) -> Result<Reply, ()> {
    let Some(raw) = read(path)? else {
        return Ok(ok(
            json!({"success":true,"message":"No config file to reset"}),
        ));
    };
    let mut config = parse_toml(&raw)?;
    if config.get("model_provider").and_then(Toml::as_str) == Some("9router") {
        config.remove("model");
        config.remove("model_provider");
    }
    if let Some(providers) = config
        .get_mut("model_providers")
        .and_then(Toml::as_table_mut)
    {
        providers.remove("9router");
    }
    if let Some(agents) = config.get_mut("agents").and_then(Toml::as_table_mut) {
        agents.remove("default_subagent_model");
        agents.remove("subagent");
    }
    save_toml(path, &config)?;
    let _ = clear_legacy_auth(&path.with_file_name("auth.json"));
    Ok(ok(
        json!({"success":true,"message":"9Router settings removed successfully"}),
    ))
}

fn clear_legacy_auth(path: &Path) -> Result<(), ()> {
    let Some(raw) = read(path)? else {
        return Ok(());
    };
    let mut original: Value = serde_json::from_slice(&raw).map_err(|_| ())?;
    let fields = original.as_object_mut().ok_or(())?;
    fields.shift_remove("OPENAI_API_KEY");
    fields.shift_remove("auth_mode");
    if fields.is_empty() {
        fs::remove_file(path).map_err(|_| ())
    } else {
        let encoded = serde_json::to_vec_pretty(&original).map_err(|_| ())?;
        write_private(path, &encoded, true).map_err(|_| ())
    }
}

fn parse_toml(bytes: &[u8]) -> Result<Table, ()> {
    toml::from_str(&String::from_utf8_lossy(bytes)).map_err(|_| ())
}

fn table<'a>(root: &'a mut Table, key: &str) -> Result<&'a mut Table, ()> {
    let node = root
        .entry(key.to_owned())
        .or_insert(Toml::Table(Table::new()));
    if !matches!(node, Toml::Table(_) | Toml::Array(_)) {
        *node = Toml::Table(Table::new());
    }
    node.as_table_mut().ok_or(())
}

fn save_toml(path: &Path, config: &Table) -> Result<(), ()> {
    let content = toml::to_string(config).map_err(|_| ())?;
    write_private(path, content.as_bytes(), path.exists()).map_err(|_| ())
}

fn list_profiles(directory: &Path) -> Result<Reply, ()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(ok(json!({"profiles":[]})));
        }
        Err(_) => return Err(()),
    };
    let mut profiles = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| ())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = name.strip_suffix(SUFFIX) else {
            continue;
        };
        let content = fs::read(entry.path())
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        profiles.push((name.to_owned(), profile(name, profile_model(&content))));
    }
    profiles.sort_by(|(first, _), (second, _)| compare_profiles(first, second));
    Ok(ok(
        json!({"profiles":profiles.into_iter().map(|(_, profile)| profile).collect::<Vec<_>>()}),
    ))
}

fn profile_model(content: &str) -> &str {
    static MODEL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?:^|[\r\n\u{2028}\u{2029}])[\s\u{feff}--\u{85}]*model[\s\u{feff}--\u{85}]*=[\s\u{feff}--\u{85}]*(?:"([^"\n]+)"|'([^"\n]+)')"#)
            .expect("Codex profile model pattern")
    });
    MODEL
        .captures(content)
        .and_then(|fields| fields.get(1).or_else(|| fields.get(2)))
        .map_or("", |field| field.as_str())
}

fn compare_profiles(first: &str, second: &str) -> std::cmp::Ordering {
    fn primary(text: &str) -> impl Iterator<Item = u8> + '_ {
        text.bytes().map(|byte| match byte {
            b'_' => 1,
            b'-' => 2,
            byte => byte.to_ascii_lowercase(),
        })
    }
    primary(first)
        .cmp(primary(second))
        .then_with(|| {
            first
                .bytes()
                .map(|byte| byte.is_ascii_uppercase())
                .cmp(second.bytes().map(|byte| byte.is_ascii_uppercase()))
        })
        .then_with(|| first.cmp(second))
}

fn save_profile(directory: &Path, body: &Value) -> Result<Reply, ()> {
    let Some(name) = profile_name(&body["name"]) else {
        return Ok(error(
            StatusCode::BAD_REQUEST,
            "Profile name can only contain alphanumeric characters, dashes, underscores and cannot be 'config'",
        ));
    };
    let model = body["model"].as_str().map(js_trim).unwrap_or_default();
    if model.is_empty() || model.encode_utf16().count() > 256 || model.contains(['\r', '\n', '"']) {
        return Ok(error(StatusCode::BAD_REQUEST, "Invalid model name"));
    }
    let path = directory.join(format!("{name}{SUFFIX}"));
    let content = format!("# codex -p {name}\nmodel = \"{model}\"\nmodel_provider = \"9router\"\n");
    write_private(&path, content.as_bytes(), path.exists()).map_err(|_| ())?;
    Ok(ok(json!({"success":true,"profile":profile(&name,model)})))
}

fn delete_profile(directory: &Path, body: &Value) -> Result<Reply, ()> {
    let Some(name) = profile_name(&body["name"]) else {
        return Ok(error(StatusCode::BAD_REQUEST, "Invalid profile name"));
    };
    let path = directory.join(format!("{name}{SUFFIX}"));
    match fs::remove_file(path) {
        Ok(()) => Ok(ok(
            json!({"success":true,"message":format!("Profile {name} deleted")}),
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            Ok(self::error(StatusCode::NOT_FOUND, "Profile not found"))
        }
        Err(_) => Err(()),
    }
}

fn profile_name(value: &Value) -> Option<String> {
    let name = js_trim(value.as_str()?).to_lowercase();
    (!name.is_empty()
        && name != "config"
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then_some(name)
}

fn profile(name: &str, model: &str) -> Value {
    json!({"name":name,"model":model,"command":format!("codex -p {name}")})
}

fn read(path: &Path) -> Result<Option<Vec<u8>>, ()> {
    match fs::read(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

fn ok(value: Value) -> Reply {
    (StatusCode::OK, value)
}

fn error(status: StatusCode, message: &str) -> Reply {
    (status, json!({"error":message}))
}

fn failed(message: &str) -> Reply {
    error(StatusCode::INTERNAL_SERVER_ERROR, message)
}

fn reply((status, value): Reply) -> Response {
    into_http(Ok(json!({
        "status":status.as_u16(),"headers":{"content-type":"application/json"},"body":value.to_string(),
    })))
}
