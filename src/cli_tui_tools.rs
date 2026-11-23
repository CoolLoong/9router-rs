use std::collections::HashSet;
use std::io;

use reqwest::Method;
use serde_json::{Map, Value, json};

use super::{Api, confirm, get_text, labels, menu, notice, prompt, rows};

const TOOLS: [(&str, &str); 6] = [
    ("claude", "Claude Code"),
    ("codex", "Codex CLI"),
    ("droid", "Factory Droid"),
    ("openclaw", "Open Claw"),
    ("opencode", "OpenCode"),
    ("hermes", "Hermes"),
];

const CLAUDE_MODELS: [(&str, &str, &str); 3] = [
    (
        "Sonnet",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "cc/claude-sonnet-4-5-20250929",
    ),
    (
        "Opus",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "cc/claude-opus-4-5-20251101",
    ),
    (
        "Haiku",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "cc/claude-haiku-4-5-20251001",
    ),
];

pub(super) async fn run(api: &Api) -> io::Result<()> {
    let items = std::iter::once("← Back")
        .chain(TOOLS.map(|(_, name)| name))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut profile = String::from("default");
    loop {
        match menu("🛠️ CLI Tools", "", &items).await? {
            Some(index) if index > 0 => {
                let (tool, name) = TOOLS[index - 1];
                tool_menu(api, tool, name, &mut profile).await?;
            }
            _ => return Ok(()),
        }
    }
}

async fn tool_menu(api: &Api, tool: &str, name: &str, profile: &mut String) -> io::Result<()> {
    let path = format!("/api/cli-tools/{tool}-settings");
    loop {
        let profiles = if tool == "hermes" {
            api.get("/api/cli-tools/hermes-profiles")
                .await
                .unwrap_or_default()
        } else {
            Value::Null
        };
        let profiles = rows(&profiles, "profiles");
        if !profiles.is_empty()
            && !profiles
                .iter()
                .any(|entry| entry["name"].as_str() == Some(profile.as_str()))
        {
            *profile = "default".into();
        }
        let current = if tool == "hermes" {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("profile", profile)
                .finish();
            format!("{path}?{query}")
        } else {
            path.clone()
        };
        let settings = api.get(&current).await;
        let header = settings.as_ref().map_or_else(
            |_| "Failed to load settings".into(),
            |data| tool_header(tool, data, profiles),
        );
        let data = settings.unwrap_or_default();
        let (items, reset_index) = match tool {
            "claude" => {
                let mut items = labels(&["← Back", "⚡ Quick Setup (recommended)"]);
                items.extend(CLAUDE_MODELS.map(|(name, env, default)| {
                    format!(
                        "{name} → {}",
                        data["settings"]["env"][env].as_str().unwrap_or(default)
                    )
                }));
                items.push("Reset to Default".into());
                (items, 5)
            }
            "hermes" => (
                vec![
                    "← Back".into(),
                    format!("🎯 Profile: {profile}"),
                    "⚡ Quick Setup".into(),
                    "🔁 Apply to All Profiles".into(),
                    format!("↺ Reset Profile: {profile}"),
                ],
                4,
            ),
            _ => (labels(&["← Back", "⚡ Quick Setup", "Reset to Default"]), 2),
        };
        match menu(&format!("{name} Settings"), &header, &items).await? {
            Some(index) if index == reset_index => {
                notice(
                    api.request(Method::DELETE, &current, None).await,
                    "Settings reset successfully!",
                )
                .await?;
            }
            Some(1) if tool == "hermes" => {
                let names: Vec<_> = profiles.iter().map(|row| get_text(row, "name")).collect();
                if names.is_empty() {
                    notice(Err("No Hermes profiles found.".into()), "").await?;
                } else if let Some(index) = menu("Select Hermes profile", "", &names).await? {
                    *profile = names[index].clone();
                }
            }
            Some(index @ 2..=4) if tool == "claude" => {
                let (tier, env, _) = CLAUDE_MODELS[index - 2];
                if let Some(model) =
                    choose_model(api, &format!("Select {tier} Model"), false).await?
                {
                    let mut payload = Map::new();
                    payload.insert(env.into(), json!(model));
                    if data["settings"]["env"]["ANTHROPIC_BASE_URL"]
                        .as_str()
                        .is_none_or(str::is_empty)
                    {
                        let (endpoint, key) = connection_info(api).await;
                        payload.insert("ANTHROPIC_BASE_URL".into(), json!(endpoint));
                        payload.insert("API_TIMEOUT_MS".into(), json!("600000"));
                        if let Some(key) = key {
                            payload.insert("ANTHROPIC_AUTH_TOKEN".into(), json!(key));
                        }
                    }
                    notice(
                        api.request(Method::POST, &path, Some(json!({"env":payload})))
                            .await,
                        &format!("{tier} → {model} saved!"),
                    )
                    .await?;
                }
            }
            Some(index) if index > 0 => {
                setup(
                    api,
                    tool,
                    name,
                    profile,
                    &path,
                    tool == "hermes" && index == 3,
                )
                .await?
            }
            _ => return Ok(()),
        }
    }
}

