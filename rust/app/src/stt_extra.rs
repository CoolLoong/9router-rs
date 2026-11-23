use std::time::Duration;

use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use reqwest::multipart::Form;
use reqwest::{Client, Method, RequestBuilder, header};
use serde_json::{Value, json};
use url::Url;

use super::{
    Adapter, Audio, Fields, TranscriptionError, first_text, fixture, multipart_file,
    protected_client, raw_text_response, saved_base, upstream_error,
};
use crate::proxy_transport::{self, Route, Selection};
use crate::storage::Storage;
use crate::{chat_output::json_response, provider_connections::truthy, provider_index::js_trim};

const FIXTURE: &str = "http://127.0.0.1:52989";
const ASSEMBLY_ORIGIN: &str = "https://api.assemblyai.com";
const UPLOAD_PATH: &str = "/v2/upload";
pub(super) const ASSEMBLY_TRANSCRIPT: &str = "https://api.assemblyai.com/v2/transcript";
pub(super) const ASSEMBLY_TRANSCRIPT_FIXTURE: &str =
    "http://127.0.0.1:52989/assemblyai/v2/transcript";
const RESPONSE_LIMIT: usize = 32 * 1024 * 1024;
const ASSEMBLY_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) enum AssemblyFailure {
    BeforeSubmit(TranscriptionError),
    PossiblySubmitted(TranscriptionError),
}

pub(super) fn elevenlabs_form(
    file: &Audio,
    model: &str,
    fields: &Fields,
    renderings: bool,
) -> Result<Form, TranscriptionError> {
    let mut form = multipart_file(file, "model_id", model)?;
    let read = |name| {
        first_text(fields, name)
            .map(js_trim)
            .filter(|value| !value.is_empty())
    };
    if let Some(language) = read("language") {
        form = form.text("language_code", language.to_owned());
    }
    if let Some(granularity @ ("word" | "character" | "none")) = read("timestamps_granularity") {
        form = form.text("timestamps_granularity", granularity.to_owned());
    }
    if read("tag_audio_events") == Some("true") {
        form = form.text("tag_audio_events", "true");
    }
    if read("diarize") == Some("true") {
        form = form.text("diarize", "true");
    } else if let Some(count) = read("num_speakers")
        .and_then(|value| crate::thinking_intent::numeric(&json!(value)))
        .filter(|count| count.fract() == 0.0 && (1.0..=32.0).contains(count))
    {
        form = form.text("num_speakers", (count as u8).to_string());
    }
    if let Some(extra) = elevenlabs_extra(&elevenlabs_format(fields), renderings) {
        form = form.text("additional_formats", json!([{"format":extra}]).to_string());
    }
    Ok(form)
}

fn elevenlabs_format(fields: &Fields) -> String {
    first_text(fields, "response_format")
        .map(js_trim)
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn elevenlabs_extra(format: &str, renderings: bool) -> Option<&'static str> {
    match (format, renderings) {
        ("srt", true) => Some("srt"),
        ("vtt", true) => Some("vtt"),
        ("verbose_json", true) => Some("seg_json"),
        _ => None,
    }
}

