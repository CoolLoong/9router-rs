use std::sync::Arc;
use std::time::Instant;

use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};

use crate::chat_output::js_text;
use crate::key_identity;
use crate::openai_models;
use crate::provider_index::{Compatible, compatible, js_trim};
use crate::storage::{Storage, is_active};

use super::{ProviderTools, account, discover, error, missing, redact, reply, secrets, truthy};

pub(super) async fn model(State(state): State<ProviderTools>, request: Request) -> Response {
    let Ok(bytes) = to_bytes(request.into_body(), 128 * 1024 * 1024).await else {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    };
    if body.is_null() {
        return error(StatusCode::BAD_REQUEST, "Invalid JSON body");
    }
    if !truthy(&body["model"]) {
        return error(StatusCode::BAD_REQUEST, "Model required");
    }
    let Some(target) = body["model"].as_str() else {
        return error(StatusCode::BAD_REQUEST, "Invalid model format");
    };
    let kind = body["kind"].as_str().unwrap_or("llm");
    let credentials = Credentials::load(&state.storage).await;
    match ping(&state, target, kind, &credentials).await {
        Ok(value) => reply(StatusCode::OK, value),
        Err(text) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"ok":false,"error":text}),
        ),
    }
}

pub(super) async fn models(State(state): State<ProviderTools>, Path(id): Path<String>) -> Response {
    let connection = match account(&state, id.clone()).await {
        Ok(Some(connection)) => connection,
        Ok(None) => return missing(),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Test failed"),
    };
    let provider = connection["provider"].as_str().unwrap_or_default();
    let cline = crate::cline_transport::is_provider(provider);
    let alias = openai_models::data()["transportAlias"][provider]
        .as_str()
        .unwrap_or(provider);
    let mut rows = openai_models::data()["models"][alias]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if matches!(
        compatible(provider),
        Some(Compatible::OpenAi | Compatible::Anthropic)
    ) && rows.is_empty()
    {
        if let Ok(discovered) = discover(&state, &connection).await
            && let Some(models) = discovered.as_array()
        {
            rows = models
                .iter()
                .filter_map(|model| {
                    let id = first(&model["id"], &model["name"]).as_str()?;
                    Some(json!({"id":id,"name":first(&model["name"], &model["id"])}))
                })
                .collect();
        }
    }
    let mut models = rows.into_iter();
    let Some(first) = models.next() else {
        return error(
            StatusCode::BAD_REQUEST,
            "No models configured for this provider",
        );
    };
    let credentials = Arc::new(Credentials::load(&state.storage).await);
    let alias: Arc<str> = alias.into();
    let run_probe = |entry: Value| {
        let state = state.clone();
        let alias = alias.clone();
        let credentials = credentials.clone();
        async move { probe_entry(&state, &alias, &entry, &credentials).await }
    };
    let Ok(warmup) = run_probe(first).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Test failed");
    };
    let mut output = vec![warmup];
    let parallelism = if cline { models.len().max(1) } else { 8 };
    let results = stream::iter(models.map(run_probe))
        .buffered(parallelism)
        .collect::<Vec<_>>()
        .await;
    for result in results {
        match result {
            Ok(result) => output.push(result),
            Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Test failed"),
        }
    }
    reply(
        StatusCode::OK,
        json!({"provider":provider,"connectionId":id,"results":output}),
    )
}

fn model_provider(target: &str) -> &str {
    let prefix = target.split('/').next().unwrap_or_default();
    openai_models::data()["providerByAlias"][prefix]
        .as_str()
        .unwrap_or(prefix)
}

async fn probe_entry(
    state: &ProviderTools,
    alias: &str,
    entry: &Value,
    credentials: &Credentials,
) -> Result<Value, String> {
    let id = entry["id"].as_str().unwrap_or_default();
    let target = format!("{alias}/{id}");
    let mut result = json!({"modelId":id,"name":first(&entry["name"], &entry["id"])});
    let kind = openai_models::model_kind(entry);
    if let Value::Object(ping) = ping(state, &target, kind, credentials).await?
        && let Some(fields) = result.as_object_mut()
    {
        fields.extend(ping);
    }
    Ok(result)
}

