use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use reqwest::{Client, Method, RequestBuilder, header};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::github_executor::{catalog_metadata, token};
use crate::provider_connections::truthy;
use crate::provider_http::{self, Bound, http_client};
use crate::provider_oauth::{RefreshError, RefreshMode, refresh_account};
use crate::proxy_transport::Route;
use crate::storage::Storage;

const URL: &str = "https://api.githubcopilot.com/models";
const FIXTURE_URL: &str = "http://127.0.0.1:52989/github/models";
const CACHE_TTL: Duration = Duration::from_secs(300);
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CREDENTIALS: usize = 256;

type Failure = (StatusCode, String);
type Models = Vec<Value>;
type Slots = VecDeque<([u8; 32], Arc<AsyncMutex<Option<Cached>>>)>;

struct Cached {
    expires: Instant,
    models: Models,
}

pub(super) struct Resolver {
    http: Client,
    slots: Mutex<Slots>,
}

static RESOLVER: LazyLock<Resolver> = LazyLock::new(|| Resolver {
    http: http_client(TIMEOUT)
        .build()
        .expect("Copilot models HTTP client"),
    slots: Mutex::new(VecDeque::new()),
});

pub async fn for_management(
    storage: &Storage,
    settings: &Value,
    connection: &Value,
) -> Result<Models, Failure> {
    let delivery = RESOLVER
        .bind(storage, settings, connection)
        .await
        .ok_or_else(proxy_unavailable)?;
    RESOLVER.management(connection, &delivery).await
}

pub async fn for_catalog(
    storage: &Storage,
    settings: &Value,
    connection: &Value,
) -> Option<Models> {
    let delivery = RESOLVER.bind(storage, settings, connection).await?;
    RESOLVER.live(storage, connection, &delivery).await
}

impl Resolver {
    async fn bind(&self, storage: &Storage, settings: &Value, connection: &Value) -> Option<Bound> {
        let route = provider_http::select_authenticated(storage, settings, connection, URL)
            .await
            .ok()?;
        provider_http::bind_selection(route, URL, FIXTURE_URL, &self.http, TIMEOUT, None)
    }

    async fn management(&self, connection: &Value, delivery: &Bound) -> Result<Models, Failure> {
        let token = token(connection)
            .or_else(|| nonempty(&connection["apiKey"]))
            .ok_or((StatusCode::UNAUTHORIZED, "No valid token found".to_owned()))?;
        let raw = fetch(
            request(token, delivery)
                .header("editor-version", "vscode/1.107.1")
                .header("editor-plugin-version", "copilot-chat/0.26.7")
                .header(header::USER_AGENT, "GitHubCopilotChat/0.26.7"),
            delivery.relay_secret(),
        )
        .await?;
        management_models(&raw).ok_or_else(invalid)
    }

    async fn live(
        &self,
        storage: &Storage,
        connection: &Value,
        delivery: &Bound,
    ) -> Option<Models> {
        let initial = token(connection)?;
        let slot = self.slot(initial, delivery);
        let mut cached = slot.lock().await;
        let now = Instant::now();
        if let Some(hit) = &*cached
            && hit.expires > now
        {
            return Some(hit.models.clone());
        }

        let mut selected = connection.clone();
        if refreshable(connection) && expired_copilot(connection) {
            match refresh_account(
                storage,
                &selected,
                RefreshMode::github(false, delivery.route.relay_identity()),
            )
            .await
            {
                Ok(latest) => selected = latest,
                Err(RefreshError::AccountUnavailable) => return None,
                Err(_) => (),
            }
            if !same_delivery(storage, &selected, delivery).await {
                return None;
            }
        }
        let raw = match fetch(
            live_request(token(&selected)?, delivery),
            delivery.relay_secret(),
        )
        .await
        {
            Ok(models) => models,
            Err((status, _))
                if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                    && refreshable(&selected)
                    && nonempty(&selected["accessToken"]).is_some() =>
            {
                let refreshed = refresh_account(
                    storage,
                    &selected,
                    RefreshMode::CopilotOnly {
                        relay: delivery.route.relay_identity(),
                    },
                )
                .await
                .ok()?;
                if !same_delivery(storage, &refreshed, delivery).await {
                    return None;
                }
                let token = nonempty(&refreshed["providerSpecificData"]["copilotToken"])?;
                fetch(live_request(token, delivery), delivery.relay_secret())
                    .await
                    .ok()?
            }
            Err(_) => return None,
        };
        let models = live_models(&raw);
        if models.is_empty() {
            return None;
        }
        *cached = Some(Cached {
            expires: now + CACHE_TTL,
            models: models.clone(),
        });
        Some(models)
    }

