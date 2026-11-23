use std::sync::LazyLock;
use std::time::Duration;

use reqwest::{Client, Method, RequestBuilder, Url, header};
use rusqlite::TransactionBehavior;
use serde_json::{Value, json};

use super::flow::{ApiError, Credentials, Google, mapped_code_tokens, nonempty, platform};
use crate::provider_connections::{self, by_id};
use crate::provider_http::{self, Bound, http_client};
use crate::storage::Storage;

const TIMEOUT: Duration = Duration::from_secs(20);
const LIMIT: usize = 256 * 1024;
static CLIENT: LazyLock<Option<Client>> = LazyLock::new(|| http_client(TIMEOUT).build().ok());

pub(super) async fn exchange(
    storage: &Storage,
    config: &Google,
    code: &str,
    redirect: &str,
) -> Result<(Credentials, Option<String>), ApiError> {
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(|| ApiError::failed("Could not load Antigravity authorization settings"))?;
    let response = request(
        storage,
        &settings,
        &Value::Null,
        &config.token,
        "token",
        Method::POST,
    )
    .await
    .ok_or_else(|| ApiError::failed("Antigravity authorization proxy is unavailable"))?;
    let tokens = json_reply(response.form(&[
        ("grant_type", "authorization_code"),
        ("client_id", config.client.as_str()),
        ("client_secret", config.secret.as_str()),
        ("code", code),
        ("redirect_uri", redirect),
    ]))
    .await
    .ok_or_else(|| ApiError::failed("Antigravity token exchange failed"))?;
    let access = nonempty(&tokens["access_token"])
        .filter(|token| token.bytes().all(|byte| byte.is_ascii_graphic()))
        .ok_or_else(|| ApiError::failed("Antigravity token exchange returned no access token"))?;
    let mut profile_url = Url::parse(&config.user)
        .map_err(|_| ApiError::failed("Invalid Antigravity profile endpoint"))?;
    profile_url.query_pairs_mut().append_pair("alt", "json");
    let profile = match request(
        storage,
        &settings,
        &Value::Null,
        profile_url.as_str(),
        "user",
        Method::GET,
    )
    .await
    {
        Some(request) => json_reply(
            request
                .bearer_auth(access)
                .header("x-request-source", "local"),
        )
        .await
        .unwrap_or_default(),
        None => Value::Null,
    };
    let project = match request(
        storage,
        &settings,
        &Value::Null,
        &config.project,
        "project",
        Method::POST,
    )
    .await
    {
        Some(request) => json_reply(
            request
                .bearer_auth(access)
                .header("x-request-source", "local")
                .json(&json!({"metadata":metadata()})),
        )
        .await
        .unwrap_or_default(),
        None => Value::Null,
    };
    let project_id = crate::google_code_assist::project_id(&project["cloudaicompanionProject"]);
    let onboarding = project_id.map(|_| {
        project["allowedTiers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|tier| tier["isDefault"] == true)
            .find_map(|tier| {
                nonempty(&tier["id"])
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
            })
            .unwrap_or("legacy-tier")
            .to_owned()
    });
    let mut fields = mapped_code_tokens(&tokens);
    if let Some(email) = nonempty(&profile["email"]) {
        fields.insert("email".into(), json!(email));
    }
    if let Some(project_id) = project_id {
        fields.insert("projectId".into(), json!(project_id));
    }
    Ok((Credentials(fields), onboarding))
}

pub(super) fn onboard(storage: Storage, endpoint: String, connection: &Value, tier: String) {
    let (Some(id), Some(project), Some(token)) = (
        nonempty(&connection["id"]),
        nonempty(&connection["projectId"]),
        nonempty(&connection["accessToken"]),
    ) else {
        return;
    };
    let id = id.to_owned();
    let project = project.to_owned();
    let token = token.to_owned();
    tokio::spawn(async move {
        let mut first_route = None;
        for _ in 0..10 {
            let Some(current) = storage
                .run(false, {
                    let id = id.clone();
                    move |db| by_id(db, &id)
                })
                .await
                .ok()
                .flatten()
                .filter(|account| {
                    account["provider"] == "antigravity"
                        && account["authType"] == "oauth"
                        && account["isActive"] != false
                        && account["projectId"] == project
                        && account["accessToken"] == token
                })
            else {
                return;
            };
            let Some(settings) = storage.read_settings().await else {
                return;
            };
            let Some(route) = binding(&storage, &settings, &current, &endpoint, "onboard").await
            else {
                return;
            };
            if first_route
                .as_ref()
                .is_some_and(|expected: &Bound| !expected.same_route(&route))
            {
                return;
            }
            let request = request_for_binding(&route, Method::POST)
                .bearer_auth(&token)
                .header("x-request-source", "local")
                .json(&json!({"tierId":tier,"metadata":metadata()}));
            first_route.get_or_insert(route);
            let Some(reply) = json_reply(request).await else {
                return;
            };
            if reply["done"] == true {
                if let Some(updated) = crate::google_code_assist::project_id(
                    &reply["response"]["cloudaicompanionProject"],
                )
                .filter(|updated| *updated != project)
                {
                    save_project(&storage, id, project, token, updated.to_owned()).await;
                }
                return;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn save_project(
    storage: &Storage,
    id: String,
    previous: String,
    token: String,
    project: String,
) {
    let _ = storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(mut account) = by_id(&tx, &id)?
                && account["provider"] == "antigravity"
                && account["authType"] == "oauth"
                && account["isActive"] != false
                && account["projectId"] == previous
                && account["accessToken"] == token
            {
                account["projectId"] = json!(project);
                account["updatedAt"] = json!(provider_connections::now());
                provider_connections::write(&tx, &account)?;
            }
            tx.commit()
        })
        .await;
}

fn metadata() -> Value {
    json!({"ideType":9,"platform":platform(),"pluginType":2})
}

async fn request(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    endpoint: &str,
    label: &str,
    method: Method,
) -> Option<RequestBuilder> {
    let route = binding(storage, settings, account, endpoint, label).await?;
    Some(request_for_binding(&route, method))
}

async fn binding(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    endpoint: &str,
    label: &str,
) -> Option<Bound> {
    let selection = provider_http::select(storage, settings, account, endpoint)
        .await
        .ok()?;
    let original = Url::parse(endpoint).ok()?;
    let mut local =
        Url::parse(&format!("http://127.0.0.1:52989/antigravity-oauth/{label}")).ok()?;
    local.set_query(original.query());
    provider_http::bind_selection(
        selection,
        endpoint,
        local.as_str(),
        CLIENT.as_ref()?,
        TIMEOUT,
        None,
    )
}

fn request_for_binding(route: &Bound, method: Method) -> RequestBuilder {
    route
        .request(method)
        .header(header::USER_AGENT, "9Router")
        .header(header::ACCEPT, "application/json")
}

async fn json_reply(request: RequestBuilder) -> Option<Value> {
    let mut reply = request.send().await.ok()?.error_for_status().ok()?;
    if !reply.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = reply.chunk().await.ok()? {
        if chunk.len() > LIMIT - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}