fn first<'a>(value: &'a Value, fallback: &'a Value) -> &'a Value {
    if truthy(value) { value } else { fallback }
}

fn saved_secrets(account: &Value) -> Vec<String> {
    let mut private = secrets(account);
    if account["provider"]
        .as_str()
        .is_some_and(crate::cline_transport::is_provider)
        && let Some(token) = account["accessToken"].as_str()
    {
        let trimmed = js_trim(token);
        if !trimmed.is_empty() && trimmed != token {
            private.push(trimmed.to_owned());
        }
    }
    private
}

#[derive(Default)]
struct Credentials {
    api_key: Option<String>,
    cli_token: Option<String>,
    secrets: Vec<String>,
}

impl Credentials {
    async fn load(storage: &Storage) -> Self {
        let mut credentials = storage
            .run(false, |db| {
                let mut statement = db.prepare(
                    "SELECT key,isActive,accessRestricted FROM apiKeys ORDER BY createdAt ASC",
                )?;
                let mut keys = statement.query([])?;
                let mut selected = None;
                let mut unrestricted = None;
                let mut private = Vec::new();
                while let Some(row) = keys.next()? {
                    let key: String = row.get(0)?;
                    if is_active(row.get_ref(1)?) {
                        selected.get_or_insert_with(|| key.clone());
                        if !is_active(row.get_ref(2)?) {
                            unrestricted.get_or_insert_with(|| key.clone());
                        }
                    }
                    if !key.is_empty() {
                        private.push(key);
                    }
                }
                private.extend(
                    crate::provider_connections::read_all(db, None)?
                        .iter()
                        .flat_map(saved_secrets),
                );
                Ok(Self {
                    api_key: unrestricted.or(selected),
                    cli_token: None,
                    secrets: private,
                })
            })
            .await
            .unwrap_or_default();
        credentials.cli_token = key_identity::cli_token(storage.root());
        credentials
    }

    fn apply(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        if let Some(token) = &self.cli_token {
            request = request.header("x-9r-cli-token", token);
        }
        request
    }

    fn redact(&self, text: String) -> String {
        let text = redact(text, &self.secrets);
        if let Some(token) = &self.cli_token {
            text.replace(token, "[redacted]")
        } else {
            text
        }
    }
}

