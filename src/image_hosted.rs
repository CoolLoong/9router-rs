use std::sync::LazyLock;
use std::time::Duration;

use axum::http::{StatusCode, header};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Client, Method, RequestBuilder, Response, multipart::Form};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

use super::{
    IMAGE_BASE64, ImageError, QueuedImageError, default_error, proxy_error, proxy_required,
};
use crate::chat_prefetch::{fetch_image_bytes, mime};
use crate::provider_connections::truthy;
use crate::provider_http::{self, Bound};
use crate::provider_index::js_trim;
use crate::proxy_transport::{Route, Selection};
use crate::storage::Storage;

const STABILITY: &str = "https://api.stability.ai/v2beta/stable-image/generate";
const HUGGING_FACE: &str = "https://router.huggingface.co";
const RESPONSE_LIMIT: usize = 32 * 1024 * 1024;
const ERROR_LIMIT: usize = 128 * 1024;
const SOURCE_LIMIT: usize = 20 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(125);
static MODELS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("huggingface-image-models.json"))
        .expect("pinned Hugging Face image registry")
});
static CLIENT: LazyLock<Result<Client, reqwest::Error>> = LazyLock::new(|| {
    Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(20))
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
});

#[derive(Clone, Copy)]
pub(super) enum Adapter {
    Stability,
    HuggingFace,
}

pub(super) enum Request {
    Stability {
        endpoint: &'static str,
        fields: Vec<(&'static str, String)>,
    },
    HuggingFace(HuggingFace),
}

pub(super) struct HuggingFace {
    model: String,
    path: Option<&'static str>,
    prompt: String,
    source: Option<SourceImage>,
}

struct SourceImage {
    base64: String,
    mime: Option<&'static str>,
}

fn invalid(message: impl Into<String>) -> ImageError {
    ImageError {
        status: StatusCode::BAD_REQUEST,
        message: message.into(),
    }
}

impl Adapter {
    pub(super) async fn prepare(self, model: &str, body: &Value) -> Result<Request, ImageError> {
        let prompt = body["prompt"]
            .as_str()
            .ok_or_else(|| invalid("Image prompt must be a string"))?;
        match self {
            Self::Stability => stability(model, body, prompt),
            Self::HuggingFace => {
                let entry = MODELS.get(model).unwrap_or(&Value::Null);
                let path = entry.as_str().or_else(|| entry["path"].as_str());
                let source = if entry["task"] == "image-to-image" {
                    let raw = if truthy(&body["image"]) {
                        &body["image"]
                    } else {
                        &body["images"][0]
                    };
                    Some(source_image(raw).await.ok_or_else(|| invalid(format!(
                        "HuggingFace: model \"{model}\" requires a source image. Send it as \"image\" (or \"images\") in the request body."
                    )))?)
                } else {
                    None
                };
                Ok(Request::HuggingFace(HuggingFace {
                    model: model.to_owned(),
                    path,
                    prompt: prompt.to_owned(),
                    source,
                }))
            }
        }
    }
}

fn stability(model: &str, body: &Value, prompt: &str) -> Result<Request, ImageError> {
    let format = if truthy(&body["output_format"]) {
        body["output_format"]
            .as_str()
            .ok_or_else(|| invalid("Invalid Stability AI image output format"))?
            .to_ascii_lowercase()
    } else {
        "png".into()
    };
    let format = match format.as_str() {
        "jpg" => "jpeg".into(),
        "jpeg" | "png" | "webp" => format,
        _ => return Err(invalid("Invalid Stability AI image output format")),
    };
    let mut fields = vec![("prompt", prompt.to_owned()), ("output_format", format)];
    if truthy(&body["size"]) {
        let ratio = match body["size"].as_str() {
            Some("1024x1792") => "9:16",
            Some("1792x1024") => "16:9",
            Some("1024x1536") => "2:3",
            Some("1536x1024") => "3:2",
            _ => "1:1",
        };
        fields.push(("aspect_ratio", ratio.into()));
    }
    if let Some(style) = body["style"].as_str().filter(|style| !style.is_empty()) {
        fields.push(("style_preset", style.to_owned()));
    }
    if model.contains("sd3") {
        fields.push(("model", model.to_owned()));
    }
    let endpoint = if model.contains("ultra") {
        "ultra"
    } else if model.contains("sd3") {
        "sd3"
    } else {
        "core"
    };
    Ok(Request::Stability { endpoint, fields })
}

async fn source_image(raw: &Value) -> Option<SourceImage> {
    let value = js_trim(raw.as_str()?);
    if value.is_empty() {
        return None;
    }
    let bytes = if Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https")) {
        fetch_image_bytes(value).await?
    } else {
        let encoded = if let Some((header, data)) = value.split_once(',') {
            let header = header.to_ascii_lowercase();
            let kind = header
                .strip_prefix("data:image/")?
                .strip_suffix(";base64")?;
            if kind.is_empty() || kind.contains(';') {
                return None;
            }
            data
        } else {
            value
        };
        if encoded.len() > SOURCE_LIMIT * 4 / 3 + 4 {
            return None;
        }
        IMAGE_BASE64.decode(encoded).ok()?
    };
    if bytes.is_empty() || bytes.len() > SOURCE_LIMIT {
        return None;
    }
    Some(SourceImage {
        base64: STANDARD.encode(&bytes),
        mime: mime(&bytes),
    })
}

