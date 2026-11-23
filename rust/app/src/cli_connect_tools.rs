use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use serde_yaml::{Mapping as YamlMap, Value as Yaml};
use toml::Value as Toml;

use crate::cli::Tool;

use super::{
    MODELS, apply_claude, read_config, read_settings, reset_claude, truthy, write_config,
    write_private,
};

impl Tool {
    pub(super) fn needs_model(self) -> bool {
        !matches!(self, Self::Claude | Self::Omp)
    }

    pub(super) fn route(self) -> Option<&'static str> {
        match self {
            Self::Codex => Some("codex-settings"),
            Self::OpenCode => Some("opencode-settings"),
            Self::Droid => Some("droid-settings"),
            Self::Crush => Some("crush-settings"),
            Self::Cline => Some("cline-settings"),
            Self::Pi => Some("pi-settings"),
            Self::Claude | Self::Kilo | Self::Omp => None,
        }
    }

    pub(super) fn server_model(self, data: &Value) -> Option<String> {
        let model = match self {
            Self::Codex => {
                let config: toml::Table = toml::from_str(data["config"].as_str()?).ok()?;
                return (config.get("model_provider")?.as_str()? == "9router")
                    .then(|| {
                        config
                            .get("model")?
                            .as_str()
                            .filter(|model| !model.is_empty())
                            .map(str::to_owned)
                    })
                    .flatten();
            }
            Self::OpenCode => data["config"]["model"].as_str()?.strip_prefix("9router/"),
            Self::Droid => data["settings"]["customModels"]
                .as_array()?
                .iter()
                .find(|model| is_droid_9router(model))?
                .get("model")?
                .as_str(),
            Self::Crush => data["config"]["providers"]["9router"]["models"][0]["id"].as_str(),
            Self::Cline if data["settings"]["actModeApiProvider"] == "openai" => {
                data["settings"]["openAiModelId"].as_str()
            }
            Self::Pi => {
                let model = &data["config"]["providers"]["9router"]["models"][0];
                model.as_str().or_else(|| model["id"].as_str())
            }
            _ => None,
        };
        model.filter(|model| !model.is_empty()).map(str::to_owned)
    }

    pub(super) fn apply(
        self,
        paths: &Paths<'_>,
        origin: &str,
        base: &str,
        key: &str,
        model: Option<&str>,
        claude_models: [Option<&str>; MODELS.len()],
    ) -> Result<Vec<PathBuf>, String> {
        let files = self.files(paths.home, paths.config_home);
        let file = &files[0];
        match self {
            Self::Claude => apply_claude(file, base, key, claude_models)?,
            Self::Omp => apply_omp(file, base, key)?,
            _ => {
                let model = model.ok_or("No model configured")?;
                match self {
                    Self::Codex => apply_codex(file, base, key, model)?,
                    Self::OpenCode => {
                        update_json(file, |config| apply_opencode(config, base, key, model))?
                    }
                    Self::Droid => {
                        update_json(file, |config| apply_droid(config, base, key, model))?
                    }
                    Self::Crush => {
                        update_json(file, |config| {
                            object_field(config, "providers")?.insert("9router".into(), json!({
                            "type": "openai-compat", "base_url": base, "api_key": key,
                            "models": [{"id": model, "name": model, "context_window": 128000}],
                        }));
                            Ok(())
                        })?
                    }
                    Self::Kilo => update_json(file, |config| {
                        configure_kilo(config, base, &json!(key), &json!(model));
                        Ok(())
                    })?,
                    Self::Cline => return apply_cline(files, origin, key, model),
                    Self::Pi => update_json(file, |config| {
                        let providers = object_field(config, "providers")?;
                        let provider = object_field(providers, "9router")?;
                        provider.insert("baseUrl".into(), json!(base));
                        provider.insert("apiKey".into(), json!(key));
                        provider.insert("api".into(), json!("openai-completions"));
                        provider.insert("models".into(), json!([{
                            "id": model, "name": model, "contextWindow": 128000, "maxTokens": 16384,
                        }]));
                        Ok(())
                    })?,
                    Self::Claude | Self::Omp => unreachable!(),
                }
            }
        }
        Ok(files)
    }

    pub(super) fn reset(self, paths: &Paths<'_>) -> Result<Vec<PathBuf>, String> {
        let files = self.files(paths.home, paths.config_home);
        let file = &files[0];
        let written = match self {
            Self::Claude => reset_claude(file)?,
            Self::Codex => reset_codex(file)?,
            Self::OpenCode => reset_json(file, |config| {
                reset_opencode(config, None);
                Ok(true)
            })?,
            Self::Droid => reset_json(file, reset_droid)?,
            Self::Crush | Self::Pi => reset_json(file, |config| {
                let Some(providers) = config.get_mut("providers").and_then(Value::as_object_mut)
                else {
                    return Ok(false);
                };
                if !providers.get("9router").is_some_and(truthy) {
                    return Ok(false);
                }
                providers.shift_remove("9router");
                if providers.is_empty() {
                    config.shift_remove("providers");
                }
                Ok(true)
            })?,
            Self::Kilo => reset_json(file, |config| {
                reset_kilo(config);
                Ok(true)
            })?,
            Self::Cline => return reset_cline(files),
            Self::Omp => reset_omp(file)?,
        };
        Ok(if written { files } else { Vec::new() })
    }
}

