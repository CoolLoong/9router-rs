use std::time::Duration;

use axum::http::{StatusCode, header};
use axum::response::Response;
use reqwest::{Client, Response as Upstream};
use serde_json::{Value, json};
use url::Url;

use super::{
    SpeechInput, SpeechState, SynthesisError, audio_bytes, js_text, proxy_required, speech_request,
    truthy,
};

const DEFAULT_MODEL: &str = "phonon:multilingual";
const DEFAULT_VOICE: &str = "default";
const TAGS_LIMIT: usize = 1024 * 1024;
const TAGS_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn host(account: &Value) -> Option<Url> {
    let raw = crate::ollama_local::host(account)?;
    let raw = raw.trim_end_matches('/');
    let raw = raw.strip_suffix("/v1/audio/speech").unwrap_or(raw);
    let raw = raw.strip_suffix("/v1").unwrap_or(raw);
    let root = Url::parse(raw).ok()?;
    if !matches!(root.scheme(), "http" | "https")
        || !root.has_host()
        || !root.username().is_empty()
        || root.password().is_some()
        || root.query().is_some()
        || root.fragment().is_some()
    {
        return None;
    }
    Some(root)
}

pub(super) async fn synthesize(
    state: &SpeechState,
    settings: &Value,
    account: &Value,
    model: &str,
    text: &SpeechInput<'_>,
    key: Option<&str>,
) -> Result<Response, SynthesisError> {
    let root = host(account).ok_or_else(|| {
        if proxy_required(settings, account, true) || proxy_required(settings, account, false) {
            SynthesisError::proxy("Invalid protected Ollama host")
        } else {
            SynthesisError::upstream("Invalid Ollama host")
        }
    })?;
    let host = root.as_str().trim_end_matches('/');
    let endpoint = format!("{host}/v1/audio/speech");
    let route = speech_request(state, settings, account, &endpoint, &endpoint, false).await?;
    if route.protected && key.is_some_and(|key| !key.bytes().all(|byte| byte.is_ascii_graphic())) {
        return Err(SynthesisError::proxy("Invalid Ollama provider API key"));
    }
    let (client, request) = route.request.build_split();
    request.map_err(|_| {
        SynthesisError::upstream("Invalid Ollama request").protect(route.protected, key, None)
    })?;
    let upstream = Ollama {
        client,
        endpoint,
        host,
        key,
        protected: route.protected,
    };
    upstream.generate(model, text).await.map_err(|failure| {
        let mut failure = failure.protect(true, key, None);
        failure.protected = route.protected;
        failure
    })
}

struct Ollama<'a> {
    client: Client,
    endpoint: String,
    host: &'a str,
    key: Option<&'a str>,
    protected: bool,
}

impl Ollama<'_> {
    async fn generate(
        &self,
        model: &str,
        text: &SpeechInput<'_>,
    ) -> Result<Response, SynthesisError> {
        let (default_model, voice) = model_voice(model);
        let format = if text.response_format == "pcm" {
            "pcm"
        } else {
            "wav"
        };
        let mut active_model = default_model.to_owned();
        let mut response = self.post(text.text, &active_model, &voice, format).await?;
        if response.status() == StatusCode::NOT_FOUND
            && active_model.starts_with("phonon")
            && let Some(alternate) = self.alternate(&active_model).await
        {
            active_model = alternate;
            response = self.post(text.text, &active_model, &voice, format).await?;
        }
        let status = response.status();
        if status.is_success() {
            return audio(response, text.response_format).await;
        }
        let message = response.text().await.unwrap_or_default();
        if status == StatusCode::BAD_REQUEST
            && voice != DEFAULT_VOICE
            && message.contains("voice")
            && message.contains("not found")
        {
            let retry = self
                .post(text.text, &active_model, DEFAULT_VOICE, format)
                .await?;
            if retry.status().is_success() {
                return audio(retry, text.response_format).await;
            }
        }
        Err(provider_error(status, message))
    }

    async fn post(
        &self,
        text: &str,
        model: &str,
        voice: &str,
        format: &str,
    ) -> Result<Upstream, SynthesisError> {
        let mut request = self
            .client
            .post(&self.endpoint)
            .json(&json!({"model":model,"input":text,"voice":voice,"response_format":format}));
        if let Some(key) = self.key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.map_err(|_| SynthesisError::upstream(format!(
            "Cannot connect to Ollama at {}. Please ensure Ollama is running (e.g. 'ollama serve').",
            self.host
        )))?;
        if self.protected && response.status().is_redirection() {
            return Err(SynthesisError::upstream("Speech upstream redirect refused"));
        }
        Ok(response)
    }

    async fn alternate(&self, model: &str) -> Option<String> {
        let discovered = self.find_alternate(model).await;
        discovered.or_else(|| match model {
            "phonon" => Some(DEFAULT_MODEL.to_owned()),
            DEFAULT_MODEL => Some("phonon:latest".to_owned()),
            _ => None,
        })
    }

    async fn find_alternate(&self, model: &str) -> Option<String> {
        let mut response = self
            .client
            .get(format!("{}/api/tags", self.host))
            .timeout(TAGS_TIMEOUT)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if chunk.len() > TAGS_LIMIT - bytes.len() {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        let data: Value = serde_json::from_slice(&bytes).ok()?;
        data["models"]
            .as_array()?
            .iter()
            .filter_map(|item| {
                let name = if truthy(&item["name"]) {
                    &item["name"]
                } else {
                    &item["model"]
                };
                name.as_str()
            })
            .find(|tag| tag.starts_with("phonon") && *tag != model)
            .map(str::to_owned)
    }
}

fn model_voice(model: &str) -> (&str, String) {
    let mut parts = model.split('/').filter(|part| !part.is_empty());
    let Some(first) = parts.next() else {
        return (DEFAULT_MODEL, DEFAULT_VOICE.to_owned());
    };
    let voice = parts.collect::<Vec<_>>().join("/");
    if !voice.is_empty() {
        (first, voice)
    } else if first.starts_with("phonon") {
        (first, DEFAULT_VOICE.to_owned())
    } else {
        (DEFAULT_MODEL, first.to_owned())
    }
}

fn provider_error(status: StatusCode, raw: String) -> SynthesisError {
    let json: Value = serde_json::from_str(&raw).unwrap_or_default();
    let detail = [
        Some(&json["error"]["message"]),
        Some(&json["message"]),
        Some(&json["detail"]["message"]),
        json["detail"].is_string().then_some(&json["detail"]),
    ]
    .into_iter()
    .flatten()
    .find(|value| truthy(value))
    .map(js_text)
    .unwrap_or_else(|| {
        if raw.is_empty() {
            format!("Upstream error ({})", status.as_u16())
        } else {
            raw
        }
    });
    SynthesisError::upstream(detail)
}

async fn audio(response: Upstream, requested: &str) -> Result<Response, SynthesisError> {
    let mime = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let format = if mime.contains("wav") {
        "wav"
    } else if mime.contains("mpeg") || mime.contains("mp3") {
        "mp3"
    } else if mime.contains("ogg") {
        "ogg"
    } else if mime.contains("pcm") {
        "pcm"
    } else if requested.is_empty() {
        "wav"
    } else {
        requested
    };
    let bytes = response
        .bytes()
        .await
        .map_err(|_| SynthesisError::upstream("terminated"))?;
    if bytes.len() < 100 {
        return Err(SynthesisError::upstream("Upstream returned empty audio"));
    }
    audio_bytes(bytes, format, requested)
}
