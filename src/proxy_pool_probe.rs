use std::time::{Duration, Instant};

use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use rusqlite::TransactionBehavior;
use serde_json::{Value, json};
use url::Url;

use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::proxy_transport::{Route, pool_relay};
use crate::storage::Storage;

use super::{StateData, by_id, error, missing, now, persist, relay, reply};

const PROBE_URL: &str = "https://google.com/";
const FIXTURE_URL: &str = "http://127.0.0.1:52983/proxy-pool-test";

struct Probe {
    ok: bool,
    status: u16,
    status_text: Option<&'static str>,
    error: Option<&'static str>,
    elapsed_ms: u64,
}

impl Probe {
    fn failed(status: u16, error: &'static str) -> Self {
        Self {
            ok: false,
            status,
            status_text: None,
            error: Some(error),
            elapsed_ms: 0,
        }
    }

    fn problem(&self) -> Option<String> {
        (!self.ok).then(|| {
            self.error.map_or_else(
                || format!("Proxy test failed with status {}", self.status),
                ToOwned::to_owned,
            )
        })
    }

    fn reply(self, tested_at: String) -> Response {
        reply(
            StatusCode::OK,
            json!({
                "ok":self.ok,
                "status":self.status,
                "statusText":self.status_text,
                "error":self.error,
                "elapsedMs":self.elapsed_ms,
                "testedAt":tested_at,
            }),
            false,
        )
    }
}

pub(super) async fn test(State(state): State<StateData>, Path(id): Path<String>) -> Response {
    let id_for_read = id.clone();
    let pool = match state
        .storage
        .run(false, move |db| by_id(db, &id_for_read))
        .await
    {
        Ok(Some(pool)) => pool,
        Ok(None) => return missing(),
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to test proxy pool",
            );
        }
    };
    let url = pool
        .get("proxyUrl")
        .filter(|value| !value.is_null())
        .map(js_text)
        .unwrap_or_default();
    let test_result = if pool["type"].as_str().is_some_and(relay) {
        relay_probe(&url, &pool).await
    } else {
        proxy_probe(&url).await
    };

    let tested_at = now();
    let timestamp = tested_at.clone();
    let ok = test_result.ok;
    let last_error = test_result.problem();
    let saved = state
        .storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(mut current) = by_id(&tx, &id)?
                && current["proxyUrl"] == pool["proxyUrl"]
                && current["type"] == pool["type"]
                && super::managed_relay::PRIVATE_FIELDS
                    .iter()
                    .all(|name| current[*name] == pool[*name])
            {
                current["testStatus"] = json!(if ok { "active" } else { "error" });
                current["lastTestedAt"] = json!(timestamp);
                current["lastError"] = json!(last_error);
                current["isActive"] = json!(ok);
                current["updatedAt"] = json!(now());
                persist(&tx, &current)?;
            }
            tx.commit()
        })
        .await;
    if saved.is_err() {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to test proxy pool",
        );
    }
    test_result.reply(tested_at)
}

async fn proxy_probe(address: &str) -> Probe {
    proxy_probe_to(address, PROBE_URL, Duration::from_secs(8)).await
}

pub(crate) async fn connection_problem(
    storage: &Storage,
    connection: &Value,
) -> Result<Option<String>, String> {
    let details = &connection["providerSpecificData"];
    let pool_id = js_text(&details["proxyPoolId"]);
    let pool_id = js_trim(&pool_id);
    if !matches!(pool_id, "" | "__none__") && truthy(&details["proxyPoolId"]) {
        let pool_id = pool_id.to_owned();
        if let Some(pool) = storage.run(false, move |db| by_id(db, &pool_id)).await?
            && pool["isActive"] == true
            && let Some(url) = pool["proxyUrl"]
                .as_str()
                .map(js_trim)
                .filter(|url| !url.is_empty())
        {
            if super::managed_relay::managed(&pool) || pool["type"].as_str().is_some_and(relay) {
                return Ok(None);
            }
            return Ok(proxy_probe(url).await.problem());
        }
    }
    if details["connectionProxyEnabled"] == true && truthy(&details["connectionProxyUrl"]) {
        let url = js_text(&details["connectionProxyUrl"]);
        if !js_trim(&url).is_empty() {
            return Ok(proxy_probe(&url).await.problem());
        }
    }
    Ok(None)
}