pub(super) fn elevenlabs_output(data: Value, fields: &Fields, renderings: bool) -> Response {
    let format = elevenlabs_format(fields);
    let plain = data["text"].as_str().unwrap_or_default();
    let extra = elevenlabs_extra(&format, renderings).and_then(|extra| {
        data["additional_formats"]
            .as_array()?
            .iter()
            .find(|entry| entry["format"] == extra)?["content"]
            .as_str()
    });
    if format == "text" || renderings && matches!(format.as_str(), "srt" | "vtt") {
        return raw_text_response(
            extra.unwrap_or(plain).to_owned(),
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
    }
    let mut output = json!({"text":plain});
    if format == "verbose_json" {
        output["language"] = data["language_code"].clone();
        for key in ["language_probability", "words", "num_speakers"] {
            if let Some(value) = data.get(key).filter(|value| {
                if key == "words" {
                    value.is_array()
                } else {
                    !value.is_null()
                }
            }) {
                output[key] = value.clone();
            }
        }
        if let Some(segments) = extra
            .and_then(|value| serde_json::from_str::<Value>(value).ok())
            .filter(truthy)
        {
            output["segments"] = segments;
        }
    }
    json_response(StatusCode::OK, output, None)
}

pub(super) async fn assembly(
    client: &Client,
    submit_client: &Client,
    endpoint: &str,
    adapter: Adapter,
    file: &Audio,
    model: &str,
    token: &str,
) -> Result<Response, AssemblyFailure> {
    use AssemblyFailure::{BeforeSubmit, PossiblySubmitted};
    let (auth, key) = adapter.auth.header(token).map_err(BeforeSubmit)?;
    let submit = transcript_endpoint(endpoint).map_err(BeforeSubmit)?;
    let upload = if adapter.endpoint == ASSEMBLY_TRANSCRIPT {
        if fixture() {
            format!("{FIXTURE}/assemblyai{UPLOAD_PATH}")
        } else {
            format!("{ASSEMBLY_ORIGIN}{UPLOAD_PATH}")
        }
    } else {
        format!("{}{UPLOAD_PATH}", submit.origin().ascii_serialization())
    };
    let uploaded = fetch(
        client
            .post(upload)
            .header(auth.clone(), key.clone())
            .header("content-type", "application/octet-stream")
            .body(file.bytes.clone()),
        token,
    )
    .await
    .map_err(BeforeSubmit)?;
    let mut body = json!({"speech_models":[model],"language_detection":true});
    if let Some(audio) = uploaded.get("upload_url") {
        body["audio_url"] = audio.clone();
    }
    let response = submit_client
        .post(submit.clone())
        .header(auth.clone(), key.clone())
        .json(&body)
        .send()
        .await
        .map_err(|_| PossiblySubmitted(TranscriptionError::upstream("fetch failed")))?;
    let submit_status = response.status();
    if !submit_status.is_success() {
        let text = response
            .text()
            .await
            .map_err(|_| PossiblySubmitted(TranscriptionError::upstream("fetch failed")))?;
        let value: Value = serde_json::from_str(&text).unwrap_or_default();
        let may_be_accepted = ["id", "transcript_id", "taskId"]
            .into_iter()
            .any(|field| truthy(&value[field]))
            || matches!(
                value["status"].as_str(),
                Some("queued" | "processing" | "completed")
            );
        let mut failure = upstream_error(submit_status, &text);
        failure.message = failure.message.replace(token, "[redacted]");
        return Err(
            if !may_be_accepted
                && matches!(
                    submit_status.as_u16(),
                    400 | 401 | 402 | 403 | 404 | 413 | 422 | 429
                )
            {
                BeforeSubmit(failure)
            } else {
                PossiblySubmitted(failure)
            },
        );
    }
    let submitted: Value = response.json().await.map_err(|_| {
        PossiblySubmitted(TranscriptionError::upstream(
            "Invalid JSON response from STT provider",
        ))
    })?;
    let request_id = submitted["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            PossiblySubmitted(TranscriptionError::upstream(
                "AssemblyAI returned no transcript id",
            ))
        })?;
    let mut status = submit;
    status
        .path_segments_mut()
        .map_err(|_| PossiblySubmitted(TranscriptionError::upstream("Invalid URL")))?
        .pop_if_empty()
        .push(request_id);
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let poll = client
                .get(status.clone())
                .header(auth.clone(), key.clone())
                .send()
                .await
                .map_err(|_| TranscriptionError::upstream("fetch failed"))?;
            if !poll.status().is_success() {
                continue;
            }
            let data = poll.json::<Value>().await.map_err(|_| {
                TranscriptionError::upstream("Invalid JSON response from STT provider")
            })?;
            match data["status"].as_str() {
                Some("completed") => {
                    let text = data
                        .get("text")
                        .filter(|value| truthy(value))
                        .cloned()
                        .unwrap_or_else(|| json!(""));
                    return Ok(json_response(StatusCode::OK, json!({"text":text}), None));
                }
                Some("error") => {
                    return Err(TranscriptionError {
                        status: StatusCode::INTERNAL_SERVER_ERROR,
                        message: data["error"]
                            .as_str()
                            .filter(|error| !error.is_empty())
                            .unwrap_or("AssemblyAI failed")
                            .replace(token, "[redacted]"),
                    });
                }
                _ => (),
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        Err(TranscriptionError {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: "AssemblyAI timeout after 120s".into(),
        })
    })
    .map_err(PossiblySubmitted)
}

enum AssemblyDestination {
    Vendor(String),
    Relay {
        url: String,
        authorization: HeaderValue,
    },
}

struct AssemblyDelivery {
    client: Client,
    destination: AssemblyDestination,
    origin: String,
    submit_path: String,
    upload_path: String,
}