    fn slot(&self, credential: &str, delivery: &Bound) -> Arc<AsyncMutex<Option<Cached>>> {
        let mut key = Sha256::new().chain_update(credential).chain_update([0]);
        match &delivery.route {
            Route::Direct => key.update([u8::from(delivery.protected)]),
            Route::Proxy { url, .. } => {
                key.update([2]);
                key.update(url.as_bytes());
            }
            Route::Relay { .. } => {
                key.update([3]);
                key.update(
                    delivery
                        .route
                        .relay_identity()
                        .expect("authenticated Copilot Relay"),
                );
            }
            Route::MissingStrict => unreachable!("a missing proxy cannot bind a Copilot request"),
        }
        let key: [u8; 32] = key.finalize().into();
        let mut slots = self.slots.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(index) = slots.iter().position(|(candidate, _)| *candidate == key)
            && let Some(entry) = slots.remove(index)
        {
            let hit = entry.1.clone();
            slots.push_back(entry);
            return hit;
        }
        if slots.len() >= MAX_CREDENTIALS {
            slots.pop_front();
        }
        let slot = Arc::new(AsyncMutex::new(None));
        slots.push_back((key, slot.clone()));
        slot
    }
}

fn live_request(token: &str, delivery: &Bound) -> RequestBuilder {
    let (vscode, chat_version, agent, api_version) = catalog_metadata();
    request(token, delivery)
        .header("editor-version", format!("vscode/{vscode}"))
        .header(
            "editor-plugin-version",
            format!("copilot-chat/{chat_version}"),
        )
        .header(header::USER_AGENT, agent)
        .header("x-github-api-version", api_version)
}

fn request(token: &str, delivery: &Bound) -> RequestBuilder {
    delivery
        .request(Method::GET)
        .bearer_auth(token)
        .header(header::CONTENT_TYPE, "application/json")
        .header("copilot-integration-id", "vscode-chat")
}

async fn same_delivery(storage: &Storage, connection: &Value, expected: &Bound) -> bool {
    let Some(settings) = storage.read_settings().await else {
        return false;
    };
    RESOLVER
        .bind(storage, &settings, connection)
        .await
        .is_some_and(|current| current.same_route(expected))
}

fn proxy_unavailable() -> Failure {
    (
        StatusCode::BAD_GATEWAY,
        "Copilot models connection proxy is unavailable".into(),
    )
}

async fn fetch(request: RequestBuilder, relay: Option<&str>) -> Result<Value, Failure> {
    let response = request.send().await.map_err(|_| invalid())?;
    if !response.status().is_success() {
        return Err((
            response.status(),
            format!("Failed to fetch models: {}", response.status().as_u16()),
        ));
    }
    let mut value = response.json::<Value>().await.map_err(|_| invalid())?;
    if let Some(secret) = relay {
        crate::provider_tools::scrub_payload(&mut value, &[secret.to_owned()]);
    }
    Ok(value)
}

fn invalid() -> Failure {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Failed to fetch models".into(),
    )
}

fn refreshable(connection: &Value) -> bool {
    connection["authType"] == "oauth"
        && connection["isActive"] != false
        && nonempty(&connection["id"]).is_some()
}

fn expired_copilot(connection: &Value) -> bool {
    let data = &connection["providerSpecificData"];
    if nonempty(&data["copilotToken"]).is_none() {
        return false;
    }
    let expires = &data["copilotTokenExpiresAt"];
    let milliseconds = expires
        .as_i64()
        .map(|value| {
            if value < 1_000_000_000_000 {
                value.saturating_mul(1000)
            } else {
                value
            }
        })
        .or_else(|| {
            let date = chrono::DateTime::parse_from_rfc3339(expires.as_str()?).ok()?;
            Some(date.timestamp_millis())
        });
    milliseconds.is_some_and(|expires| expires < chrono::Utc::now().timestamp_millis())
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

fn management_models(data: &Value) -> Option<Models> {
    if !truthy(&data["data"]) {
        return Some(Vec::new());
    }
    let models = data["data"].as_array()?;
    if models.iter().any(Value::is_null) {
        return None;
    }
    Some(
        models
            .iter()
            .filter(|model| {
                model["capabilities"]["type"] == "chat" && model["policy"]["state"] != "disabled"
            })
            .map(|model| {
                let mut out = Map::new();
                if let Some(id) = model.get("id") {
                    out.insert("id".to_owned(), id.clone());
                }
                if let Some(name) = model
                    .get("name")
                    .filter(|name| truthy(name))
                    .or_else(|| model.get("id"))
                {
                    out.insert("name".to_owned(), name.clone());
                }
                if let Some(version) = model.get("version") {
                    out.insert("version".to_owned(), version.clone());
                }
                out.insert("capabilities".to_owned(), model["capabilities"].clone());
                out.insert(
                    "isDefault".to_owned(),
                    json!(model["model_picker_enabled"] == true),
                );
                Value::Object(out)
            })
            .collect(),
    )
}

fn live_models(data: &Value) -> Models {
    let mut seen = HashSet::new();
    data["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| {
            if model["capabilities"]["type"] != "chat"
                || (truthy(&model["policy"]) && model["policy"]["state"] != "enabled")
            {
                return None;
            }
            let id = nonempty(&model["id"])?;
            if !seen.insert(id) {
                return None;
            }
            let name = model
                .get("name")
                .filter(|value| truthy(value))
                .cloned()
                .unwrap_or_else(|| json!(id));
            Some(json!({"id":id,"name":name}))
        })
        .collect()
}
