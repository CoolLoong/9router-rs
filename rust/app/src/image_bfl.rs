use std::sync::LazyLock;
use std::time::Duration;

use axum::http::{HeaderValue, StatusCode, header};
use reqwest::{Client, Method, RequestBuilder, Response};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

use super::{ImageError, QueuedImageError, default_error, proxy_required};
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::proxy_transport::{self, Route, Selection};
use crate::storage::Storage;

const SUBMIT_BASE: &str = "https://api.bfl.ai/v1/";
const POLL_PATH: &str = "/v1/get_result";
const POLL_INTERVAL: Duration = Duration::from_millis(1500);
const POLL_TIMEOUT: Duration = Duration::from_secs(120);
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;

static CLIENT: LazyLock<Result<Client, reqwest::Error>> =
    LazyLock::new(|| client_builder().build());

fn client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

struct Delivery {
    client: Client,
    fixture: bool,
    protected: bool,
    relay: Option<(String, HeaderValue)>,
}

impl Delivery {
    fn request(&self, method: Method, endpoint: &Url) -> RequestBuilder {
        let Some((url, authorization)) = &self.relay else {
            return self
                .client
                .request(method, destination(endpoint, self.fixture));
        };
        let mut path = endpoint.path().to_owned();
        if let Some(query) = endpoint.query() {
            path.push('?');
            path.push_str(query);
        }
        Route::Relay {
            url: url.clone(),
            target: endpoint.origin().ascii_serialization(),
            path,
            authorization: Some(authorization.clone()),
        }
        .relay(
            self.client
                .request(method, url)
                .timeout(Duration::from_secs(30)),
        )
    }

    fn relay_secret(&self) -> Option<&str> {
        self.relay
            .as_ref()
            .and_then(|(_, secret)| secret.to_str().ok())
    }

    fn contains_secret(&self, value: &str, secret: &str) -> bool {
        value.contains(secret)
            || self
                .relay_secret()
                .is_some_and(|token| value.contains(token))
    }

    fn scrub(&self, message: &str, secret: &str) -> String {
        let mut message = message.replace(secret, "[redacted]");
        if let Some(token) = self.relay_secret() {
            message = message.replace(token, "[redacted]");
        }
        message.chars().take(2048).collect()
    }
}

async fn delivery(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    url: &Url,
) -> Result<Delivery, QueuedImageError> {
    use QueuedImageError::{BeforeSubmit, Protected};
    let unavailable = || {
        Protected(ImageError::bad_gateway(
            "BFL connection proxy is unavailable",
        ))
    };
    let route = crate::provider_http::select(storage, settings, account, url.as_str())
        .await
        .map_err(|_| unavailable())?;
    let direct = |protected| {
        let client = CLIENT.as_ref().map_err(|_| {
            let failure = ImageError::bad_gateway("[502]: fetch failed");
            if protected {
                Protected(failure)
            } else {
                BeforeSubmit(failure)
            }
        })?;
        Ok(Delivery {
            client: client.clone(),
            fixture: fixture(),
            protected,
            relay: None,
        })
    };
    match route {
        Selection::Routed(Route::Direct) if !proxy_required(settings, account) => direct(false),
        Selection::ExplicitBypass => direct(true),
        Selection::Routed(Route::Proxy { url, .. }) => {
            let proxy = proxy_transport::tunnel_proxy_url(&url).ok_or_else(unavailable)?;
            let client = proxy_transport::vendor_metadata_client(proxy, client_builder())
                .ok_or_else(unavailable)?;
            Ok(Delivery {
                client,
                fixture: false,
                protected: true,
                relay: None,
            })
        }
        Selection::Routed(Route::Relay {
            url,
            authorization: Some(token),
            ..
        }) => {
            let client = CLIENT.as_ref().map_err(|_| unavailable())?.clone();
            Ok(Delivery {
                client,
                fixture: false,
                protected: true,
                relay: Some((url, token)),
            })
        }
        Selection::Routed(Route::Relay { .. }) => Err(Protected(ImageError::bad_gateway(
            "BFL requires a validated Relay pool",
        ))),
        _ => Err(unavailable()),
    }
}

