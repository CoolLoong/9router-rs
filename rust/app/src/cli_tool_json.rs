use std::fs;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Extension, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde_json::{Map, Value, json};

use crate::chat_output::js_text;
use crate::cli::{Tool, without_trailing_commas};
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;

use super::{
    CliTools, PendingFile, executable, failure, in_config, persist, persist_batch, read, reply,
    resolve_key,
};

#[derive(Clone, Copy)]
enum JsonTool {
    OpenClaw,
    Pi,
}

pub(super) fn add_routes(router: Router<CliTools>) -> Router<CliTools> {
    [("openclaw", JsonTool::OpenClaw), ("pi", JsonTool::Pi)]
        .into_iter()
        .fold(router, |router, (id, tool)| {
            router.route(
                &format!("/api/cli-tools/{id}-settings"),
                get(status)
                    .post(apply)
                    .delete(restore)
                    .layer(Extension(tool)),
            )
        })
}

pub(super) async fn tool_status(id: &str) -> Option<Value> {
    let tool = match id {
        "openclaw" => JsonTool::OpenClaw,
        "pi" => JsonTool::Pi,
        _ => return None,
    };
    tokio::task::spawn_blocking(move || config_status(tool))
        .await
        .ok()?
        .ok()
}

async fn status(State(state): State<CliTools>, Extension(tool): Extension<JsonTool>) -> Response {
    let result = in_config(state.editing, move || config_status(tool)).await;
    tool_reply(tool, result, "Failed to check openclaw settings")
}

async fn apply(
    State(state): State<CliTools>,
    Extension(tool): Extension<JsonTool>,
    request: Request,
) -> Response {
    let body = to_bytes(request.into_body(), 128 * 1024 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let update_error = "Failed to update openclaw settings";
    let Some(body) = body else {
        return match tool {
            JsonTool::OpenClaw => openclaw_error(StatusCode::INTERNAL_SERVER_ERROR, update_error),
            JsonTool::Pi => failure(StatusCode::BAD_REQUEST, "Invalid JSON body"),
        };
    };
    match tool {
        JsonTool::OpenClaw if body.is_null() => {
            return openclaw_error(StatusCode::INTERNAL_SERVER_ERROR, update_error);
        }
        JsonTool::OpenClaw if !truthy(&body["baseUrl"]) || !truthy(&body["model"]) => {
            return openclaw_error(StatusCode::BAD_REQUEST, "baseUrl and model are required");
        }
        JsonTool::Pi if !truthy(&body["baseUrl"]) => {
            return failure(StatusCode::BAD_REQUEST, "baseUrl is required");
        }
        _ => (),
    }
    let Some(base) = body["baseUrl"].as_str() else {
        return tool_reply(
            tool,
            Err("baseUrl.endsWith is not a function".into()),
            update_error,
        );
    };
    let base = if base.ends_with("/v1") {
        base.to_owned()
    } else {
        format!("{base}/v1")
    };
    let resolved_key = if matches!(tool, JsonTool::Pi) && !truthy(&body["apiKey"]) {
        resolve_key(&state.storage, None).await
    } else {
        String::new()
    };
    let result = in_config(state.editing, move || {
        let path = config_path(tool)?;
        match tool {
            JsonTool::OpenClaw => apply_openclaw(path, &body, &base),
            JsonTool::Pi => apply_pi(&path, &body, &base, &resolved_key),
        }
    })
    .await;
    tool_reply(tool, result, update_error)
}

async fn restore(State(state): State<CliTools>, Extension(tool): Extension<JsonTool>) -> Response {
    let result = in_config(state.editing, move || {
        let path = config_path(tool)?;
        match tool {
            JsonTool::OpenClaw => reset_openclaw(&path),
            JsonTool::Pi => reset_pi(&path),
        }
    })
    .await;
    tool_reply(tool, result, "Failed to reset openclaw settings")
}

fn tool_reply(tool: JsonTool, result: Result<Value, String>, message: &str) -> Response {
    match result {
        Ok(value) => reply(StatusCode::OK, value),
        Err(error) => match tool {
            JsonTool::OpenClaw => openclaw_error(StatusCode::INTERNAL_SERVER_ERROR, message),
            JsonTool::Pi => failure(StatusCode::INTERNAL_SERVER_ERROR, error),
        },
    }
}

fn openclaw_error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}))
}

fn config_path(tool: JsonTool) -> Result<PathBuf, String> {
    let home = std::env::home_dir().ok_or("Unable to locate the home directory")?;
    Ok(match tool {
        JsonTool::OpenClaw => home.join(".openclaw/openclaw.json"),
        JsonTool::Pi => Tool::Pi.files(&home, None).remove(0),
    })
}

