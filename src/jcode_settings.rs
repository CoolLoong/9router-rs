use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item, Table, value};

use super::{
    CliTools, PendingFile, executable, in_config, persist_batch, persist_private_batch, read, reply,
};
use crate::provider_connections::truthy;

const KEY: &str = "JCODE_9ROUTER_API_KEY";
const DEFAULT_MODEL: &str = "cc/claude-opus-4-7";
const NO_CONFIGURATION: &str = "No configuration to remove";

pub(super) fn add_routes(router: Router<CliTools>) -> Router<CliTools> {
    router.route(
        "/api/cli-tools/jcode-settings",
        get(status).post(apply).delete(reset),
    )
}

pub(super) async fn host_status() -> Option<Value> {
    tokio::task::spawn_blocking(check).await.ok()?.ok()
}

struct Paths {
    config: PathBuf,
    dotenv: PathBuf,
}

fn paths() -> Result<Paths, String> {
    let home = std::env::home_dir().ok_or("Missing home directory")?;
    let config = home.join(".jcode/config.toml");
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map_or_else(|| home.join(".config"), PathBuf::from);
    Ok(Paths {
        config,
        dotenv: xdg.join("jcode/provider-9router.env"),
    })
}

fn check() -> Result<Value, String> {
    let Paths { config, .. } = paths()?;
    if !executable("jcode") && !config.parent().is_some_and(Path::exists) {
        return Ok(json!({
            "installed":false,
            "message":"jcode not installed. Install via: curl -fsSL https://raw.githubusercontent.com/1jehuang/jcode/master/scripts/install.sh | bash",
        }));
    }
    let mut loaded = read_regular(&config)
        .ok()
        .flatten()
        .and_then(|raw| String::from_utf8(raw).ok())
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        .and_then(|table| serde_json::to_value(table).ok())
        .unwrap_or_else(|| json!({"providers":{}}));
    let providers = &loaded["providers"];
    let configured = truthy(&providers["9router"])
        || providers.as_object().is_some_and(|providers| {
            providers.values().any(|provider| {
                provider["base_url"]
                    .as_str()
                    .is_some_and(|base| base.contains("localhost:20128"))
            })
        });
    redact(&mut loaded);
    Ok(json!({"installed":true,"config":loaded,"has9Router":configured,"configPath":config}))
}

async fn status(State(state): State<CliTools>) -> Response {
    complete(
        in_config(state.editing, check).await,
        "Failed to check jcode settings",
    )
}

async fn apply(State(state): State<CliTools>, request: Request) -> Response {
    let error = "Failed to configure jcode";
    let body = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|body| !body.is_null());
    let Some(body) = body else {
        return problem(StatusCode::INTERNAL_SERVER_ERROR, error);
    };
    if !truthy(&body["baseUrl"]) || !truthy(&body["apiKey"]) {
        return problem(StatusCode::BAD_REQUEST, "baseUrl and apiKey are required");
    }
    let Some(base) = body["baseUrl"].as_str() else {
        return problem(StatusCode::INTERNAL_SERVER_ERROR, error);
    };
    let Some(key) = body["apiKey"].as_str().and_then(quote_dotenv) else {
        return problem(
            StatusCode::BAD_REQUEST,
            "apiKey must be a supported single-line string",
        );
    };
    let model = match body["models"].as_array().and_then(|models| models.first()) {
        Some(model) => match model.as_str() {
            Some(model) => model.to_owned(),
            None => {
                return problem(
                    StatusCode::BAD_REQUEST,
                    "models must contain model identifiers",
                );
            }
        },
        None => DEFAULT_MODEL.to_owned(),
    };
    let base = if base.ends_with("/v1") {
        base.to_owned()
    } else {
        format!("{base}/v1")
    };
    let result = in_config(state.editing, move || {
        let paths = paths()?;
        let files = read_pair(&paths)?;
        let mut config = parse_config(files.config.as_deref())?;
        if !config.contains_key("providers") {
            config["providers"] = Item::Table(Table::new());
        }
        let providers = config["providers"]
            .as_table_like_mut()
            .ok_or("Invalid JCode providers table")?;
        let mut profile = Table::new();
        for (name, field) in [
            ("type", value("openai-compatible")),
            ("base_url", value(base)),
            ("auth", value("bearer")),
            ("api_key_env", value(KEY)),
            ("env_file", value("provider-9router.env")),
            ("default_model", value(model)),
            ("requires_api_key", value(true)),
        ] {
            profile.insert(name, field);
        }
        providers.insert("9router", Item::Table(profile));
        let dotenv = edit_dotenv(files.dotenv.as_deref(), Some(&key));
        persist_private_batch(
            &[PendingFile {
                path: paths.config.clone(),
                previous: files.config,
                next: config.to_string().into_bytes(),
            }],
            &PendingFile {
                path: paths.dotenv,
                previous: files.dotenv,
                next: dotenv,
            },
        )?;
        Ok(json!({
            "success":true,
            "message":"jcode configured successfully. Use: jcode --provider-profile 9router",
            "configPath":paths.config,
        }))
    })
    .await;
    complete(result, error)
}