impl AssemblyDelivery {
    async fn select(
        storage: &Storage,
        settings: &Value,
        account: &Value,
        direct: &Client,
        adapter: Adapter,
    ) -> Result<Self, TranscriptionError> {
        let unavailable = |message: &'static str| TranscriptionError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        };
        let message = "AssemblyAI connection proxy is unavailable";
        let configured = saved_base(account);
        let endpoint = transcript_endpoint(configured.unwrap_or(adapter.endpoint))?;
        if endpoint.query().is_some() {
            return Err(unavailable(
                "Protected AssemblyAI transcript endpoints require a URL without a query",
            ));
        }
        let origin = endpoint.origin().ascii_serialization();
        let path = endpoint.path();
        let route = crate::provider_http::select(storage, settings, account, endpoint.as_str())
            .await
            .map_err(|_| unavailable(message))?;
        let (client, destination, submit_path, upload_path) = match route {
            Selection::ExplicitBypass => {
                let use_assembly_fixture = fixture() && endpoint.as_str() == ASSEMBLY_TRANSCRIPT;
                let local = transcript_endpoint(if use_assembly_fixture {
                    ASSEMBLY_TRANSCRIPT_FIXTURE
                } else {
                    configured.unwrap_or(adapter.direct_endpoint())
                })?;
                let mut upload_path = UPLOAD_PATH.to_owned();
                if use_assembly_fixture {
                    upload_path = "/assemblyai/v2/upload".into();
                }
                (
                    direct.clone(),
                    AssemblyDestination::Vendor(local.origin().ascii_serialization()),
                    local.path().to_owned(),
                    upload_path,
                )
            }
            Selection::Routed(Route::Proxy { url, .. }) if endpoint.scheme() == "https" => {
                let proxy =
                    proxy_transport::tunnel_proxy_url(&url).ok_or_else(|| unavailable(message))?;
                let client = proxy_transport::vendor_metadata_client(proxy, protected_client())
                    .ok_or_else(|| unavailable(message))?;
                (
                    client,
                    AssemblyDestination::Vendor(origin.clone()),
                    path.to_owned(),
                    UPLOAD_PATH.into(),
                )
            }
            Selection::Routed(Route::Relay {
                url,
                authorization: Some(authorization),
                ..
            }) => (
                direct.clone(),
                AssemblyDestination::Relay { url, authorization },
                path.to_owned(),
                UPLOAD_PATH.into(),
            ),
            Selection::Routed(Route::Relay { .. }) => {
                return Err(unavailable("AssemblyAI requires a validated Relay pool"));
            }
            _ => return Err(unavailable(message)),
        };
        Ok(Self {
            client,
            destination,
            origin,
            submit_path,
            upload_path,
        })
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        auth: &(header::HeaderName, HeaderValue),
    ) -> RequestBuilder {
        match &self.destination {
            AssemblyDestination::Vendor(origin) => self
                .client
                .request(method, format!("{origin}{path}"))
                .timeout(ASSEMBLY_TIMEOUT)
                .header(auth.0.clone(), auth.1.clone()),
            AssemblyDestination::Relay { url, authorization } => Route::Relay {
                url: url.clone(),
                target: self.origin.clone(),
                path: path.into(),
                authorization: Some(authorization.clone()),
            }
            .relay(
                self.client
                    .request(method, url)
                    .timeout(ASSEMBLY_TIMEOUT)
                    .header(auth.0.clone(), auth.1.clone()),
            ),
        }
    }

    fn relay_key(&self) -> Option<&str> {
        match &self.destination {
            AssemblyDestination::Vendor(_) => None,
            AssemblyDestination::Relay { authorization, .. } => authorization.to_str().ok(),
        }
    }

    fn contains_secret(&self, value: &str, key: &str) -> bool {
        value.contains(key) || self.relay_key().is_some_and(|relay| value.contains(relay))
    }

    fn redact(&self, value: String, key: &str) -> String {
        let mut secrets = [key, self.relay_key().unwrap_or_default()];
        secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets
            .into_iter()
            .filter(|secret| !secret.is_empty())
            .fold(value, |text, secret| text.replace(secret, "[redacted]"))
    }