impl Request {
    pub(super) async fn generate(
        &self,
        storage: &Storage,
        settings: &Value,
        account: &Value,
    ) -> Result<(Value, bool), QueuedImageError> {
        let client = CLIENT
            .as_ref()
            .map_err(|_| ImageError::bad_gateway("[502]: fetch failed"))?;
        let key = ["apiKey", "accessToken"]
            .into_iter()
            .find_map(|key| account[key].as_str().filter(|value| !value.is_empty()));
        let egress = Egress {
            storage,
            settings,
            account,
            client,
            key,
        };
        match self {
            Self::Stability { endpoint, fields } => {
                let url = format!("{STABILITY}/{endpoint}");
                let binding = egress.bind(&url, "stability").await?;
                let multipart = fields.iter().fold(Form::new(), |form, (name, value)| {
                    form.text(*name, value.clone())
                });
                let response = send(
                    egress
                        .request(&binding, Method::POST)
                        .header(header::ACCEPT, "application/json")
                        .multipart(multipart),
                    key,
                    binding.relay_secret(),
                )
                .await
                .map_err(|failure| classify(failure, binding.protected))?;
                let value = read_json(response, "Stability AI")
                    .await
                    .map_err(QueuedImageError::PossiblyAccepted)?;
                Ok((
                    image(
                        value["image"]
                            .as_str()
                            .filter(|data| !data.is_empty())
                            .map(str::to_owned),
                    ),
                    binding.protected,
                ))
            }
            Self::HuggingFace(hf) => hf.generate(&egress).await,
        }
    }
}

fn custom_endpoint(account: &Value, model: &str) -> Result<Option<Url>, ImageError> {
    let Some(base) = account["providerSpecificData"]["baseUrl"]
        .as_str()
        .map(js_trim)
        .filter(|url| !url.is_empty())
    else {
        return Ok(None);
    };
    if model.len() > 1024
        || model.contains("..")
        || model.contains(['?', '#', '\\', '%'])
        || model.chars().any(char::is_control)
        || model.split('/').any(|part| part.is_empty() || part == ".")
    {
        return Err(invalid("HuggingFace: invalid model ID"));
    }
    let failed = || invalid("HuggingFace: invalid stored inference endpoint");
    let mut url = Url::parse(base).map_err(|_| failed())?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(failed());
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    url.path_segments_mut()
        .map_err(|_| failed())?
        .pop_if_empty()
        .extend(model.split('/'));
    Ok(Some(url))
}