pub(crate) fn configure_kilo(
    config: &mut Map<String, Value>,
    base: &str,
    key: &Value,
    model: &Value,
) {
    config.insert(
        "openai-compatible".into(),
        json!({
            "type":"api-key","apiKey":key,"baseUrl":base,"model":model,
        }),
    );
}

pub(crate) fn reset_kilo(config: &mut Map<String, Value>) {
    config.shift_remove("openai-compatible");
    config.shift_remove("9router");
}

pub(super) struct Paths<'a> {
    pub(super) home: &'a Path,
    pub(super) config_home: Option<&'a Path>,
}

type JsonObject = Map<String, Value>;

fn object_field<'a>(object: &'a mut JsonObject, key: &str) -> Result<&'a mut JsonObject, String> {
    let value = object.entry(key).or_insert_with(|| json!({}));
    if !truthy(value) {
        *value = json!({});
    }
    value
        .as_object_mut()
        .ok_or_else(|| format!("{key} must be a JSON object"))
}

fn update_json(
    file: &Path,
    update: impl FnOnce(&mut JsonObject) -> Result<(), String>,
) -> Result<(), String> {
    let (raw, existing) =
        read_settings(file)?.map_or((None, Value::Null), |(raw, value)| (Some(raw), value));
    let mut value = if truthy(&existing) {
        existing
    } else {
        json!({})
    };
    update(
        value
            .as_object_mut()
            .ok_or("Settings must be a JSON object")?,
    )?;
    let content = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
    write_config(file, &content, raw.as_deref())
}

fn reset_json(
    file: &Path,
    reset: impl FnOnce(&mut JsonObject) -> Result<bool, String>,
) -> Result<bool, String> {
    let Some((_, mut value)) = read_settings(file)? else {
        return Ok(false);
    };
    if !truthy(&value) {
        return Ok(false);
    }
    if !reset(
        value
            .as_object_mut()
            .ok_or("Settings must be a JSON object")?,
    )? {
        return Ok(false);
    }
    let content = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
    write_private(file, &content, true).map_err(|error| error.to_string())?;
    Ok(true)
}

fn read_toml(file: &Path) -> Result<Option<(Vec<u8>, toml::Table)>, String> {
    let Some(raw) = read_config(file)? else {
        return Ok(None);
    };
    let config = toml::from_str(&String::from_utf8_lossy(&raw))
        .map_err(|_| format!("Cannot parse {}: invalid TOML", file.display()))?;
    Ok(Some((raw, config)))
}

fn table_field<'a>(table: &'a mut toml::Table, key: &str) -> Result<&'a mut toml::Table, String> {
    let value = table
        .entry(key.to_owned())
        .or_insert_with(|| Toml::Table(toml::Table::new()));
    value
        .as_table_mut()
        .ok_or_else(|| format!("{key} must be a TOML table"))
}

fn apply_codex(file: &Path, base: &str, key: &str, model: &str) -> Result<(), String> {
    let (raw, mut config) = read_toml(file)?.map_or_else(
        || (None, toml::Table::new()),
        |(raw, config)| (Some(raw), config),
    );
    config.insert("model".into(), Toml::String(model.into()));
    config.insert("model_provider".into(), Toml::String("9router".into()));
    let headers = toml::Table::from_iter([(
        "Authorization".into(),
        Toml::String(format!("Bearer {key}")),
    )]);
    let provider = toml::Table::from_iter([
        ("name".into(), Toml::String("9Router".into())),
        ("base_url".into(), Toml::String(base.into())),
        ("wire_api".into(), Toml::String("responses".into())),
        ("http_headers".into(), Toml::Table(headers)),
    ]);
    table_field(&mut config, "model_providers")?.insert("9router".into(), Toml::Table(provider));
    let agents = table_field(&mut config, "agents")?;
    agents.remove("subagent");
    agents.insert("default_subagent_model".into(), Toml::String(model.into()));
    let content = toml::to_string(&config).map_err(|error| error.to_string())?;
    write_config(file, content.as_bytes(), raw.as_deref())
}

