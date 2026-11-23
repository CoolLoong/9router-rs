use std::io;

use reqwest::Method;
use serde_json::{Map, Value, json};

use super::{Api, confirm, get_text, labels, menu, notice, prompt, resource, rows};

const OAUTH: [(&str, &str); 9] = [
    ("claude", "Claude Code"),
    ("codex", "OpenAI Codex"),
    ("gemini-cli", "Gemini CLI"),
    ("github", "GitHub Copilot"),
    ("antigravity", "Antigravity"),
    ("iflow", "iFlow AI"),
    ("qwen", "Qwen Code"),
    ("kiro", "Kiro AI"),
    ("glm", "Zai GLM Coding"),
];
const API_KEY: [(&str, &str); 7] = [
    ("openrouter", "OpenRouter"),
    ("glm", "Zai GLM Coding"),
    ("minimax", "Minimax Coding"),
    ("kimi", "Kimi"),
    ("openai", "OpenAI"),
    ("anthropic", "Anthropic"),
    ("gemini", "Gemini"),
];

pub(super) async fn run(api: &Api) -> io::Result<()> {
    loop {
        let (connections, nodes) =
            tokio::join!(api.get("/api/providers"), api.get("/api/provider-nodes"));
        let connections = match connections {
            Ok(data) => data,
            Err(error) => return notice(Err(error), "").await,
        };
        let connections = rows(&connections, "connections");
        let nodes = nodes.unwrap_or_default();
        let mut items = labels(&["← Back"]);
        items.extend(
            OAUTH
                .into_iter()
                .map(|(id, name)| label(connections, id, name, "OAuth")),
        );
        items.extend(
            API_KEY
                .into_iter()
                .map(|(id, name)| label(connections, id, name, "API")),
        );
        items.push("── Custom Providers ──".into());
        items.push(format!(
            "Custom Providers - {} Configured",
            rows(&nodes, "nodes").len()
        ));
        let Some(index) = menu("🔌 Providers Management", "", &items).await? else {
            return Ok(());
        };
        if index == 0 {
            return Ok(());
        }
        let index = index - 1;
        if let Some((id, name)) = OAUTH.get(index) {
            connections_menu(api, id, name, true).await?;
        } else if let Some((id, name)) = API_KEY.get(index - OAUTH.len()) {
            connections_menu(api, id, name, false).await?;
        } else if index == OAUTH.len() + API_KEY.len() + 1 {
            custom_menu(api).await?;
        }
    }
}

fn label(connections: &[Value], id: &str, name: &str, auth: &str) -> String {
    let count = connections
        .iter()
        .filter(|connection| matches_provider(connection, id))
        .count();
    format!("{name} ({auth}) - {count} Connected")
}

fn matches_provider(connection: &Value, id: &str) -> bool {
    connection["provider"]
        .as_str()
        .or_else(|| connection["providerId"].as_str())
        == Some(id)
}

async fn connections_menu(api: &Api, id: &str, name: &str, oauth: bool) -> io::Result<()> {
    loop {
        let response = match api.get("/api/providers").await {
            Ok(data) => data,
            Err(error) => return notice(Err(error), "").await,
        };
        let connections = rows(&response, "connections")
            .iter()
            .filter(|connection| matches_provider(connection, id))
            .collect::<Vec<_>>();
        let mut items = labels(&["← Back", "Add New Connection"]);
        items.extend(connections.iter().map(|connection| {
            format!(
                "{} ({})",
                connection_name(connection),
                connection_status(connection)
            )
        }));
        match menu(&format!("🔌 {name}"), "", &items).await? {
            Some(1) if oauth && matches!(id, "github" | "qwen" | "kiro" | "glm") => {
                device_connection(api, id).await?
            }
            Some(1) if oauth => oauth_connection(api, id).await?,
            Some(1) => add_connection(api, id).await?,
            Some(index) if index > 1 => connection_actions(api, connections[index - 2]).await?,
            _ => return Ok(()),
        }
    }
}

fn connection_name(connection: &Value) -> String {
    ["name", "email", "displayName"]
        .into_iter()
        .find_map(|field| connection[field].as_str().filter(|s| !s.is_empty()))
        .map_or_else(|| "Unnamed".into(), super::printable)
}

fn connection_status(connection: &Value) -> &'static str {
    match connection["testStatus"].as_str() {
        Some("active") => "✓ Active",
        Some("error") => "✗ Error",
        _ => "? Unknown",
    }
}

async fn add_connection(api: &Api, id: &str) -> io::Result<()> {
    let Some(name) = required("Connection Name: ", false).await? else {
        return Ok(());
    };
    let Some(key) = required("API Key: ", true).await? else {
        return Ok(());
    };
    notice(
        api.request(
            Method::POST,
            "/api/providers",
            Some(json!({"provider": id, "name": name, "apiKey": key})),
        )
        .await,
        "Connection created successfully!",
    )
    .await
}

