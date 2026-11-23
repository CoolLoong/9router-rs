use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde_json::{Map, Value, json};
use toml::{Table, Value as Toml};
use toml_edit::{DocumentMut, Item, TableLike, value};

use super::{CliTools, executable, in_config, parse_toml, persist, read, reply, resolve_key};

pub(super) fn add_routes(router: Router<CliTools>) -> Router<CliTools> {
    router.route(
        "/api/cli-tools/deepseek-tui-settings",
        get(status).post(apply).delete(reset),
    )
}

pub(super) async fn host_status() -> Option<Value> {
    tokio::task::spawn_blocking(check).await.ok()?.ok()
}

fn path() -> Result<PathBuf, String> {
    Ok(std::env::home_dir()
        .ok_or("Unable to locate the home directory")?
        .join(".deepseek/config.toml"))
}

fn openai(config: &Table) -> Option<&Table> {
    config.get("providers")?.get("openai")?.as_table()
}

fn add_sections(settings: &mut Map<String, Value>, name: &str, table: &Table) {
    let mut fields = Map::new();
    for (key, value) in table {
        if let Toml::Table(section) = value {
            let label = if name.is_empty() {
                key.clone()
            } else {
                format!("{name}.{key}")
            };
            add_sections(settings, &label, section);
        } else if !key.eq_ignore_ascii_case("api_key") && !key.eq_ignore_ascii_case("apiKey") {
            let rendered = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            fields.insert(key.clone(), Value::String(rendered));
        }
    }
    if name.is_empty() {
        settings.extend(fields);
    } else {
        settings.insert(name.into(), Value::Object(fields));
    }
}

fn check() -> Result<Value, String> {
    let path = path()?;
    let raw = read(&path)?;
    if raw.is_none() && !executable("deepseek") {
        return Ok(
            json!({"installed":false,"settings":null,"message":"DeepSeek TUI is not installed"}),
        );
    }
    let config = parse_toml(raw.as_deref())?;
    let configured = config.get("provider").and_then(Toml::as_str) == Some("openai")
        && openai(&config)
            .and_then(|table| table.get("base_url"))
            .and_then(Toml::as_str)
            .is_some_and(|url| {
                ["localhost", "127.0.0.1", "0.0.0.0"]
                    .iter()
                    .any(|host| url.contains(host))
            });
    let mut settings = Map::new();
    add_sections(&mut settings, "", &config);
    Ok(json!({"installed":true,"settings":settings,"has9Router":configured,"configPath":path}))
}

async fn status(State(state): State<CliTools>) -> Response {
    answer(
        in_config(state.editing, check).await,
        "Failed to check deepseek-tui settings",
    )
}

async fn apply(State(state): State<CliTools>, request: Request) -> Response {
    let failure = "Failed to update deepseek-tui settings";
    let Some(body) = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|body| !body.is_null())
    else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, failure);
    };
    let Some((base, model)) = body["baseUrl"]
        .as_str()
        .filter(|base| !base.is_empty())
        .zip(body["model"].as_str().filter(|model| !model.is_empty()))
    else {
        return error(StatusCode::BAD_REQUEST, "baseUrl and model are required");
    };
    let url = if base.ends_with("/v1") {
        base.to_owned()
    } else {
        format!("{base}/v1")
    };
    let model = model.to_owned();
    let key = resolve_key(&state.storage, body["apiKey"].as_str()).await;
    answer(in_config(state.editing, move || {
        let path = path()?;
        let mut config = document(read(&path)?.as_deref())?;
        let providers = table_entry(config.as_table_mut(), "providers")?;
        let openai = table_entry(providers, "openai")?;
        for (field, field_value) in [("base_url", url), ("api_key", key), ("model", model)] {
            openai.insert(field, value(field_value));
        }
        config.insert("provider", value("openai"));
        write(&path, &config)?;
        Ok(json!({"success":true,"message":"DeepSeek TUI settings applied successfully!","configPath":path}))
    }).await, failure)
}

async fn reset(State(state): State<CliTools>) -> Response {
    answer(
        in_config(state.editing, || {
            let path = path()?;
            let Some(raw) = read(&path)? else {
                return Ok(json!({"success":true,"message":"No config file to reset"}));
            };
            let mut config = document(Some(&raw))?;
            if let Some(providers) = config
                .get_mut("providers")
                .and_then(Item::as_table_like_mut)
            {
                if let Some(openai) = providers
                    .get_mut("openai")
                    .and_then(Item::as_table_like_mut)
                {
                    for field in ["base_url", "api_key", "model"] {
                        openai.remove(field);
                    }
                    if openai.is_empty() {
                        providers.remove("openai");
                    }
                }
                if providers.is_empty() {
                    config.remove("providers");
                }
            }
            config.insert("provider", value("deepseek"));
            write(&path, &config)?;
            Ok(json!({"success":true,"message":"9router config reset to DeepSeek defaults"}))
        })
        .await,
        "Failed to reset deepseek-tui settings",
    )
}

fn document(raw: Option<&[u8]>) -> Result<DocumentMut, String> {
    let input = std::str::from_utf8(raw.unwrap_or_default()).map_err(|error| error.to_string())?;
    input
        .parse::<DocumentMut>()
        .map_err(|_| "Cannot parse DeepSeek TUI configuration".into())
}

fn table_entry<'a>(
    table: &'a mut dyn TableLike,
    name: &str,
) -> Result<&'a mut dyn TableLike, String> {
    table
        .entry(name)
        .or_insert_with(|| Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| format!("{name} must be a TOML table"))
}

fn write(path: &Path, config: &DocumentMut) -> Result<(), String> {
    persist(path, Some(config.to_string().as_bytes()))
}

fn answer(result: Result<Value, String>, message: &str) -> Response {
    match result {
        Ok(value) => reply(StatusCode::OK, value),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}))
}