fn config_status(tool: JsonTool) -> Result<Value, String> {
    let path = config_path(tool)?;
    let (command, message) = match tool {
        JsonTool::OpenClaw => ("openclaw", "Open Claw CLI is not installed"),
        JsonTool::Pi => ("pi", "Pi CLI is not installed"),
    };
    let installed = path.exists() || executable(command);
    let raw = fs::read(&path).ok();
    match tool {
        JsonTool::OpenClaw if !installed => {
            Ok(json!({"installed":false,"settings":null,"message":message}))
        }
        JsonTool::Pi if !installed => {
            Ok(json!({"installed":false,"config":null,"message":message}))
        }
        JsonTool::OpenClaw => {
            let settings = raw
                .and_then(|raw| {
                    serde_json::from_slice::<Value>(&without_trailing_commas(&raw)).ok()
                })
                .unwrap_or_default();
            let agents = openclaw_agents(&settings["agents"]["list"])?
                .iter()
                .map(|agent| {
                    let current = agent["agentDir"]
                        .as_str()
                        .filter(|path| !path.is_empty())
                        .and_then(|dir| fs::read(Path::new(dir).join("models.json")).ok())
                        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
                        .and_then(|data| {
                            let id = &data["providers"]["9router"]["models"][0]["id"];
                            truthy(id).then(|| id.clone())
                        });
                    let mut entry = agent
                        .as_object()
                        .cloned()
                        .ok_or("Open Claw agent must be an object")?;
                    entry.insert("model".into(), agent_model(&agent["model"]));
                    entry.insert("currentModel".into(), json!(current));
                    Ok(Value::Object(entry))
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(json!({
                "installed":true,"settings":settings,"agents":agents,
                "has9Router":truthy(&settings["models"]["providers"]["9router"]),"settingsPath":path,
            }))
        }
        JsonTool::Pi => {
            let config = raw
                .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
                .unwrap_or_default();
            let providers = &config["providers"];
            let configured = truthy(&providers["9router"]["baseUrl"])
                || providers.as_object().is_some_and(|providers| {
                    providers.values().any(|provider| {
                        provider["baseUrl"]
                            .as_str()
                            .is_some_and(|base| base.contains("20128"))
                    })
                });
            Ok(json!({"installed":true,"config":config,"has9Router":configured,"configPath":path}))
        }
    }
}

fn object_field<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>, String> {
    let value = object.entry(key).or_insert(Value::Null);
    if !truthy(value) {
        *value = json!({});
    }
    value
        .as_object_mut()
        .ok_or_else(|| format!("{key} must be a JSON object"))
}

fn agent_model(model: &Value) -> Value {
    match model {
        Value::String(_) => model.clone(),
        Value::Object(fields) => fields
            .get("primary")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| json!("")),
        _ => json!(""),
    }
}

fn openclaw_agents(agents: &Value) -> Result<&[Value], String> {
    if !truthy(agents) {
        return Ok(&[]);
    }
    agents
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| "Open Claw agents.list must be an array".into())
}

fn openclaw_provider<'a>(
    base: &str,
    key: &Value,
    models: impl IntoIterator<Item = &'a str>,
) -> Value {
    let models: Vec<_> = models
        .into_iter()
        .map(|model| {
            let name = model
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or(model);
            json!({"id":model,"name":name})
        })
        .collect();
    json!({"baseUrl":base,"apiKey":key,"api":"openai-completions","models":models})
}