async fn ping(
    state: &ProviderTools,
    model: &str,
    kind: &str,
    credentials: &Credentials,
) -> Result<Value, String> {
    let (kind, path, body) = match kind {
        "embedding" => (
            "embedding",
            "/api/v1/embeddings",
            json!({"model":model,"input":"test"}),
        ),
        "image" => (
            "image",
            "/api/v1/images/generations",
            json!({"model":model,"prompt":"test"}),
        ),
        "stt" => ("stt", "/api/v1/audio/transcriptions", Value::Null),
        "systemone" => (
            "systemone",
            "/api/v1/systemone",
            json!({
                "model":model,"state":"Customer: I was charged twice for my order this morning.",
                "questions":{"probe":{"type":"noul","instructions":"Is the customer reporting a billing problem?"}},
            }),
        ),
        _ => (
            "llm",
            "/api/v1/chat/completions",
            json!({
                "model":model,"max_tokens":1024,"stream":false,"messages":[{"role":"user","content":"hi"}],
            }),
        ),
    };
    let request = credentials.apply(state.http.post(format!("{}{path}", state.gateway)));
    let request = if kind == "stt" {
        let file = reqwest::multipart::Part::bytes(silent_wav())
            .file_name("test.wav")
            .mime_str("audio/wav")
            .map_err(|_| "Could not build transcription fixture")?;
        request.multipart(
            reqwest::multipart::Form::new()
                .part("file", file)
                .text("model", model.to_owned()),
        )
    } else {
        request.json(&body)
    };
    let start = Instant::now();
    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            "The operation was aborted due to timeout"
        } else {
            "fetch failed"
        }
    })?;
    let elapsed = start.elapsed().as_millis() as u64;
    let status = response.status();
    let raw = response.text().await.unwrap_or_default();
    let mut parsed: Value = serde_json::from_str(&raw).unwrap_or_default();
    let provider = model_provider(model);
    let cline = kind == "llm" && crate::cline_transport::is_provider(provider);
    if cline && parsed["success"] == true && parsed["data"].is_object() {
        parsed = parsed["data"].take();
    }
    let current = if cline
        && (!status.is_success()
            || truthy(&parsed["error"])
            || truthy(first(&parsed["msg"], &parsed["message"])))
    {
        let provider = provider.to_owned();
        state
            .storage
            .run(false, move |db| {
                Ok(crate::provider_connections::read_all(db, Some(&provider))?
                    .iter()
                    .flat_map(saved_secrets)
                    .collect::<Vec<_>>())
            })
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let redact_probe = |text| redact(credentials.redact(text), &current);
    let base = |error: Option<String>| json!({"ok":error.is_none(),"latencyMs":elapsed,"error":error,"status":status.as_u16()});

    if !status.is_success() {
        let detail = error_detail(&parsed, &raw, kind);
        let detail = limited(redact_probe(detail), if kind == "llm" { 500 } else { 240 });
        let phrase = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        return Ok(base(Some(format!("HTTP {}{phrase}", status.as_u16()))));
    }

    if kind == "llm" {
        let status_value = &parsed["status"];
        let message = first(&parsed["msg"], &parsed["message"]);
        let symbolic = js_text(status_value);
        if !status_value.is_null() && symbolic != "0" && symbolic != "200" && truthy(message) {
            return Ok(model_error(
                elapsed,
                status,
                format!(
                    "Provider status {}: {}",
                    redact_probe(symbolic),
                    limited(redact_probe(js_text(message)), 240)
                ),
            ));
        }
        if truthy(&parsed["error"]) {
            let text = js_text(first(&parsed["error"]["message"], &parsed["error"]));
            return Ok(model_error(
                elapsed,
                status,
                limited(redact_probe(text), 240),
            ));
        }
        let choice = &parsed["choices"][0];
        let content = &choice["message"]["content"];
        let content_empty = if cline {
            !truthy(content) || js_trim(&js_text(content)).is_empty()
        } else {
            content.as_str().unwrap_or_default().trim().is_empty()
        };
        if choice["finish_reason"] == "length"
            && content_empty
            && [
                "reasoning",
                "reasoning_content",
                "thinking",
                "thinking_content",
            ]
            .iter()
            .any(|field| truthy(&choice["message"][*field]))
        {
            let mut result = base(None);
            result["note"] = json!("reasoning-only response (length-limited)");
            return Ok(result);
        }
    }

    let error = match kind {
        "embedding" if !parsed["data"][0]["embedding"].is_array() => {
            Some("Provider returned no embedding data")
        }
        "image" if parsed["data"].as_array().is_none_or(Vec::is_empty) => {
            Some("Provider returned no image data for this model")
        }
        "stt"
            if parsed["text"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .is_empty() =>
        {
            Some("Provider returned no transcription text for this model")
        }
        "systemone"
            if parsed["answers"]
                .as_object()
                .is_none_or(|answers| answers.is_empty()) =>
        {
            Some("Provider returned no answers for this model")
        }
        "llm" if parsed["choices"].as_array().is_none_or(Vec::is_empty) => {
            Some("Provider returned no completion choices for this model")
        }
        _ => None,
    };
    Ok(match error {
        Some(error) => model_error(elapsed, status, error.into()),
        None => base(None),
    })
}

fn model_error(latency: u64, status: StatusCode, error: String) -> Value {
    json!({"ok":false,"latencyMs":latency,"status":status.as_u16(),"error":error})
}

fn error_detail(parsed: &Value, raw: &str, kind: &str) -> String {
    let keys: &[&Value] = if kind == "embedding" {
        &[&parsed["error"]["message"], &parsed["error"]]
    } else {
        &[
            &parsed["error"]["message"],
            &parsed["msg"],
            &parsed["message"],
            &parsed["error"],
        ]
    };
    keys.iter()
        .find(|value| truthy(value))
        .map_or_else(|| raw.to_owned(), |value| js_text(value))
}

fn limited(text: String, count: usize) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(count).collect::<Vec<_>>())
}

fn silent_wav() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8044);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&8036_u32.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&16000_u32.to_le_bytes());
    bytes.extend_from_slice(&32000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&8000_u32.to_le_bytes());
    bytes.resize(8044, 0);
    bytes
}