fn reset_codex(file: &Path) -> Result<bool, String> {
    let Some((_, mut config)) = read_toml(file)? else {
        return Ok(false);
    };
    if config.get("model_provider").and_then(Toml::as_str) == Some("9router") {
        config.remove("model_provider");
        config.remove("model");
    }
    for (key, fields) in [
        ("model_providers", &["9router"][..]),
        ("agents", &["default_subagent_model", "subagent"][..]),
    ] {
        if let Some(table) = config.get_mut(key).and_then(Toml::as_table_mut) {
            for field in fields {
                table.remove(*field);
            }
            if table.is_empty() {
                config.remove(key);
            }
        }
    }
    let content = toml::to_string(&config).map_err(|error| error.to_string())?;
    write_private(file, content.as_bytes(), true).map_err(|error| error.to_string())?;
    Ok(true)
}

fn apply_opencode(
    config: &mut JsonObject,
    base: &str,
    key: &str,
    model: &str,
) -> Result<(), String> {
    configure_opencode_models(config, base, key, [model], |_, _| None)?;
    let qualified = format!("9router/{model}");
    config.insert("model".into(), json!(qualified));
    configure_opencode_explorer(config, &qualified)
}

pub(crate) fn configure_opencode_models<'a>(
    config: &mut JsonObject,
    base: &str,
    key: &str,
    requested: impl IntoIterator<Item = &'a str>,
    variants: impl Fn(&str, Option<&Value>) -> Option<Value>,
) -> Result<(), String> {
    let providers = object_field(config, "provider")?;
    if !providers.get("9router").is_some_and(truthy) {
        providers.insert(
            "9router".into(),
            json!({"npm": "@ai-sdk/openai-compatible", "options": {}, "models": {}}),
        );
    }
    let provider = object_field(providers, "9router")?;
    let options = object_field(provider, "options")?;
    options.insert("baseURL".into(), json!(base));
    options.insert("apiKey".into(), json!(key));
    let models = object_field(provider, "models")?;
    for model in requested {
        let mut entry = json!({
            "name": model, "modalities": {"input": ["text", "image"], "output": ["text"]},
        });
        if let Some(variants) = variants(
            model,
            models.get(model).and_then(|entry| entry.get("variants")),
        ) {
            entry["variants"] = variants;
        }
        models.insert(model.into(), entry);
    }
    Ok(())
}

pub(crate) fn configure_opencode_explorer(
    config: &mut JsonObject,
    model: &str,
) -> Result<(), String> {
    object_field(config, "agent")?.insert(
        "explorer".into(),
        json!({
            "description": "Fast explorer subagent for codebase exploration",
            "mode": "subagent",
            "model": model,
        }),
    );
    Ok(())
}

pub(crate) fn reset_opencode(config: &mut JsonObject, removing: Option<&str>) {
    let remaining = removing.and_then(|model| {
        let models = config
            .get_mut("provider")?
            .get_mut("9router")?
            .get_mut("models")?
            .as_object_mut()?;
        models.shift_remove(model);
        models.keys().next().map(|id| format!("9router/{id}"))
    });
    let active = config
        .get("model")
        .and_then(Value::as_str)
        .and_then(|id| id.strip_prefix("9router/"));
    match remaining {
        Some(next) if active.is_some() && active == removing => {
            config.insert("model".into(), json!(next));
        }
        Some(_) => (),
        None => {
            if active.is_some() {
                config.shift_remove("model");
            }
            if let Some(provider) = config.get_mut("provider").and_then(Value::as_object_mut) {
                provider.shift_remove("9router");
            }
        }
    }
    if let Some(agents) = config.get_mut("agent").and_then(Value::as_object_mut)
        && agents
            .get("explorer")
            .and_then(|agent| agent["model"].as_str())
            .is_some_and(|model| model.starts_with("9router/"))
    {
        agents.shift_remove("explorer");
        if agents.is_empty() {
            config.shift_remove("agent");
        }
    }
}