fn apply_openclaw(path: PathBuf, body: &Value, base: &str) -> Result<Value, String> {
    let model = body["model"].as_str().ok_or("model must be a string")?;
    let overrides = match body.get("agentModels") {
        Some(Value::Object(overrides)) => Some(overrides),
        None => None,
        _ => return Err("agentModels must be a JSON object".into()),
    };
    let fallback_key = json!("your_api_key");
    let key = body
        .get("apiKey")
        .filter(|key| truthy(key))
        .unwrap_or(&fallback_key);
    let previous = read(&path)?;
    let mut settings = parse_openclaw(previous.as_deref())?;
    let update_agents = overrides.is_some();
    let overrides = match overrides {
        Some(overrides) => overrides.clone(),
        None => openclaw_agents(&settings["agents"]["list"])?
            .iter()
            .filter_map(|agent| {
                let id = agent.get("id").map(js_text)?;
                let selected = agent_model(&agent["model"]);
                let model = selected.as_str()?.strip_prefix("9router/")?;
                (!model.is_empty()).then(|| (id, json!(model)))
            })
            .collect(),
    };
    let mut models = vec![model];
    for id in overrides.values().filter(|id| truthy(id)) {
        let id = id.as_str().ok_or("agent model must be a string")?;
        if !models.contains(&id) {
            models.push(id);
        }
    }
    let agents = object_field(&mut settings, "agents")?;
    let defaults = object_field(agents, "defaults")?;
    let default = object_field(defaults, "model")?;
    default.insert("primary".into(), json!(format!("9router/{model}")));
    let allowlist = object_field(defaults, "models")?;
    allowlist.retain(|key, _| !key.starts_with("9router/"));
    for id in &models {
        allowlist.insert(format!("9router/{id}"), json!({}));
    }
    let mut writes = Vec::new();
    if let Some(list) = agents.get_mut("list").filter(|value| truthy(value)) {
        let list = list
            .as_array_mut()
            .ok_or("Open Claw agents.list must be an array")?;
        for agent in list {
            let fields = agent
                .as_object_mut()
                .ok_or("Open Claw agent must be an object")?;
            let current = agent_model(fields.get("model").unwrap_or(&Value::Null));
            let current = current
                .as_str()
                .ok_or("Open Claw model id must be a string")?;
            let agent_id = fields
                .get("id")
                .map(js_text)
                .unwrap_or_else(|| "undefined".into());
            let override_model = overrides
                .get(&agent_id)
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty());
            if update_agents && current.strip_prefix("9router/") != override_model {
                if current.starts_with("9router/") {
                    fields.shift_remove("model");
                }
                if let Some(id) = override_model {
                    fields.insert("model".into(), json!(format!("9router/{id}")));
                }
            }
            if let Some(dir) = fields.get("agentDir").filter(|dir| truthy(dir)) {
                let dir = dir.as_str().ok_or("Open Claw agentDir must be a path")?;
                let target = Path::new(dir).join("models.json");
                let previous = read(&target)?;
                let mut existing = super::parse_json(previous.as_deref())?;
                object_field(&mut existing, "providers")?.insert(
                    "9router".into(),
                    openclaw_provider(base, key, [override_model.unwrap_or(model)]),
                );
                writes.push(PendingFile::new(target, previous, existing)?);
            }
        }
    }
    object_field(object_field(&mut settings, "models")?, "providers")?
        .insert("9router".into(), openclaw_provider(base, key, models));
    writes.push(PendingFile::new(path.clone(), previous, settings)?);
    persist_batch(&writes)?;
    Ok(
        json!({"success":true,"message":"Open Claw settings applied successfully!","settingsPath":path}),
    )
}

fn parse_openclaw(raw: Option<&[u8]>) -> Result<Map<String, Value>, String> {
    let normalized = raw.map(without_trailing_commas);
    super::parse_json(normalized.as_deref())
}

fn reset_openclaw(path: &Path) -> Result<Value, String> {
    let Some(raw) = read(path)? else {
        return Ok(json!({"success":true,"message":"No settings file to reset"}));
    };
    let mut config = parse_openclaw(Some(&raw))?;
    if let Some(models) = config.get_mut("models").and_then(Value::as_object_mut) {
        remove_provider(models);
    }
    if let Some(defaults) = config
        .get_mut("agents")
        .and_then(|agents| agents.get_mut("defaults"))
        .and_then(Value::as_object_mut)
    {
        if let Some(allowlist) = defaults.get_mut("models").and_then(Value::as_object_mut) {
            allowlist.retain(|id, _| !id.starts_with("9router/"));
            if allowlist.is_empty() {
                defaults.shift_remove("models");
            }
        }
        if let Some(model) = defaults.get_mut("model").and_then(Value::as_object_mut)
            && model
                .get("primary")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with("9router/"))
        {
            model.shift_remove("primary");
        }
    }
    let encoded = super::serialize_json(config)?;
    persist(path, Some(encoded.as_bytes()))?;
    Ok(json!({"success":true,"message":"9Router settings removed successfully"}))
}