pub(super) async fn generate(
    storage: &Storage,
    settings: &Value,
    model: &str,
    account: &Value,
    body: &Value,
) -> Result<Value, QueuedImageError> {
    use QueuedImageError::{BeforeSubmit, PossiblyAccepted};

    let payload = payload(model, body).map_err(BeforeSubmit)?;
    let secret = ["apiKey", "accessToken"]
        .into_iter()
        .find_map(|field| account[field].as_str().filter(|value| !value.is_empty()));
    let (secret, mut key) = secret
        .filter(|secret| secret.bytes().all(|byte| byte.is_ascii_graphic()))
        .and_then(|secret| HeaderValue::from_str(secret).ok().map(|key| (secret, key)))
        .ok_or_else(|| {
            BeforeSubmit(ImageError {
                status: StatusCode::UNAUTHORIZED,
                message: "Invalid Black Forest Labs API key".into(),
            })
        })?;
    key.set_sensitive(true);

    let submit = Url::parse(&format!("{SUBMIT_BASE}{model}"))
        .map_err(|_| BeforeSubmit(invalid("Invalid Black Forest Labs model")))?;
    let delivery = delivery(storage, settings, account, &submit).await?;
    let response = delivery
        .request(Method::POST, &submit)
        .header("x-key", key.clone())
        .json(&payload)
        .send()
        .await
        .map_err(|_| PossiblyAccepted(ImageError::bad_gateway("[502]: fetch failed")))?;
    let status = response.status();
    if !status.is_success() {
        let failure = submit_error(response, &delivery, secret).await;
        return Err(if !delivery.protected && definite_rejection(status) {
            BeforeSubmit(failure)
        } else {
            PossiblyAccepted(failure)
        });
    }
    let submitted = read_json(response).await.map_err(PossiblyAccepted)?;
    let raw = submitted["polling_url"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PossiblyAccepted(ImageError::bad_gateway("BFL: no polling_url returned")))?;
    let address = polling_url(raw, submitted.get("id"))
        .ok_or_else(|| PossiblyAccepted(ImageError::bad_gateway("BFL: invalid polling_url")))?;
    if delivery.protected
        && address
            .query_pairs()
            .any(|(_, value)| delivery.contains_secret(&value, secret))
    {
        return Err(PossiblyAccepted(ImageError::bad_gateway(
            "BFL polling URL contains a credential",
        )));
    }
    poll(&delivery, &address, key, secret)
        .await
        .map_err(PossiblyAccepted)
}

fn payload(model: &str, body: &Value) -> Result<Value, ImageError> {
    let (aspect, image_arg) = match model {
        "flux-pro-1.1" | "flux-pro" | "flux-dev" => (false, "image_prompt"),
        "flux-pro-1.1-ultra" => (true, "image_prompt"),
        "flux-kontext-pro" | "flux-kontext-max" => (true, "input_image"),
        _ => return Err(invalid("Invalid Black Forest Labs model")),
    };
    let prompt = body["prompt"]
        .as_str()
        .ok_or_else(|| invalid("Image prompt must be a string"))?;
    let mut payload = json!({"prompt":prompt});
    if truthy(&body["size"]) {
        let size = body["size"]
            .as_str()
            .ok_or_else(|| invalid("Image size must be a string"))?;
        let mut parts = size.split('x').map(dimension);
        let width = parts.next().flatten();
        let height = parts.next().flatten();
        if aspect {
            if let (Some(width), Some(height)) = (width, height) {
                let divisor = gcd(width, height);
                payload["aspect_ratio"] =
                    json!(format!("{}:{}", width / divisor, height / divisor));
            }
        } else {
            if let Some(width) = width {
                payload["width"] = json!(width);
            }
            if let Some(height) = height {
                payload["height"] = json!(height);
            }
        }
    }
    if truthy(&body["image"]) {
        let image = body["image"]
            .as_str()
            .ok_or_else(|| invalid("Reference image must be a string"))?;
        payload[image_arg] = json!(image);
    }
    Ok(payload)
}