impl HuggingFace {
    async fn generate(&self, egress: &Egress<'_>) -> Result<(Value, bool), QueuedImageError> {
        if let Some(url) = custom_endpoint(egress.account, &self.model)? {
            let binding = egress.bind(url.as_str(), "hf-self-hosted").await?;
            let payload = if let Some(source) = &self.source {
                json!({"inputs":source.base64,"parameters":{"prompt":self.prompt}})
            } else {
                json!({"inputs":self.prompt})
            };
            let response = send(
                egress.request(&binding, Method::POST).json(&payload),
                egress.key,
                binding.relay_secret(),
            )
            .await
            .map_err(|failure| classify(failure, binding.protected))?;
            let bytes = read(response, RESPONSE_LIMIT)
                .await
                .map_err(QueuedImageError::PossiblyAccepted)?;
            return Ok((image(Some(STANDARD.encode(bytes))), binding.protected));
        }
        let path = self.path.ok_or_else(|| invalid(format!(
            "HuggingFace: no HuggingFace router mapping for model \"{}\". Add a model mapping in the built-in HuggingFace model catalog or set a custom base URL on the connection.",
            self.model
        )))?;
        let mut url = format!("{HUGGING_FACE}/{path}");
        let payload = if let Some(source) = &self.source {
            let mime = source
                .mime
                .ok_or_else(|| invalid("HuggingFace: unsupported source image format"))?;
            let uri = format!("data:{mime};base64,{}", source.base64);
            url.push_str("?_subdomain=queue");
            json!({"prompt":self.prompt,"image_url":uri,"image_urls":[uri]})
        } else {
            json!({"prompt":self.prompt})
        };
        let deadline = Instant::now() + Duration::from_secs(120);
        let binding = egress.bind(&url, "hf-hosted").await?;
        let response = send(
            egress.request(&binding, Method::POST).json(&payload),
            egress.key,
            binding.relay_secret(),
        )
        .await
        .map_err(|failure| classify(failure, binding.protected))?;
        if response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|header| header.to_str().ok())
            .is_some_and(|mime| mime.starts_with("image/"))
        {
            return Ok((
                image(Some(
                    STANDARD.encode(
                        read(response, RESPONSE_LIMIT)
                            .await
                            .map_err(QueuedImageError::PossiblyAccepted)?,
                    ),
                )),
                binding.protected,
            ));
        }
        let mut value = read_json(response, "HuggingFace")
            .await
            .map_err(QueuedImageError::PossiblyAccepted)?;
        if self.source.is_some() && !value["images"].is_array() {
            value = queue(egress, &binding, &value, deadline)
                .await
                .map_err(QueuedImageError::PossiblyAccepted)?;
        }
        let url = value["images"][0]["url"]
            .as_str()
            .filter(|url| !url.is_empty())
            .ok_or_else(|| {
                QueuedImageError::PossiblyAccepted(ImageError::bad_gateway(
                    "HuggingFace returned no generated image",
                ))
            })?;
        let artifact = source_image(&json!(url))
            .await
            .and_then(|result| result.mime.map(|_| result.base64))
            .ok_or_else(|| {
                QueuedImageError::PossiblyAccepted(ImageError::bad_gateway(
                    "Could not read the generated HuggingFace image",
                ))
            })?;
        Ok((image(Some(artifact)), binding.protected))
    }
}

fn fixture() -> bool {
    std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

struct Egress<'a> {
    storage: &'a Storage,
    settings: &'a Value,
    account: &'a Value,
    client: &'a Client,
    key: Option<&'a str>,
}

impl Egress<'_> {
    async fn bind(&self, url: &str, lane: &str) -> Result<Bound, QueuedImageError> {
        let unavailable =
            || QueuedImageError::Protected(proxy_error("Image connection proxy is unavailable"));
        let selection = provider_http::select(self.storage, self.settings, self.account, url)
            .await
            .map_err(|_| unavailable())?;
        if matches!(&selection, Selection::Routed(Route::Direct))
            && proxy_required(self.settings, self.account)
            || matches!(&selection, Selection::Routed(relay @ Route::Relay { .. }) if relay.relay_secret().is_none())
        {
            return Err(unavailable());
        }
        self.bind_selection(selection, url, lane)
            .ok_or_else(unavailable)
    }

    fn bind_selection(&self, selection: Selection, url: &str, lane: &str) -> Option<Bound> {
        let parsed = Url::parse(url).ok()?;
        let mut local = format!("http://127.0.0.1:52989/{lane}{}", parsed.path());
        if let Some(query) = parsed.query() {
            local.push('?');
            local.push_str(query);
        }
        provider_http::bind_selection(selection, url, &local, self.client, TIMEOUT, None)
    }

    fn follow_up(&self, initial: &Bound, url: &str) -> Result<Bound, ImageError> {
        let unavailable = || proxy_error("HuggingFace image job proxy is unavailable");
        let selection = match &initial.route {
            Route::Direct if initial.protected => Selection::ExplicitBypass,
            relay @ Route::Relay { .. } => {
                let target = Url::parse(url).map_err(|_| unavailable())?;
                Selection::Routed(relay.relay_target(&target).ok_or_else(unavailable)?)
            }
            route => Selection::Routed(route.clone()),
        };
        self.bind_selection(selection, url, "hf-hosted")
            .ok_or_else(unavailable)
    }

    fn request(&self, binding: &Bound, method: Method) -> RequestBuilder {
        let request = binding.request(method);
        if let Some(key) = self.key {
            request.bearer_auth(key)
        } else {
            request
        }
    }
}

fn classify(failure: ImageError, protected: bool) -> QueuedImageError {
    if protected {
        QueuedImageError::Protected(failure)
    } else if failure.status.is_client_error() && failure.status != StatusCode::REQUEST_TIMEOUT {
        QueuedImageError::BeforeSubmit(failure)
    } else {
        QueuedImageError::PossiblyAccepted(failure)
    }
}

