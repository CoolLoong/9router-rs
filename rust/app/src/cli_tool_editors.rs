use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Extension, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use provider_capabilities_rs::port::model_capabilities;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::chat_output::js_text;
use crate::cli::Tool;
use crate::cli_connect::read_settings;
use crate::cli_connect::tools::{
    configure_cline_state, configure_opencode_explorer, configure_opencode_models,
    reset_cline_state, reset_opencode,
};
use crate::{openai_models, provider_connections::truthy, thinking_catalog};

use super::{
    CliTools, PendingFile, executable, in_config, persist, persist_batch, reply, resolve_key,
    serialize_json,
};

#[derive(Clone, Copy)]
enum Editor {
    OpenCode,
    Cline,
}

impl Editor {
    fn files(self) -> Result<Vec<PathBuf>, String> {
        let home = std::env::home_dir().ok_or("Unable to locate the home directory")?;
        let tool = match self {
            Self::OpenCode => Tool::OpenCode,
            Self::Cline => Tool::Cline,
        };
        Ok(tool.files(&home, None))
    }

    fn id(self) -> &'static str {
        match self {
            Self::OpenCode => "opencode",
            Self::Cline => "cline",
        }
    }
}

pub(super) fn add_routes(router: Router<CliTools>) -> Router<CliTools> {
    router
        .route(
            "/api/cli-tools/opencode-settings",
            get(status)
                .post(apply)
                .patch(patch_opencode)
                .delete(restore)
                .layer(Extension(Editor::OpenCode)),
        )
        .route(
            "/api/cli-tools/cline-settings",
            get(status)
                .post(apply)
                .delete(restore)
                .layer(Extension(Editor::Cline)),
        )
}

pub(super) async fn tool_status(id: &str) -> Option<Value> {
    let editor = match id {
        "opencode" => Editor::OpenCode,
        "cline" => Editor::Cline,
        _ => return None,
    };
    tokio::task::spawn_blocking(move || config_status(editor))
        .await
        .ok()?
        .ok()
}

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({"error":message}))
}

fn result(value: Result<Value, String>, message: &str) -> Response {
    match value {
        Ok(value) => reply(StatusCode::OK, value),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}

async fn body(request: Request) -> Option<Value> {
    let bytes = to_bytes(request.into_body(), 128 * 1024 * 1024)
        .await
        .ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(|value| !value.is_null())
}

async fn status(State(state): State<CliTools>, Extension(editor): Extension<Editor>) -> Response {
    let value = in_config(state.editing, move || config_status(editor)).await;
    let message = match editor {
        Editor::OpenCode => "Failed to check opencode settings",
        Editor::Cline => "Failed to check cline settings",
    };
    result(value, message)
}

fn config_status(editor: Editor) -> Result<Value, String> {
    let files = editor.files()?;
    let path = &files[0];
    let command = editor.id();
    let installed = path.exists()
        || executable(command)
        || cfg!(windows)
            && std::env::var_os("APPDATA").is_some_and(|dir| {
                Path::new(&dir)
                    .join("npm")
                    .join(command)
                    .to_str()
                    .is_some_and(executable)
            });
    let (_, config) = read_settings(path).unwrap_or_default().unwrap_or_default();
    match editor {
        Editor::OpenCode if !installed => Ok(json!({
            "installed":false,"config":null,"message":"OpenCode CLI is not installed",
        })),
        Editor::Cline if !installed => Ok(json!({
            "installed":false,"settings":null,"message":"Cline CLI is not installed",
        })),
        Editor::OpenCode => {
            let provider = &config["provider"]["9router"];
            let models = provider["models"]
                .as_object()
                .into_iter()
                .flat_map(Map::keys)
                .collect::<Vec<_>>();
            Ok(json!({
                "installed":true,"config":config,"has9Router":truthy(provider),"configPath":path,
                "opencode":{"models":models,"activeModel":managed_model(&config["model"])?,
                    "explorerModel":config["agent"]["explorer"]["model"].as_str()
                        .and_then(|model| model.strip_prefix("9router/")),
                    "baseURL":provider["options"].get("baseURL").filter(|value|truthy(value))},
            }))
        }
        Editor::Cline => {
            let settings: Map<_, _> = [
                "actModeApiProvider",
                "planModeApiProvider",
                "openAiBaseUrl",
                "openAiModelId",
            ]
            .into_iter()
            .filter_map(|name| Some((name.to_owned(), config.get(name)?.clone())))
            .collect();
            let openai = config["actModeApiProvider"] == "openai"
                || config["planModeApiProvider"] == "openai";
            let configured = openai
                && config["openAiBaseUrl"].as_str().is_some_and(|base| {
                    ["localhost", "127.0.0.1", "9router"]
                        .iter()
                        .any(|marker| base.contains(marker))
                });
            Ok(
                json!({"installed":true,"settings":settings,"has9Router":configured,"globalStatePath":path}),
            )
        }
    }
}

async fn apply(
    State(state): State<CliTools>,
    Extension(editor): Extension<Editor>,
    request: Request,
) -> Response {
    let failure = match editor {
        Editor::OpenCode => "Failed to apply settings",
        Editor::Cline => "Failed to update cline settings",
    };
    let Some(input) = body(request).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, failure);
    };
    match editor {
        Editor::OpenCode => {
            if !truthy(&input["baseUrl"]) || requested_models(&input).is_empty() {
                return error(
                    StatusCode::BAD_REQUEST,
                    "baseUrl and at least one model are required",
                );
            }
            if truthy(&input["apiKey"]) && !input["apiKey"].is_string() {
                return error(StatusCode::INTERNAL_SERVER_ERROR, failure);
            }
            let key = resolve_key(&state.storage, input["apiKey"].as_str()).await;
            let catalog = state.storage.model_catalog();
            result(
                in_config(state.editing, move || {
                    apply_opencode(&editor.files()?[0], &input, &key, catalog.as_deref())
                })
                .await,
                failure,
            )
        }
        Editor::Cline => {
            if ["baseUrl", "apiKey", "model"]
                .iter()
                .any(|field| !truthy(&input[*field]))
            {
                return error(
                    StatusCode::BAD_REQUEST,
                    "baseUrl, apiKey and model are required",
                );
            }
            result(
                in_config(state.editing, move || {
                    update_cline(editor.files()?, Some(input))
                })
                .await,
                failure,
            )
        }
    }
}