    async fn fetch(&self, request: RequestBuilder, key: &str) -> Result<Value, TranscriptionError> {
        let mut response = request
            .send()
            .await
            .map_err(|_| TranscriptionError::upstream("AssemblyAI request failed"))?;
        let status = response.status();
        if status.is_redirection() {
            return Err(TranscriptionError::upstream("AssemblyAI redirect refused"));
        }
        let invalid = || TranscriptionError::upstream("Invalid JSON response from STT provider");
        let limit = if status.is_success() {
            RESPONSE_LIMIT
        } else {
            64 * 1024
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| invalid())? {
            if chunk.len() > limit - bytes.len() {
                return Err(invalid());
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let mut failure = upstream_error(status, &String::from_utf8_lossy(&bytes));
            failure.message = self.redact(failure.message, key);
            return Err(failure);
        }
        serde_json::from_slice(&bytes).map_err(|_| invalid())
    }
}

pub(super) async fn protected_assembly(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    direct: &Client,
    adapter: Adapter,
    file: &Audio,
    model: &str,
    token: &str,
) -> Result<Response, TranscriptionError> {
    let invalid = || TranscriptionError {
        status: StatusCode::UNAUTHORIZED,
        message: "Invalid AssemblyAI API key".into(),
    };
    if token.len() > 512 || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(invalid());
    }
    let auth = adapter.auth.header(token).map_err(|_| invalid())?;
    let delivery = AssemblyDelivery::select(storage, settings, account, direct, adapter).await?;
    let path = percent_encoding::percent_decode_str(&delivery.submit_path).decode_utf8_lossy();
    if delivery.contains_secret(&path, token) {
        return Err(TranscriptionError {
            status: StatusCode::BAD_REQUEST,
            message: "AssemblyAI transcript endpoint contains a credential".into(),
        });
    }
    let request = delivery
        .request(Method::POST, &delivery.upload_path, &auth)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(file.bytes.clone());
    let uploaded = delivery.fetch(request, token).await?;
    let upload = uploaded["upload_url"]
        .as_str()
        .filter(|value| value.len() <= 1024 && !delivery.contains_secret(value, token))
        .and_then(|raw| Url::parse(raw).ok())
        .filter(|url| {
            url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && if delivery.origin == ASSEMBLY_ORIGIN {
                    url.scheme() == "https"
                        && url.host_str() == Some("cdn.assemblyai.com")
                        && url.port().is_none()
                        && url.query().is_none()
                        && url.path().strip_prefix("/upload/").is_some_and(safe_id)
                } else {
                    url.scheme() == "https" || url.origin().ascii_serialization() == delivery.origin
                }
        })
        .ok_or_else(|| TranscriptionError::upstream("Invalid AssemblyAI upload URL"))?;
    let request = delivery
        .request(Method::POST, &delivery.submit_path, &auth)
        .json(
            &json!({"audio_url":upload.as_str(),"speech_models":[model],"language_detection":true}),
        );
    let submitted = delivery.fetch(request, token).await?;
    let id = submitted["id"]
        .as_str()
        .filter(|id| safe_id(id) && !delivery.contains_secret(id, token))
        .ok_or_else(|| TranscriptionError::upstream("Invalid AssemblyAI transcript id"))?;
    let path = format!("{}/{id}", delivery.submit_path.trim_end_matches('/'));
    let (interval, timeout) = if fixture() {
        (Duration::from_millis(20), Duration::from_millis(500))
    } else {
        (Duration::from_secs(2), ASSEMBLY_TIMEOUT)
    };
    tokio::time::timeout(timeout, async {
        loop {
            tokio::time::sleep(interval).await;
            let result = delivery
                .fetch(delivery.request(Method::GET, &path, &auth), token)
                .await?;
            if result["id"] != id {
                return Err(TranscriptionError::upstream(
                    "AssemblyAI returned a different transcript",
                ));
            }
            match result["status"].as_str() {
                Some("completed") => {
                    let text = delivery.redact(
                        result["text"].as_str().unwrap_or_default().to_owned(),
                        token,
                    );
                    return Ok(json_response(StatusCode::OK, json!({"text":text}), None));
                }
                Some("error") => {
                    let error = result["error"]
                        .as_str()
                        .filter(|error| !error.is_empty())
                        .unwrap_or("AssemblyAI failed");
                    return Err(TranscriptionError {
                        status: StatusCode::INTERNAL_SERVER_ERROR,
                        message: delivery.redact(error.to_owned(), token),
                    });
                }
                Some("queued" | "processing") => (),
                _ => {
                    return Err(TranscriptionError::upstream(
                        "Invalid AssemblyAI transcript status",
                    ));
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        Err(TranscriptionError {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: "AssemblyAI timeout after 120s".into(),
        })
    })
}

fn transcript_endpoint(endpoint: &str) -> Result<Url, TranscriptionError> {
    Url::parse(endpoint)
        .ok()
        .filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
        })
        .ok_or_else(|| TranscriptionError {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid AssemblyAI transcript endpoint".into(),
        })
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

async fn fetch(request: RequestBuilder, token: &str) -> Result<Value, TranscriptionError> {
    let response = request
        .send()
        .await
        .map_err(|_| TranscriptionError::upstream("fetch failed"))?;
    let status = response.status();
    if !status.is_success() {
        let mut failure = super::upstream_failure(response).await;
        failure.message = failure.message.replace(token, "[redacted]");
        return Err(failure);
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| TranscriptionError::upstream("Invalid JSON response from STT provider"))
}