fn dimension(part: &str) -> Option<u32> {
    js_trim(part)
        .parse::<u32>()
        .ok()
        .filter(|value| *value != 0)
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn polling_url(raw: &str, job: Option<&Value>) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "https"
        || !matches!(
            url.host_str()?,
            "api.bfl.ai"
                | "api.eu.bfl.ai"
                | "api.us.bfl.ai"
                | "api.us1.bfl.ai"
                | "api.us2.bfl.ai"
                | "api.bfl.ml"
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.path() != POLL_PATH
        || url.fragment().is_some()
    {
        return None;
    }
    let mut params = url.query_pairs();
    let (field, value) = params.next()?;
    if field != "id"
        || value.is_empty()
        || value.len() > 256
        || value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
        || params.next().is_some()
        || job.is_some_and(|job| job.as_str() != Some(value.as_ref()))
    {
        return None;
    }
    Some(url)
}

fn fixture() -> bool {
    cfg!(debug_assertions) && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

fn destination(url: &Url, use_fixture: bool) -> String {
    if !use_fixture {
        return url.to_string();
    }
    let host = url.host_str().unwrap_or_default();
    let mut mocked = format!("http://127.0.0.1:52989/bfl/{host}{}", url.path());
    if let Some(query) = url.query() {
        mocked.push('?');
        mocked.push_str(query);
    }
    mocked
}

async fn poll(
    delivery: &Delivery,
    address: &Url,
    key: HeaderValue,
    secret: &str,
) -> Result<Value, ImageError> {
    let (interval, timeout) = if fixture() {
        (Duration::from_millis(25), Duration::from_millis(450))
    } else {
        (POLL_INTERVAL, POLL_TIMEOUT)
    };
    timeout_at(Instant::now() + timeout, async {
        loop {
            sleep(interval).await;
            let response = delivery
                .request(Method::GET, address)
                .header("x-key", key.clone())
                .header(header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(|_| ImageError::bad_gateway("BFL polling request failed"))?;
            if !response.status().is_success() {
                return Err(ImageError::bad_gateway(format!(
                    "BFL status {}",
                    response.status().as_u16()
                )));
            }
            let result = read_json(response).await?;
            match result["status"].as_str() {
                Some("Ready") => {
                    let entries: Vec<_> = result["result"]["sample"]
                        .as_str()
                        .filter(|sample| sample_url(sample, delivery, secret))
                        .map(|sample| json!({"url":sample}))
                        .into_iter()
                        .collect();
                    return Ok(json!({"created":chrono::Utc::now().timestamp(),"data":entries}));
                }
                Some("Error" | "Failed") => {
                    let error = result["error"]
                        .as_str()
                        .filter(|error| !error.is_empty())
                        .unwrap_or("BFL generation failed");
                    return Err(ImageError::bad_gateway(delivery.scrub(error, secret)));
                }
                Some(
                    status @ ("Request Moderated" | "Content Moderated" | "Task not found"
                    | "No Card Generated"),
                ) => {
                    return Err(ImageError::bad_gateway(format!("BFL: {status}")));
                }
                None => {
                    return Err(ImageError::bad_gateway(
                        "Invalid response from black-forest-labs",
                    ));
                }
                Some(_) => {}
            }
        }
    })
    .await
    .map_err(|_| ImageError::bad_gateway("BFL polling timeout"))?
}

fn sample_url(sample: &str, delivery: &Delivery, secret: &str) -> bool {
    Url::parse(sample).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && !delivery.contains_secret(
                &percent_encoding::percent_decode_str(sample).decode_utf8_lossy(),
                secret,
            )
    })
}

fn definite_rejection(status: StatusCode) -> bool {
    matches!(
        status.as_u16(),
        400 | 401 | 402 | 403 | 404 | 406 | 422 | 429
    )
}

fn invalid(message: &str) -> ImageError {
    ImageError {
        status: StatusCode::BAD_REQUEST,
        message: message.to_owned(),
    }
}

async fn submit_error(response: Response, delivery: &Delivery, secret: &str) -> ImageError {
    let status = response.status();
    if status.is_redirection() {
        return ImageError::bad_gateway("BFL submit redirect refused");
    }
    let raw = bounded(response).await.unwrap_or_default();
    let raw = String::from_utf8_lossy(&raw);
    let parsed = serde_json::from_str::<Value>(&raw).ok();
    let message = parsed
        .as_ref()
        .and_then(|data| {
            [&data["error"]["message"], &data["message"], &data["error"]]
                .into_iter()
                .find(|value| truthy(value))
        })
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_else(|| {
            if raw.is_empty() {
                default_error(status)
            } else {
                raw.into_owned()
            }
        });
    ImageError {
        status,
        message: format!(
            "[{}]: {}",
            status.as_u16(),
            delivery.scrub(&message, secret)
        ),
    }
}

async fn read_json(response: Response) -> Result<Value, ImageError> {
    let error = || ImageError::bad_gateway("Invalid response from black-forest-labs");
    serde_json::from_slice(&bounded(response).await.ok_or_else(error)?).map_err(|_| error())
}

async fn bounded(mut response: Response) -> Option<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|size| size > RESPONSE_LIMIT as u64)
    {
        return None;
    }
    let mut buffer = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if buffer.len() + chunk.len() > RESPONSE_LIMIT {
            return None;
        }
        buffer.extend_from_slice(&chunk);
    }
    Some(buffer)
}