fn requested_models(input: &Value) -> &[Value] {
    input["models"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| {
            if input["model"].is_string() {
                std::slice::from_ref(&input["model"])
            } else {
                &[]
            }
        })
}

type Config = Map<String, Value>;

fn config_file(path: &Path) -> Result<Option<(Vec<u8>, Config)>, String> {
    read_settings(path)?
        .map(|(raw, value)| Ok((raw, config_map(value)?)))
        .transpose()
}

fn config_map(value: Value) -> Result<Config, String> {
    match value {
        Value::Object(config) => Ok(config),
        value if !truthy(&value) => Ok(Map::new()),
        _ => Err("CLI settings must be a JSON object".into()),
    }
}

fn managed_model(value: &Value) -> Result<Option<&str>, String> {
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(|id| id.strip_prefix("9router/"))
        .ok_or_else(|| "Invalid OpenCode model".into())
}

fn apply_opencode(
    path: &Path,
    input: &Value,
    key: &str,
    catalog: Option<&Value>,
) -> Result<Value, String> {
    let base = input["baseUrl"]
        .as_str()
        .ok_or("Invalid OpenCode base URL")?;
    let base = if base.ends_with("/v1") {
        base.to_owned()
    } else {
        format!("{base}/v1")
    };
    let mut config = config_file(path)?
        .map(|(_, config)| config)
        .unwrap_or_default();
    let requested = requested_models(input);
    let models = requested
        .iter()
        .filter_map(Value::as_str)
        .filter(|id| !id.is_empty());
    configure_opencode_models(&mut config, &base, key, models, |model, previous| {
        previous
            .filter(|variants| truthy(variants))
            .cloned()
            .or_else(|| effort_variants(model, catalog))
    })?;
    let active = &input["activeModel"];
    if active == "" {
        config.insert("model".into(), json!(""));
    } else {
        let model = if truthy(active) {
            active
        } else {
            &requested[0]
        };
        if truthy(model) {
            config.insert("model".into(), json!(format!("9router/{}", js_text(model))));
        }
    }
    let explorer = &input["subagentModel"];
    let explorer = if truthy(explorer) {
        explorer
    } else {
        &requested[0]
    };
    configure_opencode_explorer(&mut config, &format!("9router/{}", js_text(explorer)))?;
    persist(path, Some(serialize_json(config)?.as_bytes()))?;
    Ok(
        json!({"success":true,"message":"OpenCode settings applied successfully!","configPath":path}),
    )
}

fn effort_variants(full: &str, catalog: Option<&Value>) -> Option<Value> {
    let (alias, model) = full.split_once('/')?;
    if alias.is_empty() {
        return None;
    }
    let data = openai_models::data();
    let provider = data["providerByAlias"][alias].as_str().unwrap_or(alias);
    if provider == "kiro" && !kiro_supports_effort(model) {
        return None;
    }
    let capability = model_capabilities(Some(provider), model, catalog);
    let levels = thinking_catalog::levels(provider, model, &capability, data)?;
    let variants: Map<_, _> = levels
        .into_iter()
        .map(|level| {
            let effort = json!({"reasoningEffort":level});
            (level, effort)
        })
        .collect();
    Some(Value::Object(variants))
}