async fn send(
    request: RequestBuilder,
    key: Option<&str>,
    relay: Option<&str>,
) -> Result<Response, ImageError> {
    let response = request
        .send()
        .await
        .map_err(|_| ImageError::bad_gateway("[502]: fetch failed"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status.is_redirection() {
        return Err(ImageError::bad_gateway(
            "Image provider returned a redirect",
        ));
    }
    let raw = read(response, ERROR_LIMIT).await.unwrap_or_default();
    let body = serde_json::from_slice::<Value>(&raw).unwrap_or_default();
    let mut message = [
        &body["error"]["message"],
        &body["message"],
        &body["error"],
        &body["errors"][0],
    ]
    .into_iter()
    .find_map(|value| value.as_str().filter(|message| !message.is_empty()))
    .map(str::to_owned)
    .or_else(|| {
        String::from_utf8(raw)
            .ok()
            .filter(|message| !message.is_empty())
    })
    .unwrap_or_else(|| default_error(status));
    for secret in [key, relay].into_iter().flatten() {
        message = message.replace(secret, "[redacted]");
    }
    let message: String = message.chars().take(2000).collect();
    Err(ImageError {
        status,
        message: format!("[{}]: {message}", status.as_u16()),
    })
}

async fn read(mut response: Response, limit: usize) -> Result<Vec<u8>, ImageError> {
    let failed = || ImageError::bad_gateway("Invalid image provider response");
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(failed());
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| failed())? {
        if chunk.len() > limit - data.len() {
            return Err(failed());
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

async fn read_json(response: Response, provider: &str) -> Result<Value, ImageError> {
    serde_json::from_slice(&read(response, RESPONSE_LIMIT).await?)
        .map_err(|_| ImageError::bad_gateway(format!("Invalid response from {provider}")))
}

fn queue_urls(value: &Value) -> Option<(String, String)> {
    let id = value["request_id"].as_str().filter(|id| {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    })?;
    let url = Url::parse(value["response_url"].as_str()?).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("queue.fal.run")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let segments: Vec<_> = url.path_segments()?.collect();
    if segments.len() < 4
        || segments.len() > 12
        || segments[0] != "fal-ai"
        || segments[segments.len() - 2..] != ["requests", id]
        || segments.iter().any(|part| {
            part.is_empty()
                || part.len() > 128
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return None;
    }
    let base = format!("{HUGGING_FACE}/fal-ai{}", url.path());
    Some((
        format!("{base}/status?_subdomain=queue"),
        format!("{base}?_subdomain=queue"),
    ))
}

async fn queue(
    egress: &Egress<'_>,
    initial_egress: &Bound,
    initial: &Value,
    deadline: Instant,
) -> Result<Value, ImageError> {
    let (status, result) = queue_urls(initial)
        .ok_or_else(|| ImageError::bad_gateway("Invalid HuggingFace image job"))?;
    let status = egress.follow_up(initial_egress, &status)?;
    let result = egress.follow_up(initial_egress, &result)?;
    let interval = if fixture() {
        Duration::from_millis(25)
    } else {
        Duration::from_millis(500)
    };
    timeout_at(deadline, async {
        let mut state = initial.clone();
        if state["status"].is_null() {
            state["status"] = json!("IN_QUEUE");
        }
        loop {
            match state["status"].as_str() {
                Some("COMPLETED") => break,
                Some("IN_QUEUE" | "IN_PROGRESS") => {}
                _ => return Err(ImageError::bad_gateway("HuggingFace image job failed")),
            }
            sleep(interval).await;
            state = read_json(
                send(
                    egress.request(&status, Method::GET),
                    egress.key,
                    status.relay_secret(),
                )
                .await?,
                "HuggingFace",
            )
            .await?;
            if state
                .get("request_id")
                .is_some_and(|value| value != &initial["request_id"])
            {
                return Err(ImageError::bad_gateway(
                    "HuggingFace returned a different image job",
                ));
            }
        }
        read_json(
            send(
                egress.request(&result, Method::GET),
                egress.key,
                result.relay_secret(),
            )
            .await?,
            "HuggingFace",
        )
        .await
    })
    .await
    .map_err(|_| ImageError::bad_gateway("HuggingFace image job timed out"))?
}

fn image(base64: Option<String>) -> Value {
    json!({
        "created":chrono::Utc::now().timestamp(),
        "data":base64.into_iter().map(|data| json!({"b64_json":data})).collect::<Vec<_>>(),
    })
}