fn is_droid_9router(model: &Value) -> bool {
    model["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("custom:9Router"))
}

fn droid_models(config: &mut JsonObject) -> Result<&mut Vec<Value>, String> {
    let models = config.entry("customModels").or_insert_with(|| json!([]));
    if !truthy(models) {
        *models = json!([]);
    }
    models
        .as_array_mut()
        .ok_or_else(|| "customModels must be a JSON array".into())
}

fn apply_droid(config: &mut JsonObject, base: &str, key: &str, model: &str) -> Result<(), String> {
    let models = droid_models(config)?;
    models.retain(|entry| !is_droid_9router(entry));
    models.insert(
        0,
        json!({
            "model": model, "id": "custom:9Router-0", "index": 0, "baseUrl": base,
            "apiKey": key, "displayName": model, "maxOutputTokens": 131072,
            "noImageSupport": false, "provider": "openai",
        }),
    );
    for (index, model) in models.iter_mut().enumerate() {
        let model = model
            .as_object_mut()
            .ok_or("Droid models must be JSON objects")?;
        model.insert("index".into(), json!(index));
    }
    Ok(())
}

fn reset_droid(config: &mut JsonObject) -> Result<bool, String> {
    if config.get("customModels").is_some_and(truthy) {
        let models = droid_models(config)?;
        models.retain(|entry| !is_droid_9router(entry));
        if models.is_empty() {
            config.shift_remove("customModels");
        }
    }
    Ok(true)
}

fn apply_cline(
    files: Vec<PathBuf>,
    origin: &str,
    key: &str,
    model: &str,
) -> Result<Vec<PathBuf>, String> {
    update_json(&files[0], |state| {
        configure_cline_state(state, origin, &json!(model));
        Ok(())
    })?;
    update_json(&files[1], |secrets| {
        secrets.insert("openAiApiKey".into(), json!(key));
        Ok(())
    })?;
    Ok(files)
}

pub(crate) fn configure_cline_state(state: &mut JsonObject, origin: &str, model: &Value) {
    state.insert("actModeApiProvider".into(), json!("openai"));
    state.insert("planModeApiProvider".into(), json!("openai"));
    state.insert("openAiBaseUrl".into(), json!(origin));
    state.insert("openAiModelId".into(), model.clone());
    state.insert("planModeOpenAiModelId".into(), model.clone());
}

fn reset_cline(mut files: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
    if !reset_json(&files[0], |state| {
        reset_cline_state(state);
        Ok(true)
    })? {
        return Ok(Vec::new());
    }
    if !reset_json(&files[1], |secrets| {
        secrets.shift_remove("openAiApiKey");
        Ok(true)
    })? {
        files.pop();
    }
    Ok(files)
}

pub(crate) fn reset_cline_state(state: &mut JsonObject) {
    if state.get("actModeApiProvider").and_then(Value::as_str) == Some("openai") {
        for key in ["openAiBaseUrl", "openAiModelId", "planModeOpenAiModelId"] {
            state.shift_remove(key);
        }
        state.insert("actModeApiProvider".into(), json!("cline"));
        state.insert("planModeApiProvider".into(), json!("cline"));
    }
}

fn read_yaml(file: &Path) -> Result<Option<(Vec<u8>, YamlMap)>, String> {
    let Some(raw) = read_config(file)? else {
        return Ok(None);
    };
    let config = serde_yaml::from_str(&String::from_utf8_lossy(&raw))
        .map_err(|_| format!("Cannot parse {}: invalid YAML", file.display()))?;
    match config {
        Yaml::Null | Yaml::Bool(false) => Ok(Some((raw, YamlMap::new()))),
        Yaml::Mapping(config) => Ok(Some((raw, config))),
        _ => Err("Settings must be a YAML mapping".into()),
    }
}

fn apply_omp(file: &Path, base: &str, key: &str) -> Result<(), String> {
    let (raw, mut config) = read_yaml(file)?.map_or_else(
        || (None, YamlMap::new()),
        |(raw, config)| (Some(raw), config),
    );
    let providers = config
        .entry("providers".into())
        .or_insert_with(|| Yaml::Mapping(YamlMap::new()));
    if providers.is_null() || *providers == Yaml::Bool(false) {
        *providers = Yaml::Mapping(YamlMap::new());
    }
    let providers = providers
        .as_mapping_mut()
        .ok_or("providers must be a YAML mapping")?;
    let provider = serde_yaml::to_value(json!({
        "baseUrl": base, "apiKey": key, "api": "openai-completions", "authHeader": true,
        "disableStrictTools": true, "discovery": {"type": "proxy"},
    }))
    .map_err(|error| error.to_string())?;
    providers.insert("9router".into(), provider);
    let content = serde_yaml::to_string(&config).map_err(|error| error.to_string())?;
    write_config(file, content.as_bytes(), raw.as_deref())
}

fn reset_omp(file: &Path) -> Result<bool, String> {
    let Some((_, mut config)) = read_yaml(file)? else {
        return Ok(false);
    };
    let Some(providers) = config.get_mut("providers").and_then(Yaml::as_mapping_mut) else {
        return Ok(false);
    };
    if providers
        .get("9router")
        .is_none_or(|provider| provider.is_null() || *provider == Yaml::Bool(false))
    {
        return Ok(false);
    }
    providers.remove("9router");
    if providers.is_empty() {
        config.remove("providers");
    }
    let content = if config.is_empty() {
        String::new()
    } else {
        serde_yaml::to_string(&config).map_err(|error| error.to_string())?
    };
    write_private(file, content.as_bytes(), true).map_err(|error| error.to_string())?;
    Ok(true)
}