fn kiro_supports_effort(model: &str) -> bool {
    static GPT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|[/.])gpt[/.]5[/.]6(?:[/.]|$)").unwrap());
    static CLAUDE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:^|[/.])claude(?:[/.][a-z]+)*[/.]([0-9]+)(?:[/.]([0-9]+))?(?:[/.]|$)")
            .unwrap()
    });
    let model = model.to_ascii_lowercase().replace('-', ".");
    GPT.is_match(&model)
        || CLAUDE.captures(&model).is_some_and(|version| {
            let major = version
                .get(1)
                .and_then(|value| value.as_str().parse::<u32>().ok());
            let minor = version
                .get(2)
                .and_then(|value| value.as_str().parse::<u32>().ok());
            major.is_some_and(|major| {
                major > 4 || major == 4 && minor.is_some_and(|minor| (6..1000).contains(&minor))
            })
        })
}

async fn patch_opencode(State(state): State<CliTools>, request: Request) -> Response {
    let failure = "Failed to patch settings";
    let Some(body) = body(request).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, failure);
    };
    result(
        in_config(state.editing, move || {
            let path = &Editor::OpenCode.files()?[0];
            let Some((_, mut config)) = config_file(path)? else {
                return Ok(json!({"success":true,"message":"No config file found"}));
            };
            if body["clearActiveModel"] == true
                && managed_model(config.get("model").unwrap_or(&Value::Null))?.is_some()
            {
                config.insert("model".into(), json!(""));
            }
            persist(path, Some(serialize_json(config)?.as_bytes()))?;
            Ok(json!({"success":true,"message":"Settings updated"}))
        })
        .await,
        failure,
    )
}

async fn restore(
    State(state): State<CliTools>,
    Extension(editor): Extension<Editor>,
    request: Request,
) -> Response {
    let model = request.uri().query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "model")
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty())
    });
    let value = in_config(state.editing, move || {
        let files = editor.files()?;
        match editor {
            Editor::OpenCode => {
                let Some((_, mut config)) = config_file(&files[0])? else {
                    return Ok(json!({"success":true,"message":"No config file to reset"}));
                };
                reset_opencode(&mut config, model.as_deref());
                persist(&files[0], Some(serialize_json(config)?.as_bytes()))?;
                let message = model.map_or_else(
                    || "9Router settings removed from OpenCode".to_owned(),
                    |model| format!("Model \"{model}\" removed"),
                );
                Ok(json!({"success":true,"message":message}))
            }
            Editor::Cline => update_cline(files, None),
        }
    })
    .await;
    let failure = match editor {
        Editor::OpenCode => "Failed to reset opencode settings",
        Editor::Cline => "Failed to reset cline settings",
    };
    result(value, failure)
}

fn update_cline(files: Vec<PathBuf>, input: Option<Value>) -> Result<Value, String> {
    let [state_path, secrets_path] = files
        .try_into()
        .map_err(|_| "Missing Cline settings paths")?;
    let state = read_settings(&state_path);
    let (state_raw, mut state) = match state {
        Ok(Some((raw, config))) if input.is_some() || truthy(&config) => {
            (Some(raw), config_map(config)?)
        }
        _ if input.is_none() => {
            return Ok(json!({"success":true,"message":"No settings file to reset"}));
        }
        Ok(_) => (None, Map::new()),
        Err(error) => return Err(error),
    };
    let (secrets_raw, mut secrets) = config_file(&secrets_path)?
        .map_or((None, Map::new()), |(raw, secrets)| (Some(raw), secrets));
    let response = if let Some(input) = input {
        let url = input["baseUrl"].as_str().ok_or("Invalid Cline base URL")?;
        configure_cline_state(
            &mut state,
            url.strip_suffix("/v1").unwrap_or(url),
            &input["model"],
        );
        secrets.insert("openAiApiKey".into(), input["apiKey"].clone());
        json!({"success":true,"message":"Cline settings applied successfully!","globalStatePath":state_path})
    } else {
        reset_cline_state(&mut state);
        secrets.shift_remove("openAiApiKey");
        json!({"success":true,"message":"9Router settings removed from Cline"})
    };
    persist_batch(&[
        PendingFile::new(state_path, state_raw, state)?,
        PendingFile::new(secrets_path, secrets_raw, secrets)?,
    ])?;
    Ok(response)
}