fn apply_pi(path: &Path, body: &Value, base: &str, fallback_key: &str) -> Result<Value, String> {
    let existing = read(path)?;
    let mut config = super::parse_json(existing.as_deref())?;
    let providers = object_field(&mut config, "providers")?;
    let existing = providers.get("9router").cloned().unwrap_or_default();
    let mut provider = existing.as_object().cloned().unwrap_or_default();
    let mut previous_models = Map::new();
    if let Some(models) = existing["models"].as_array() {
        for model in models {
            let model = if model.is_string() {
                json!({"id":model})
            } else {
                model.clone()
            };
            if truthy(&model["id"]) {
                previous_models.insert(js_text(&model["id"]), model);
            }
        }
    }
    let mut models = Vec::new();
    let placeholder = json!("provider/model-id");
    if let Some(list) = body["models"].as_array().filter(|list| !list.is_empty()) {
        for model in list {
            if model.is_null() {
                return Err("Cannot read properties of null (reading 'id')".into());
            }
            let mut fields = model.as_object().cloned().unwrap_or_default();
            let id = if model.is_string() {
                model
            } else {
                model
                    .get("id")
                    .filter(|id| truthy(id))
                    .unwrap_or(&placeholder)
            };
            fields.insert("id".into(), id.clone());
            models.push(pi_model(fields, &previous_models, id));
        }
    } else {
        let id = body
            .get("model")
            .filter(|value| truthy(value))
            .unwrap_or(&placeholder);
        models.push(pi_model(
            Map::from_iter([("id".into(), id.clone())]),
            &previous_models,
            id,
        ));
    }
    let key = body
        .get("apiKey")
        .filter(|key| truthy(key))
        .or_else(|| existing.get("apiKey").filter(|key| truthy(key)))
        .cloned()
        .unwrap_or_else(|| json!(fallback_key));
    let api = existing
        .get("api")
        .filter(|api| truthy(api))
        .cloned()
        .unwrap_or_else(|| json!("openai-completions"));
    provider.extend([
        ("baseUrl".into(), json!(base)),
        ("apiKey".into(), key),
        ("api".into(), api),
        ("models".into(), json!(models)),
    ]);
    providers.insert("9router".into(), Value::Object(provider));
    let encoded = super::serialize_json(config)?;
    persist(path, Some(encoded.as_bytes()))?;
    Ok(json!({
        "success":true,"message":"Pi settings applied! Use /model in Pi to select the 9Router model.","configPath":path,
    }))
}

fn pi_model(input: Map<String, Value>, models: &Map<String, Value>, id: &Value) -> Value {
    let previous = models.get(&js_text(id)).and_then(Value::as_object);
    let mut model = previous.cloned().unwrap_or_default();
    let name = input
        .get("name")
        .filter(|name| truthy(name) && *name != id)
        .or_else(|| model.get("name").filter(|name| truthy(name)))
        .unwrap_or(id)
        .clone();
    let context = cap(&input, &model, "contextWindow", 200_000, 2_000_000);
    let tokens = cap(&input, &model, "maxTokens", 64_000, 200_000);
    model.extend(input);
    model.extend([
        ("name".into(), name),
        ("contextWindow".into(), context),
        ("maxTokens".into(), tokens),
    ]);
    Value::Object(model)
}

fn cap(
    input: &Map<String, Value>,
    previous: &Map<String, Value>,
    field: &str,
    fallback: u64,
    max: u64,
) -> Value {
    let value = input
        .get(field)
        .filter(|value| !value.is_null())
        .or_else(|| previous.get(field));
    let amount = match value {
        Some(Value::Bool(true)) => Some(1.0),
        Some(Value::Number(number)) => number.as_f64(),
        Some(value @ (Value::String(_) | Value::Array(_))) => {
            let text = js_text(value);
            let text = js_trim(&text);
            match text.get(..2).map(str::to_ascii_lowercase).as_deref() {
                Some("0x") => u64::from_str_radix(&text[2..], 16)
                    .ok()
                    .map(|value| value as f64),
                Some("0o") => u64::from_str_radix(&text[2..], 8)
                    .ok()
                    .map(|value| value as f64),
                Some("0b") => u64::from_str_radix(&text[2..], 2)
                    .ok()
                    .map(|value| value as f64),
                _ => text.parse::<f64>().ok(),
            }
        }
        _ => None,
    };
    match amount.filter(|amount| *amount > 0.0 && *amount <= max as f64) {
        Some(amount) if amount.fract() == 0.0 => json!(amount as u64),
        Some(amount) => json!(amount),
        None => json!(fallback),
    }
}

fn remove_provider(config: &mut Map<String, Value>) -> bool {
    let Some(providers) = config.get_mut("providers").and_then(Value::as_object_mut) else {
        return false;
    };
    let changed = providers.get("9router").is_some_and(truthy);
    providers.shift_remove("9router");
    if providers.is_empty() {
        config.shift_remove("providers");
    }
    changed
}

fn reset_pi(path: &Path) -> Result<Value, String> {
    if let Some(mut config) = read(path)
        .ok()
        .flatten()
        .as_deref()
        .and_then(|raw| super::parse_json(Some(raw)).ok())
    {
        if remove_provider(&mut config) {
            let encoded = super::serialize_json(config)?;
            persist(path, Some(encoded.as_bytes()))?;
        }
        return Ok(json!({"success":true,"message":"9Router removed from Pi"}));
    }
    Ok(json!({"success":true,"message":"No config file to reset"}))
}