async fn reset(State(state): State<CliTools>) -> Response {
    let result = in_config(state.editing, || {
        let paths = paths()?;
        let files = read_pair(&paths)?;
        let mut config = parse_config(files.config.as_deref())?;
        let mut writes = Vec::new();
        if let Some(providers) = config.get_mut("providers") {
            let providers = providers
                .as_table_like_mut()
                .ok_or("Invalid JCode providers table")?;
            if providers.remove("9router").is_some() {
                writes.push(PendingFile {
                    path: paths.config,
                    previous: files.config,
                    next: config.to_string().into_bytes(),
                });
            }
        }
        let private = files.dotenv.and_then(|previous| {
            let dotenv = edit_dotenv(Some(&previous), None);
            (previous != dotenv).then_some(PendingFile {
                path: paths.dotenv,
                previous: Some(previous),
                next: dotenv,
            })
        });
        if let Some(private) = private {
            persist_private_batch(&writes, &private)?;
        } else if writes.is_empty() {
            return Ok(json!({"success":true,"message":NO_CONFIGURATION}));
        } else {
            persist_batch(&writes)?;
        }
        Ok(json!({"success":true,"message":"9router configuration removed from jcode"}))
    })
    .await;
    complete(result, "Failed to remove jcode configuration")
}

struct Files {
    config: Option<Vec<u8>>,
    dotenv: Option<Vec<u8>>,
}

fn read_pair(paths: &Paths) -> Result<Files, String> {
    if let (Ok(config), Ok(dotenv)) = (paths.config.canonicalize(), paths.dotenv.canonicalize())
        && config == dotenv
    {
        return Err("JCode config and credential file must be distinct".into());
    }
    Ok(Files {
        config: read_regular(&paths.config)?,
        dotenv: read_regular(&paths.dotenv)?,
    })
}

fn read_regular(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => read(path),
        Ok(_) => Err("JCode configuration path is not a regular file".into()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn parse_config(bytes: Option<&[u8]>) -> Result<DocumentMut, String> {
    match bytes {
        None => Ok(DocumentMut::new()),
        Some(bytes) => std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| "Cannot parse existing JCode TOML".into()),
    }
}

fn quote_dotenv(secret: &str) -> Option<String> {
    if secret.chars().any(char::is_control) {
        return None;
    }
    if secret.contains(['"', '\\', '$', '`']) {
        return (!secret.contains('\'')).then(|| format!("'{secret}'"));
    }
    Some(format!("\"{secret}\""))
}

fn managed_line(line: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(line) else {
        return false;
    };
    let Some((key, _)) = text.split_once('=') else {
        return false;
    };
    let key = key.trim();
    let key = key
        .strip_prefix("export")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map_or(key, str::trim_start);
    key == KEY
}

fn edit_dotenv(raw: Option<&[u8]>, key: Option<&str>) -> Vec<u8> {
    let raw = raw.unwrap_or_default();
    let mut out = Vec::new();
    if raw.is_empty() && key.is_some() {
        out.extend_from_slice(b"# jcode provider environment variables\n");
    }
    let mut replacement = key;
    for line in raw.split_inclusive(|byte| *byte == b'\n') {
        if managed_line(line) {
            if let Some(value) = replacement.take() {
                out.extend_from_slice(format!("{KEY}={value}").as_bytes());
                if line.ends_with(b"\r\n") {
                    out.extend_from_slice(b"\r\n");
                } else if line.ends_with(b"\n") {
                    out.push(b'\n');
                }
            }
        } else {
            out.extend_from_slice(line);
        }
    }
    if let Some(value) = replacement {
        let newline = if raw
            .split_inclusive(|byte| *byte == b'\n')
            .next()
            .is_some_and(|line| line.ends_with(b"\r\n"))
        {
            "\r\n"
        } else {
            "\n"
        };
        let assignment = format!("{KEY}={value}{newline}");
        if !out.is_empty() && !out.ends_with(b"\n") {
            return [assignment.as_bytes(), out.as_slice()].concat();
        }
        out.extend_from_slice(assignment.as_bytes());
    }
    out
}

fn redact(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(redact),
        Value::Object(fields) => fields.retain(|name, value| {
            let normalized: String = name
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .map(|letter| letter.to_ascii_lowercase())
                .collect();
            if matches!(
                normalized.as_str(),
                "apikey"
                    | "accesstoken"
                    | "authtoken"
                    | "bearertoken"
                    | "refreshtoken"
                    | "idtoken"
                    | "token"
                    | "secret"
                    | "clientsecret"
                    | "privatekey"
                    | "password"
                    | "authorization"
                    | "credentials"
                    | "headers"
                    | "httpheaders"
                    | "cookies"
                    | "cookie"
                    | "env"
                    | "environment"
            ) {
                return false;
            }
            if matches!(normalized.as_str(), "baseurl" | "endpoint" | "url")
                && let Some(raw) = value.as_str()
                && let Ok(mut url) = url::Url::parse(raw)
                && (url.password().is_some()
                    || !url.username().is_empty()
                    || url.query().is_some()
                    || url.fragment().is_some())
            {
                let _ = url.set_username("");
                let _ = url.set_password(None);
                url.set_query(None);
                url.set_fragment(None);
                *value = json!(url.as_str());
            }
            redact(value);
            true
        }),
        _ => {}
    }
}

fn complete(result: Result<Value, String>, error: &str) -> Response {
    match result {
        Ok(value) => reply(StatusCode::OK, value),
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

fn problem(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}))
}