async fn connection_actions(api: &Api, connection: &Value) -> io::Result<()> {
    let Some(id) = connection["id"].as_str() else {
        return Ok(());
    };
    let path = resource("/api/providers", id);
    let labels = labels(&[
        "← Back",
        "Rename Connection",
        "Test Connection",
        "Delete Connection",
    ]);
    let mut current = connection.clone();
    loop {
        let name = connection_name(&current);
        let header = format!(
            "Connection: {name}\nStatus: {}",
            connection_status(&current)
        );
        match menu(&format!("🔌 {name}"), &header, &labels).await? {
            Some(1) => {
                if let Some(name) =
                    required(&format!("New name (current: {name}): "), false).await?
                {
                    let result = api
                        .request(Method::PUT, &path, Some(json!({"name": name})))
                        .await;
                    if result.is_ok() {
                        current["name"] = json!(name);
                    }
                    notice(result, "Connection renamed!").await?;
                }
            }
            Some(2) => {
                let result = api
                    .request(Method::POST, &format!("{path}/test"), None)
                    .await
                    .and_then(|data| {
                        if data["success"] == false || data["valid"] == false {
                            Err(data["error"]
                                .as_str()
                                .unwrap_or("Connection test failed")
                                .to_owned())
                        } else {
                            Ok(data)
                        }
                    });
                notice(result, "Connection is working!").await?;
            }
            Some(3) => {
                if confirm(&format!("Delete connection \"{name}\"?")).await? {
                    notice(
                        api.request(Method::DELETE, &path, None).await,
                        "Connection deleted!",
                    )
                    .await?;
                    return Ok(());
                }
            }
            _ => return Ok(()),
        }
    }
}

async fn custom_menu(api: &Api) -> io::Result<()> {
    loop {
        let response = match api.get("/api/provider-nodes").await {
            Ok(data) => data,
            Err(error) => return notice(Err(error), "").await,
        };
        let nodes = rows(&response, "nodes");
        let mut items = labels(&["← Back", "➕ Add Custom Provider"]);
        items.extend(
            nodes
                .iter()
                .map(|node| format!("{} ({})", get_text(node, "name"), get_text(node, "prefix"))),
        );
        match menu("Custom Providers", "", &items).await? {
            Some(1) => create_node(api).await?,
            Some(index) if index > 1 => node_actions(api, &nodes[index - 2]).await?,
            _ => return Ok(()),
        }
    }
}

async fn create_node(api: &Api) -> io::Result<()> {
    let items = labels(&["← Back", "openai-compatible", "anthropic-compatible"]);
    let kind = match menu("Select type", "", &items).await? {
        Some(1) => "openai-compatible",
        Some(2) => "anthropic-compatible",
        _ => return Ok(()),
    };
    let Some(name) = required("Name: ", false).await? else {
        return Ok(());
    };
    let Some(prefix) = required("Prefix (used in model IDs, e.g. myapi): ", false).await? else {
        return Ok(());
    };
    let Some(base_url) = required("Base URL (e.g. https://api.example.com/v1): ", false).await?
    else {
        return Ok(());
    };
    let mut body = json!({"name": name, "prefix": prefix, "baseUrl": base_url, "type": kind});
    if kind == "openai-compatible" {
        let items = labels(&["chat", "responses"]);
        let api_type = match menu("API Type", "", &items).await? {
            Some(0) => "chat",
            Some(1) => "responses",
            _ => return Ok(()),
        };
        body["apiType"] = json!(api_type);
    }
    notice(
        api.request(Method::POST, "/api/provider-nodes", Some(body))
            .await,
        "Provider created!",
    )
    .await
}

async fn node_actions(api: &Api, node: &Value) -> io::Result<()> {
    let Some(id) = node["id"].as_str() else {
        return Ok(());
    };
    let path = resource("/api/provider-nodes", id);
    let items = labels(&["← Back", "Connections", "Edit Node", "Delete Node"]);
    let name = get_text(node, "name");
    let header = format!(
        "Prefix: {}\nBase URL: {}",
        get_text(node, "prefix"),
        get_text(node, "baseUrl")
    );
    loop {
        match menu(&name, &header, &items).await? {
            Some(1) => connections_menu(api, id, &name, false).await?,
            Some(2) => {
                let mut updates = Map::new();
                for (key, title) in [
                    ("name", "Name"),
                    ("baseUrl", "Base URL"),
                    ("prefix", "Prefix"),
                ] {
                    match prompt(&format!("{title} ({}): ", get_text(node, key)), false).await? {
                        Some(value) if !value.is_empty() => {
                            updates.insert(key.to_owned(), json!(value));
                        }
                        Some(_) => (),
                        None => return Ok(()),
                    }
                }
                if !updates.is_empty() {
                    notice(
                        api.request(Method::PUT, &path, Some(Value::Object(updates)))
                            .await,
                        "Updated!",
                    )
                    .await?;
                }
                return Ok(());
            }
            Some(3) => {
                if confirm(&format!("Delete \"{name}\"?")).await? {
                    notice(
                        api.request(Method::DELETE, &path, None).await,
                        "Provider deleted!",
                    )
                    .await?;
                    return Ok(());
                }
            }
            _ => return Ok(()),
        }
    }
}