fn tool_header(tool: &str, data: &Value, profiles: &[Value]) -> String {
    if data["installed"] == false {
        return "Status: Not installed".into();
    }
    let configured = data["has9Router"] == true;
    let status = if configured {
        "Status: Configured"
    } else {
        "Status: Unconfigured"
    };
    if !configured && tool != "hermes" {
        return status.into();
    }
    let mut lines = vec![status.to_owned()];
    let settings = &data["settings"];
    let (endpoint, model, additional) = match tool {
        "claude" => (
            &settings["env"]["ANTHROPIC_BASE_URL"],
            &Value::Null,
            &Value::Null,
        ),
        "codex" => {
            let router = &data["routerConfig"];
            (
                &router["baseUrl"],
                &router["model"],
                &router["subagentModel"],
            )
        }
        "droid" => {
            let model = rows(settings, "customModels")
                .iter()
                .find(|entry| entry["id"] == "custom:9Router-0")
                .unwrap_or(&Value::Null);
            (&model["baseUrl"], &model["model"], &Value::Null)
        }
        "openclaw" => {
            let provider = &settings["models"]["providers"]["9router"];
            let primary = &settings["agents"]["defaults"]["model"];
            let model = primary
                .as_str()
                .map(|_| primary)
                .unwrap_or(&primary["primary"]);
            let model = if model.as_str().is_some_and(|id| id.starts_with("9router/")) {
                model
            } else {
                &provider["models"][0]["id"]
            };
            (&provider["baseUrl"], model, &Value::Null)
        }
        "opencode" => {
            let provider = &data["opencode"];
            (
                &provider["baseURL"],
                &provider["activeModel"],
                &provider["explorerModel"],
            )
        }
        "hermes" => (
            &settings["model"]["base_url"],
            &settings["model"]["default"],
            &Value::Null,
        ),
        _ => (&Value::Null, &Value::Null, &Value::Null),
    };
    if let Some(mut url) = endpoint
        .as_str()
        .and_then(|endpoint| url::Url::parse(endpoint).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https"))
    {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
        lines.push(format!("Endpoint: {}", super::printable(url.as_str())));
    }
    if let Some(model) = model.as_str().filter(|model| !model.is_empty()) {
        let model = if tool == "openclaw" {
            model.strip_prefix("9router/").unwrap_or(model)
        } else {
            model
        };
        lines.push(format!("Model: {}", super::printable(model)));
    }
    if let Some(model) = additional.as_str().filter(|model| !model.is_empty()) {
        let name = if tool == "opencode" {
            "Explorer"
        } else {
            "Subagent"
        };
        lines.push(format!("{name}: {}", super::printable(model)));
    }
    if tool == "opencode" {
        let ids = rows(&data["opencode"], "models")
            .iter()
            .filter_map(Value::as_str)
            .map(super::printable)
            .collect::<Vec<_>>();
        if !ids.is_empty() {
            lines.push(format!("Models: {}", ids.join(", ")));
        }
    }
    if tool == "hermes" && profiles.len() > 1 {
        let names = profiles
            .iter()
            .map(|profile| {
                format!(
                    "{} {}",
                    get_text(profile, "name"),
                    if profile["has9Router"] == true {
                        "✓"
                    } else {
                        "✗"
                    }
                )
            })
            .collect::<Vec<_>>();
        lines.push(format!("Profiles: {}", names.join(" · ")));
    }
    lines.join("\n")
}

async fn connection_info(api: &Api) -> (String, Option<String>) {
    let (tunnel, keys) = tokio::join!(api.get("/api/tunnel/status"), api.get("/api/keys"));
    let tunnel = tunnel.unwrap_or_default();
    let base = if tunnel["enabled"] == true {
        tunnel["publicUrl"]
            .as_str()
            .filter(|url| !url.is_empty())
            .unwrap_or(&api.origin)
    } else {
        &api.origin
    };
    let keys = keys.unwrap_or_default();
    let key = rows(&keys, "keys")
        .first()
        .and_then(|row| row["key"].as_str())
        .map(str::to_owned);
    (format!("{base}/v1"), key)
}

pub(super) async fn choose_model(
    api: &Api,
    title: &str,
    include_combos: bool,
) -> io::Result<Option<String>> {
    let (catalog, connections) = tokio::join!(api.get("/v1/models"), api.get("/api/providers"));
    let providers = crate::provider_index::entries();
    let connections = connections.unwrap_or_default();
    let mut active = HashSet::from(["opencode", "oc"]);
    for connection in rows(&connections, "connections") {
        if connection["isActive"] == false {
            continue;
        }
        if let Some(provider) = connection["provider"].as_str() {
            active.insert(provider);
            let alias = connection["providerSpecificData"]["prefix"]
                .as_str()
                .filter(|alias| !alias.is_empty())
                .or_else(|| {
                    providers
                        .iter()
                        .find(|item| item["id"] == provider)
                        .and_then(|item| item["alias"].as_str())
                });
            if let Some(alias) = alias {
                active.insert(alias);
            }
        }
    }
    let catalog = catalog.unwrap_or_default();
    let mut groups: Vec<(&str, Vec<String>)> = Vec::new();
    for model in rows(&catalog, "data") {
        let (Some(id), Some(owner)) = (model["id"].as_str(), model["owned_by"].as_str()) else {
            continue;
        };
        if !active.contains(owner) && !(include_combos && owner == "combo") {
            continue;
        }
        if let Some((_, models)) = groups.iter_mut().find(|(group, _)| *group == owner) {
            models.push(id.to_owned());
        } else {
            groups.push((owner, vec![id.to_owned()]));
        }
    }
    let mut items = labels(&["← Back", "Search models", "Enter custom model ID"]);
    items.extend(groups.iter().map(|(owner, models)| {
        let provider = providers
            .iter()
            .find(|row| row["alias"] == *owner || row["id"] == *owner)
            .and_then(|row| row["name"].as_str())
            .unwrap_or(owner);
        format!("{provider} ({} models)", models.len())
    }));
    let status = if groups.is_empty() {
        "No connected providers found."
    } else {
        ""
    };
    loop {
        let models = match menu(title, status, &items).await? {
            Some(1) => {
                let Some(query) = prompt("Enter search keyword: ", false)
                    .await?
                    .filter(|value| !value.is_empty())
                else {
                    continue;
                };
                let query = query.to_lowercase();
                groups
                    .iter()
                    .flat_map(|(_, models)| models.iter())
                    .filter(|model| model.to_lowercase().contains(&query))
                    .cloned()
                    .collect()
            }
            Some(2) => {
                return Ok(prompt("Enter custom model ID: ", false)
                    .await?
                    .filter(|model| !model.is_empty()));
            }
            Some(index) if index > 2 => groups[index - 3].1.clone(),
            _ => return Ok(None),
        };
        let mut choices = labels(&["← Back"]);
        choices.extend(models);
        let status = if choices.len() == 1 {
            "No matching models found."
        } else {
            ""
        };
        if let Some(index) = menu(title, status, &choices)
            .await?
            .filter(|index| *index > 0)
        {
            return Ok(Some(choices[index].clone()));
        }
    }
}

async fn setup(
    api: &Api,
    tool: &str,
    name: &str,
    profile: &str,
    path: &str,
    all: bool,
) -> io::Result<()> {
    let (endpoint, key) = connection_info(api).await;
    let Some(key) = key else {
        return notice(
            Err("No API keys found. Create one in API Keys menu first.".into()),
            "",
        )
        .await;
    };
    let body = if tool == "claude" {
        let mut env = Map::new();
        env.insert("ANTHROPIC_BASE_URL".into(), json!(endpoint));
        env.insert("ANTHROPIC_AUTH_TOKEN".into(), json!(key));
        env.insert("API_TIMEOUT_MS".into(), json!("600000"));
        for (_, name, model) in CLAUDE_MODELS {
            env.insert(name.into(), json!(model));
        }
        json!({"env":env})
    } else {
        let Some(model) = choose_model(api, &format!("Select {name} Model"), false).await? else {
            return Ok(());
        };
        match tool {
            "opencode" => {
                let mut models = vec![model.clone()];
                while confirm(&format!("Add another model? (current: {})", models.len())).await? {
                    let Some(next) =
                        choose_model(api, &format!("Add Model #{}", models.len() + 1), false)
                            .await?
                    else {
                        break;
                    };
                    if !models.contains(&next) {
                        models.push(next);
                    }
                }
                let mut subagent = model.clone();
                if confirm(&format!(
                    "Set a different subagent model? (default: {model})"
                ))
                .await?
                    && let Some(picked) = choose_model(api, "Select Subagent Model", false).await?
                {
                    subagent = picked;
                }
                json!({"baseUrl":endpoint,"apiKey":key,"models":models,"activeModel":model,"subagentModel":subagent})
            }
            "hermes" if all => {
                json!({"applyToAll":true,"baseUrl":endpoint,"apiKey":key,"model":model})
            }
            "hermes" => json!({"profile":profile,"baseUrl":endpoint,"apiKey":key,"model":model}),
            _ => json!({"baseUrl":endpoint,"apiKey":key,"model":model}),
        }
    };
    let result = api.request(Method::POST, path, Some(body)).await;
    let success = if all {
        format!(
            "Endpoint applied to {} profile(s).",
            result
                .as_ref()
                .ok()
                .and_then(|value| value["updated"].as_u64())
                .unwrap_or(0)
        )
    } else {
        format!("{name} setup completed!")
    };
    notice(result, &success).await
}
