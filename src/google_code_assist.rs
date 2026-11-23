use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use reqwest::Method;
use rusqlite::TransactionBehavior;
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

use crate::provider_connections;
use crate::provider_http;
use crate::storage::Storage;

type ProjectLocks = HashMap<(PathBuf, String), Weak<AsyncMutex<()>>>;
static PROJECTS: LazyLock<Mutex<ProjectLocks>> = LazyLock::new(Mutex::default);
const DISCOVERY_BASE: &str = "https://cloudcode-pa.googleapis.com/v1internal";
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
static DISCOVERY_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    provider_http::http_client(DISCOVERY_TIMEOUT)
        .build()
        .expect("Google Code Assist project discovery HTTP client")
});

pub(super) fn saved_project(account: &Value) -> Option<&str> {
    project_id(&account["projectId"])
        .or_else(|| project_id(&account["providerSpecificData"]["projectId"]))
}

pub(super) fn project_id(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value["id"].as_str())
        .map(crate::provider_index::js_trim)
        .filter(|project| !project.is_empty())
}

#[derive(Clone, Copy)]
pub(super) enum ProjectError {
    Missing,
    Rejected,
    AccountChanged,
    Route,
}

pub(super) async fn discover_project(
    storage: &Storage,
    account: &Value,
    relay: Option<[u8; 32]>,
) -> Result<Value, ProjectError> {
    let id = account["id"].as_str().ok_or(ProjectError::AccountChanged)?;
    let provider = match account["provider"].as_str() {
        Some("gemini-cli") => "gemini-cli",
        Some("antigravity") => "antigravity",
        _ => return Err(ProjectError::AccountChanged),
    };
    let lock = {
        let mut locks = PROJECTS.lock().map_err(|_| ProjectError::AccountChanged)?;
        locks.retain(|_, lock| lock.strong_count() != 0);
        let key = (storage.root().to_path_buf(), id.to_owned());
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(AsyncMutex::new(()));
            locks.insert(key, Arc::downgrade(&lock));
            lock
        }
    };
    let _guard = lock.lock().await;
    let current = active_account(storage, provider, id).await?;
    if saved_project(&current).is_some() {
        return Ok(current);
    }
    let metadata = json!({"ideType":9,"platform":platform(),"pluginType":2});
    let info = discovery_request(
        storage,
        &current,
        provider,
        "loadCodeAssist",
        json!({"metadata":metadata}),
        relay,
    )
    .await?;
    let project = if let Some(project) = project_id(&info["cloudaicompanionProject"]) {
        project.to_owned()
    } else {
        let tier = info["allowedTiers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|tier| tier["isDefault"] == true)
            .find_map(|tier| {
                tier["id"]
                    .as_str()
                    .map(crate::provider_index::js_trim)
                    .filter(|id| !id.is_empty())
            })
            .unwrap_or("legacy-tier");
        let body = json!({"tierId":tier,"metadata":metadata});
        let attempts = std::env::var("ONBOARD_MAX_ATTEMPTS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|attempts| *attempts > 0)
            .unwrap_or(2);
        let delay = std::env::var("ONBOARD_RETRY_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|delay| *delay > 0)
            .unwrap_or(12_000);
        let mut result = None;
        for attempt in 0..attempts {
            let latest = active_account(storage, provider, id).await?;
            if saved_project(&latest).is_some() {
                return Ok(latest);
            }
            match discovery_request(
                storage,
                &current,
                provider,
                "onboardUser",
                body.clone(),
                relay,
            )
            .await
            {
                Ok(reply) if reply["done"] == true => {
                    result = project_id(&reply["response"]["cloudaicompanionProject"])
                        .map(str::to_owned);
                    if result.is_some() {
                        break;
                    }
                }
                Ok(_) | Err(ProjectError::Missing) => {}
                Err(error) => return Err(error),
            }
            if attempt + 1 < attempts {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
        result.ok_or(ProjectError::Missing)?
    };
    let token = current["accessToken"].clone();
    let id = id.to_owned();
    storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(mut latest) =
                provider_connections::by_id(&tx, &id)?.filter(|account| active(account, provider))
            else {
                return Ok(None);
            };
            if saved_project(&latest).is_some() {
                return Ok(Some(latest));
            }
            if latest["accessToken"] != token {
                return Ok(None);
            }
            latest["projectId"] = json!(project);
            latest["updatedAt"] = json!(provider_connections::now());
            provider_connections::write(&tx, &latest)?;
            tx.commit()?;
            Ok(Some(latest))
        })
        .await
        .map_err(|_| ProjectError::AccountChanged)?
        .ok_or(ProjectError::AccountChanged)
}

fn active(account: &Value, provider: &str) -> bool {
    account["provider"] == provider && account["authType"] == "oauth" && account["isActive"] == true
}

async fn active_account(
    storage: &Storage,
    provider: &'static str,
    id: &str,
) -> Result<Value, ProjectError> {
    let id = id.to_owned();
    storage
        .run(false, move |db| provider_connections::by_id(db, &id))
        .await
        .map_err(|_| ProjectError::AccountChanged)?
        .filter(|account| active(account, provider))
        .ok_or(ProjectError::AccountChanged)
}

async fn discovery_request(
    storage: &Storage,
    account: &Value,
    provider: &'static str,
    operation: &str,
    body: Value,
    relay: Option<[u8; 32]>,
) -> Result<Value, ProjectError> {
    let token = account["accessToken"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or(ProjectError::Missing)?;
    let id = account["id"].as_str().ok_or(ProjectError::AccountChanged)?;
    let endpoint = format!("{DISCOVERY_BASE}:{operation}");
    let current = active_account(storage, provider, id).await?;
    if current["accessToken"] != token {
        return Err(ProjectError::AccountChanged);
    }
    let delivery = if let Some(identity) = relay {
        provider_http::bind_relay(
            storage,
            &current,
            &endpoint,
            &DISCOVERY_CLIENT,
            DISCOVERY_TIMEOUT,
            identity,
        )
        .await
    } else {
        let fixture = format!("http://127.0.0.1:52989/{provider}/v1internal:{operation}");
        provider_http::bind(
            storage,
            &current,
            &endpoint,
            &fixture,
            &DISCOVERY_CLIENT,
            DISCOVERY_TIMEOUT,
            None,
        )
        .await
    }
    .ok_or(ProjectError::Route)?;
    let request = delivery.request(Method::POST).bearer_auth(token);
    let request = if provider == "antigravity" {
        request.header("user-agent", "9Router")
    } else {
        request
            .header("user-agent", "google-api-nodejs-client/9.15.1")
            .header(
                "x-goog-api-client",
                "google-cloud-sdk vscode_cloudshelleditor/0.1",
            )
            .header("client-metadata", body["metadata"].to_string())
    };
    let response = request
        .json(&body)
        .send()
        .await
        .map_err(|_| ProjectError::Missing)?;
    if matches!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return Err(ProjectError::Rejected);
    }
    if !response.status().is_success() {
        return Err(ProjectError::Missing);
    }
    response.json().await.map_err(|_| ProjectError::Missing)
}

pub(super) fn platform() -> u8 {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => 2,
        ("macos", _) => 1,
        ("linux", "aarch64") => 4,
        ("linux", _) => 3,
        ("windows", _) => 5,
        _ => 0,
    }
}