async fn required(message: &str, hidden: bool) -> io::Result<Option<String>> {
    prompt(message, hidden)
        .await
        .map(|value| value.filter(|value| !value.is_empty()))
}

async fn oauth_connection(api: &Api, id: &str) -> io::Result<()> {
    let path = resource("/api/oauth", id);
    let redirect = if id == "codex" {
        "http://localhost:1455/auth/callback"
    } else {
        "http://localhost:20128/callback"
    };
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("redirect_uri", redirect)
        .finish();
    let authorization = match api.get(&format!("{path}/authorize?{query}")).await {
        Ok(value) => value,
        Err(error) => return notice(Err(error), "").await,
    };
    let Some(auth_url) = authorization["authUrl"].as_str() else {
        return notice(Err("No OAuth authorization URL returned".into()), "").await;
    };
    println!(
        "Open this URL to authorize {}:\n{}",
        super::printable(id),
        super::printable(auth_url)
    );
    if super::copy(auth_url) {
        println!("Authorization link copied to clipboard.");
    }
    let Some(callback) = required("Paste callback URL: ", true).await? else {
        return Ok(());
    };
    let Ok(callback) = url::Url::parse(&callback) else {
        return notice(Err("Invalid callback URL".into()), "").await;
    };
    let mut payload = json!({
        "redirectUri": authorization["redirectUri"].as_str().unwrap_or(redirect),
        "codeVerifier": authorization["codeVerifier"],
        "state": authorization["state"],
    });
    let mut error = None;
    let mut description = None;
    for (key, value) in callback.query_pairs() {
        match key.as_ref() {
            "code" | "state" if !value.is_empty() => payload[key.as_ref()] = json!(value),
            "error" if !value.is_empty() => error = Some(value),
            "error_description" => description = Some(value),
            _ => (),
        }
    }
    if let Some(error) = error {
        return notice(
            Err(format!(
                "Authorization failed: {}",
                description.unwrap_or(error)
            )),
            "",
        )
        .await;
    }
    if payload["code"].is_null() {
        return notice(Err("No authorization code found".into()), "").await;
    }
    notice(
        api.request(Method::POST, &format!("{path}/exchange"), Some(payload))
            .await,
        "Connection created successfully!",
    )
    .await?;
    Ok(())
}

async fn device_connection(api: &Api, id: &str) -> io::Result<()> {
    let path = resource("/api/oauth", id);
    let device = match api.get(&format!("{path}/device-code")).await {
        Ok(value) => value,
        Err(error) => return notice(Err(error), "").await,
    };
    let Some(code) = device["device_code"].as_str() else {
        return notice(Err("No device code received".into()), "").await;
    };
    let complete = device["verification_uri_complete"]
        .as_str()
        .filter(|url| !url.is_empty());
    let url = complete
        .or_else(|| device["verification_uri"].as_str())
        .unwrap_or_default();
    println!("📱 Device Login\nOpen: {}", super::printable(url));
    if !url.is_empty() && super::copy(url) {
        println!("Authorization link copied to clipboard.");
    }
    if complete.is_none()
        && let Some(code) = device["user_code"].as_str()
    {
        println!("Enter code: {}", super::printable(code));
    }
    println!("Waiting for authorization...");
    let payload = json!({
        "deviceCode": code,
        "codeVerifier": device["codeVerifier"],
        "extraData": device.get("extraData").unwrap_or(&device),
    });
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        match api
            .request(Method::POST, &format!("{path}/poll"), Some(payload.clone()))
            .await
        {
            Ok(data) if data["success"] == true => {
                return notice(Ok(data), "Connection created successfully!").await;
            }
            Ok(data) if data["pending"] == true => (),
            Ok(data) => {
                let error = data["error"].as_str().unwrap_or("Unknown error");
                if !matches!(error, "authorization_pending" | "slow_down") {
                    return notice(Err(error.to_owned()), "").await;
                }
            }
            Err(error) if matches!(error.as_str(), "authorization_pending" | "slow_down") => (),
            Err(error) => return notice(Err(error), "").await,
        }
    }
    notice(Err("Timeout waiting for authorization".into()), "").await
}
