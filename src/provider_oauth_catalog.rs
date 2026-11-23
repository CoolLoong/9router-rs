use std::time::Duration;

use axum::http::StatusCode;
use axum::response::Response;
use reqwest::{Method, Response as Upstream};
use serde_json::{Value, json};

use crate::provider_connections::truthy;
use crate::provider_http::{self, Bound, http_client};
use crate::provider_oauth::{RefreshMode, refresh_account};

use super::{ProviderTools, error, redact, reply, scrub_payload, secrets};

const TIMEOUT: Duration = Duration::from_secs(15);
const LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Catalog {
    GeminiCli,
    Codex,
}

pub(super) async fn models(
    state: &ProviderTools,
    account: &Value,
    id: &str,
    catalog: Catalog,
) -> Response {
    let Some(token) = nonempty(&account["accessToken"]) else {
        return error(StatusCode::UNAUTHORIZED, "No valid token found");
    };
    let mut private = secrets(account);
    let fetched = match fetch(state, account, token, &mut private, catalog).await {
        Ok(response) => read_models(response, &private, catalog).await,
        Err(message) => Err(message.into()),
    };
    let (provider, label) = match catalog {
        Catalog::GeminiCli => ("gemini-cli", "Gemini CLI"),
        Catalog::Codex => ("codex", "Codex"),
    };
    let mut result = json!({"provider":provider,"connectionId":id,"models":[]});
    match fetched {
        Ok(models) => {
            result["models"] = Value::Array(models);
            scrub_payload(&mut result["models"], &private);
        }
        Err(message) => {
            result["warning"] = json!(format!("Failed to fetch {label} models: {message}"));
        }
    }
    reply(StatusCode::OK, result)
}

async fn fetch(
    state: &ProviderTools,
    account: &Value,
    token: &str,
    private: &mut Vec<String>,
    catalog: Catalog,
) -> Result<Upstream, &'static str> {
    let (endpoint, fixture) = match catalog {
        Catalog::GeminiCli => (
            "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels".to_owned(),
            "http://127.0.0.1:52989/gemini-cli/v1internal:fetchAvailableModels",
        ),
        Catalog::Codex => (
            format!(
                "https://chatgpt.com/backend-api/codex/models?client_version={}",
                crate::codex_executor::cli_version(),
            ),
            "http://127.0.0.1:52989/codex/models",
        ),
    };
    let direct = http_client(TIMEOUT).build().map_err(|_| "fetch failed")?;
    let delivery = provider_http::bind(
        &state.storage,
        account,
        &endpoint,
        fixture,
        &direct,
        TIMEOUT,
        None,
    )
    .await
    .ok_or("Required provider proxy is unavailable")?;
    private.extend(delivery.relay_secret().map(str::to_owned));
    let response = send(&delivery, account, token, catalog).await?;
    let mode = delivery.route.relay_identity().map_or(
        RefreshMode::Routed(delivery.protected),
        RefreshMode::RelayRouted,
    );
    if matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) && nonempty(&account["refreshToken"]).is_some()
        && let Ok(current) = refresh_account(&state.storage, account, mode).await
        && current["isActive"] == account["isActive"]
        && let Some(token) = nonempty(&current["accessToken"])
        && let Some(updated) = provider_http::bind(
            &state.storage,
            &current,
            &endpoint,
            fixture,
            &direct,
            TIMEOUT,
            None,
        )
        .await
        && delivery.same_route(&updated)
    {
        private.extend(secrets(&current));
        drop(response);
        return send(&updated, &current, token, catalog).await;
    }
    Ok(response)
}

async fn send(
    delivery: &Bound,
    account: &Value,
    token: &str,
    catalog: Catalog,
) -> Result<Upstream, &'static str> {
    let request = match catalog {
        Catalog::GeminiCli => {
            let project = crate::google_code_assist::saved_project(account);
            let body = project.map_or_else(|| json!({}), |project| json!({"project":project}));
            delivery
                .request(Method::POST)
                .header("user-agent", "google-api-nodejs-client/9.15.1")
                .header(
                    "x-goog-api-client",
                    "google-cloud-sdk vscode_cloudshelleditor/0.1",
                )
                .json(&body)
        }
        Catalog::Codex => delivery
            .request(Method::GET)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("originator", "codex_cli_rs"),
    };
    request
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| "fetch failed")
}

async fn read_models(
    mut response: Upstream,
    private: &[String],
    catalog: Catalog,
) -> Result<Vec<Value>, String> {
    let status = response.status();
    let limit = if status.is_success() { LIMIT } else { 8192 };
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "fetch failed".to_owned())?
    {
        if chunk.len() > limit - bytes.len() {
            return Err(if status.is_success() {
                "model catalog exceeded the response limit".into()
            } else {
                status.as_u16().to_string()
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let body = redact(String::from_utf8_lossy(&bytes).trim().to_owned(), private);
        return Err(if body.is_empty() {
            status.as_u16().to_string()
        } else {
            format!("{} {body}", status.as_u16())
        });
    }
    let data: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid model catalog")?;
    Ok(match catalog {
        Catalog::GeminiCli => gemini_models(&data),
        Catalog::Codex => codex_models(&data),
    })
}

fn gemini_models(data: &Value) -> Vec<Value> {
    let models = &data["models"];
    if let Some(items) = models.as_array() {
        return items
            .iter()
            .filter_map(|item| {
                let id = ["id", "model", "name"]
                    .into_iter()
                    .find_map(|key| nonempty(&item[key]))?;
                Some(model_row(id, item))
            })
            .collect();
    }
    models
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, entry)| !truthy(&entry["isInternal"]))
        .map(|(id, item)| model_row(id, item))
        .collect()
}

fn codex_models(data: &Value) -> Vec<Value> {
    let rows = data.as_array().or_else(|| {
        ["data", "models", "results"]
            .into_iter()
            .find_map(|key| data[key].as_array())
    });
    let mut models = Vec::new();
    for item in rows.into_iter().flatten() {
        let Some(id) = ["id", "slug", "model", "name"]
            .into_iter()
            .find_map(|key| nonempty(&item[key]))
        else {
            continue;
        };
        let name = ["display_name", "displayName", "name"]
            .into_iter()
            .find_map(|key| nonempty(&item[key]))
            .unwrap_or(id);
        let mut row = item.clone();
        row["id"] = json!(id);
        row["name"] = json!(name);
        if item["type"] != "image"
            && !id.to_ascii_lowercase().contains("embed")
            && !id.ends_with("-review")
        {
            models.push(row.clone());
            row["id"] = json!(format!("{id}-review"));
            row["name"] = json!(format!("{name} Review"));
            row["upstreamModelId"] = json!(id);
            row["quotaFamily"] = json!("review");
        }
        models.push(row);
    }
    models
}

fn model_row(id: &str, item: &Value) -> Value {
    let name = nonempty(&item["displayName"])
        .or_else(|| nonempty(&item["name"]))
        .unwrap_or(id);
    json!({"id":id,"name":name})
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.trim().is_empty())
}
