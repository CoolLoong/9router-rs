use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{Method, StatusCode, Uri};
use axum::{Json, Router};
use futures_util::{StreamExt, stream};
use reqwest::Client;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const CHAT_PATH: &str = "/v1/chat/completions";
const GATEWAY_KEY: &str = "fixture-benchmark-gateway-key";
const MODEL: &str = "bench-echo";
const PROVIDER_PREFIX: &str = "bench-fixture";

pub fn seed(upstream: SocketAddr) -> Value {
    let provider = "openai-compatible-chat-bench-fixture";
    let api = format!("http://{upstream}/v1");
    json!({
        "settings": {
            "requireApiKey": true,
            "requireLogin": false,
            "enableRequestLogs": false
        },
        "providerNodes": [{
            "id": provider,
            "type": "openai-compatible",
            "name": "Benchmark fixture",
            "prefix": PROVIDER_PREFIX,
            "apiType": "chat",
            "baseUrl": api
        }],
        "providerConnections": [{
            "id": "bench-connection",
            "provider": provider,
            "authType": "apikey",
            "name": "Benchmark fixture",
            "apiKey": "fixture-benchmark-provider-key",
            "priority": 1,
            "isActive": true,
            "providerSpecificData": {
                "prefix": PROVIDER_PREFIX,
                "apiType": "chat",
                "baseUrl": api
            }
        }],
        "apiKeys": [{
            "id": "bench-key",
            "name": "Benchmark fixture",
            "key": GATEWAY_KEY,
            "isActive": true
        }]
    })
}

#[derive(Clone, Default)]
pub struct MockCalls(Arc<AtomicUsize>);

impl MockCalls {
    pub fn count(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    pub fn router(&self) -> Router {
        Router::new().fallback(echo).with_state(self.clone())
    }
}

async fn echo(
    State(calls): State<MockCalls>,
    method: Method,
    uri: Uri,
    request: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, StatusCode> {
    calls.0.fetch_add(1, Ordering::Relaxed);
    if method != Method::POST || uri.path() != CHAT_PATH {
        return Err(StatusCode::NOT_FOUND);
    }
    let Json(request) = request.map_err(|_| StatusCode::BAD_REQUEST)?;
    if request["model"] != MODEL {
        return Err(StatusCode::BAD_REQUEST);
    }
    let input = request["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_str())
        .ok_or(StatusCode::BAD_REQUEST)?;
    Ok(Json(json!({
        "id": "bench",
        "object": "chat.completion",
        "created": 123456,
        "model": MODEL,
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": format!("echo: {input}")}
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })))
}

pub struct Requests {
    pub latencies_ms: Vec<f64>,
    pub errors: usize,
    pub first_error: Option<String>,
}

pub async fn send(
    client: &Client,
    gateway: SocketAddr,
    label: &str,
    requests: usize,
    concurrency: usize,
) -> Requests {
    let url = format!("http://{gateway}{CHAT_PATH}");
    let mut pending = stream::iter(0..requests)
        .map(|index| chat(client, &url, format!("{label}-{index:08}")))
        .buffer_unordered(concurrency);
    let mut results = Requests {
        latencies_ms: Vec::with_capacity(requests),
        errors: 0,
        first_error: None,
    };
    while let Some(result) = pending.next().await {
        match result {
            Ok(latency) => results.latencies_ms.push(latency.as_secs_f64() * 1000.0),
            Err(error) => {
                results.errors += 1;
                results.first_error.get_or_insert(error);
            }
        }
    }
    results
}

async fn chat(client: &Client, url: &str, input: String) -> Result<Duration, String> {
    let request = client.post(url).bearer_auth(GATEWAY_KEY).json(&json!({
        "model": format!("{PROVIDER_PREFIX}/{MODEL}"),
        "messages": [{"role": "user", "content": input}],
        "stream": false
    }));
    let started = Instant::now();
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response.bytes().await.map_err(|error| error.to_string())?;
    let latency = started.elapsed();
    if status != StatusCode::OK {
        let snippet = String::from_utf8_lossy(&body[..body.len().min(300)]);
        return Err(format!("{input} returned HTTP {status}: {snippet}"));
    }
    let body: Value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
    let expected = format!("echo: {input}");
    if body["choices"][0]["message"]["content"].as_str() != Some(expected.as_str()) {
        return Err(format!("{input} returned a different assistant message"));
    }
    Ok(latency)
}