pub(crate) async fn settings_test(request: Request) -> Response {
    let Ok(bytes) = to_bytes(request.into_body(), 1024 * 1024).await else {
        return settings_error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return settings_error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    let target = body
        .get("testUrl")
        .filter(|value| !value.is_null())
        .map(js_text)
        .unwrap_or_default();
    let target = match js_trim(&target) {
        "" => PROBE_URL,
        url => url,
    };
    let timeout = body["timeoutMs"]
        .as_f64()
        .or_else(|| body["timeoutMs"].as_str()?.trim().parse().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .map_or(8000, |value| value.floor().clamp(1.0, 30_000.0) as u64);
    let proxy = body
        .get("proxyUrl")
        .filter(|value| !value.is_null())
        .map(js_text)
        .unwrap_or_default();
    let result = proxy_probe_to(&proxy, target, Duration::from_millis(timeout)).await;
    if !result.ok {
        let status =
            StatusCode::from_u16(result.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return settings_error(status, result.error.unwrap_or("Proxy test failed"));
    }
    reply(
        StatusCode::OK,
        json!({
            "ok":true,"status":result.status,"statusText":result.status_text.unwrap_or_default(),
            "url":target,"elapsedMs":result.elapsed_ms,
        }),
        false,
    )
}

fn settings_error(status: StatusCode, error: &str) -> Response {
    reply(status, json!({"ok":false,"error":error}), false)
}

async fn proxy_probe_to(address: &str, target: &str, timeout: Duration) -> Probe {
    let address = js_trim(address);
    if address.is_empty() {
        return Probe::failed(400, "proxyUrl is required");
    }
    let Ok(url) = Url::parse(address) else {
        return Probe::failed(400, "Invalid proxy URL: Invalid URL");
    };
    if !matches!(
        url.scheme(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    ) {
        return Probe::failed(
            400,
            "Invalid proxy URL: Invalid URL protocol: the URL must start with http: or https:",
        );
    }
    let Ok(proxy) = reqwest::Proxy::all(url) else {
        return Probe::failed(400, "Invalid proxy URL: Invalid URL");
    };
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .proxy(proxy)
        .redirect(crate::upstream::redirects())
        .timeout(timeout)
        .build()
    else {
        return Probe::failed(500, "Proxy connection failed");
    };
    let target = if target == PROBE_URL
        && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
    {
        FIXTURE_URL
    } else {
        target
    };
    if !Url::parse(target).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
    }) {
        return Probe::failed(500, "Invalid test URL");
    }
    send(client.head(target).header("User-Agent", "9Router"), false).await
}

async fn relay_probe(address: &str, pool: &Value) -> Probe {
    let destination = Url::parse("https://httpbin.org/get").expect("relay probe destination");
    let Ok(route) = pool_relay(pool, address, &destination) else {
        return Probe::failed(400, "Invalid managed relay configuration");
    };
    let Route::Relay { url, .. } = &route else {
        return Probe::failed(500, "Relay fetch failed");
    };
    let timeout = Duration::from_secs(10);
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .redirect(crate::upstream::redirects())
        .timeout(timeout)
        .build()
    else {
        return Probe::failed(500, "Relay fetch failed");
    };
    send(route.relay(client.get(url).timeout(timeout)), true).await
}

async fn send(request: reqwest::RequestBuilder, relay: bool) -> Probe {
    let start = Instant::now();
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            Probe {
                ok: status.is_success(),
                status: status.as_u16(),
                status_text: status.canonical_reason(),
                error: None,
                elapsed_ms: start.elapsed().as_millis() as u64,
            }
        }
        Err(error) => {
            let message = match (relay, error.is_timeout()) {
                (true, true) => "Relay test timed out",
                (true, false) => "Relay fetch failed",
                (false, true) => "Proxy test timed out",
                (false, false) => "Proxy connection failed",
            };
            Probe::failed(500, message)
        }
    }
}
