use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::LazyLock;
use std::time::Duration;

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use regex::Regex;
use reqwest::{Client, Method, Response, StatusCode, header};
use serde_json::{Value, json};
use url::Url;

use crate::provider_http::{Delivery, http_client};
use crate::provider_index::js_trim;
use crate::storage::Storage;

const ORIGIN: &str = "https://veoaifree.com";
const HOST: &str = "veoaifree.com";
const AJAX: &str = "https://veoaifree.com/wp-admin/admin-ajax.php";
const TIMEOUT: Duration = Duration::from_secs(30);
const TEXT_LIMIT: usize = 24 * 1024 * 1024;
const ARTIFACT_LIMIT: usize = 100 * 1024 * 1024;
static NONCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""nonce"\s*:\s*"([a-f0-9]{1,128})""#).expect("Veo nonce pattern")
});

pub(crate) struct Failure {
    pub status: StatusCode,
    pub message: String,
}

impl Failure {
    fn new(status: StatusCode, message: &str) -> Self {
        Self {
            status,
            message: message.to_owned(),
        }
    }

    fn upstream(message: &str) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn route() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Veo AI Free cannot use the configured route",
        )
    }
}

pub(crate) async fn image(
    storage: &Storage,
    settings: &Value,
    body: &Value,
    binary: bool,
) -> Result<Value, Failure> {
    let prompt = prompt(body)?;
    let aspect = aspect(body);
    let mut session = Session::open(storage, settings).await?;
    let answer = session
        .ajax(&[
            ("promptIMG", prompt),
            ("totalVariationsIMG", "1"),
            (
                "aspectRatioIMG",
                match aspect {
                    Aspect::Portrait => "IMAGE_ASPECT_RATIO_PORTRAIT",
                    Aspect::Square => "IMAGE_ASPECT_RATIO_SQUARE",
                    Aspect::Landscape => "IMAGE_ASPECT_RATIO_LANDSCAPE",
                },
            ),
            ("actionType", "banan-image-generator"),
        ])
        .await?;
    let answer = generated(&answer)
        .ok_or_else(|| Failure::upstream("Veo AI Free image generation failed"))?;
    let mut data = Vec::new();
    for item in answer
        .split(',')
        .map(js_trim)
        .filter(|item| !item.is_empty())
        .take(16)
    {
        if let Ok(url) = Url::parse(item) {
            if !secure(&url) {
                return Err(Failure::upstream("Invalid Veo AI Free image URL"));
            }
            data.push(json!({"url":url.as_str()}));
        } else {
            let bytes = STANDARD
                .decode(item)
                .or_else(|_| STANDARD_NO_PAD.decode(item))
                .map_err(|_| Failure::upstream("Invalid Veo AI Free image data"))?;
            if bytes.is_empty() {
                return Err(Failure::upstream("Veo AI Free returned empty image data"));
            }
            data.push(json!({"b64_json":STANDARD.encode(bytes),"mime_type":"image/png"}));
        }
    }
    if data.is_empty() {
        return Err(Failure::upstream("Veo AI Free returned no images"));
    }
    if binary && let Some(url) = data[0]["url"].as_str() {
        let url =
            Url::parse(url).map_err(|_| Failure::upstream("Invalid Veo AI Free image URL"))?;
        let bytes = session
            .download_once(url, Artifact::Image)
            .await?
            .ok_or_else(|| Failure::upstream("Veo AI Free image artifact is not ready"))?;
        data[0] = json!({"b64_json":STANDARD.encode(bytes)});
    }
    Ok(json!({"created":chrono::Utc::now().timestamp(),"data":data}))
}

pub(crate) async fn video(
    storage: &Storage,
    settings: &Value,
    model: &str,
    body: &Value,
) -> Result<Value, Failure> {
    if [
        "image",
        "image_url",
        "image_urls",
        "images",
        "input_reference",
        "input_references",
        "first_frame",
        "last_frame",
    ]
    .iter()
    .any(|key| !body[*key].is_null())
    {
        return Err(Failure::new(
            StatusCode::BAD_REQUEST,
            "Veo AI Free video uses text prompts",
        ));
    }
    let prompt = prompt(body)?;
    let aspect = aspect(body);
    let mut session = Session::open(storage, settings).await?;
    let answer = session
        .ajax(&[
            ("prompt", prompt),
            ("totalVariations", "1"),
            (
                "aspectRatio",
                match aspect {
                    Aspect::Portrait => "VIDEO_ASPECT_RATIO_PORTRAIT",
                    Aspect::Square => "VIDEO_ASPECT_RATIO_SQUARE",
                    Aspect::Landscape => "VIDEO_ASPECT_RATIO_LANDSCAPE",
                },
            ),
            ("actionType", "full-video-generate"),
        ])
        .await?;
    let scene = generated(&answer)
        .filter(|scene| scene.len() <= 64 * 1024)
        .ok_or_else(|| Failure::upstream("Veo AI Free video submission failed"))?;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let answer = session
            .ajax(&[("sceneData", scene), ("actionType", "final-video-results")])
            .await?;
        let Some(url) = generated(&answer).and_then(|answer| {
            answer
                .split([',', '\n'])
                .map(js_trim)
                .find_map(|item| Url::parse(item).ok().filter(secure))
        }) else {
            continue;
        };
        let bytes = session.download(url).await?;
        let now = chrono::Utc::now().timestamp();
        return Ok(json!({
            "id":format!("veoaifree-web:{}",uuid::Uuid::new_v4()),"model":model,"status":"completed",
            "created_at":now,"completed_at":now,"video":{"url":format!("data:video/mp4;base64,{}", STANDARD.encode(bytes))},
        }));
    }
    Err(Failure::new(
        StatusCode::GATEWAY_TIMEOUT,
        "Veo AI Free video generation timed out",
    ))
}

enum Aspect {
    Portrait,
    Square,
    Landscape,
}

#[derive(Clone, Copy)]
enum Artifact {
    Image,
    Video,
}

fn aspect(body: &Value) -> Aspect {
    let size = body["size"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if size.contains("9:16") || size.contains("portrait") {
        Aspect::Portrait
    } else if size.contains("1:1") || size.contains("square") {
        Aspect::Square
    } else {
        Aspect::Landscape
    }
}

fn prompt(body: &Value) -> Result<&str, Failure> {
    body["prompt"]
        .as_str()
        .map(js_trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| Failure::new(StatusCode::BAD_REQUEST, "prompt is required"))
}

fn generated(answer: &str) -> Option<&str> {
    let answer = js_trim(answer);
    (!answer.is_empty()
        && answer != "0"
        && !answer
            .get(..answer.len().min(120))
            .unwrap_or(answer)
            .to_ascii_lowercase()
            .contains("error"))
    .then_some(answer)
}

struct Session<'a> {
    storage: &'a Storage,
    route: Delivery,
    client: Client,
    nonce: String,
    cookies: BTreeMap<String, String>,
}

impl<'a> Session<'a> {
    async fn open(storage: &'a Storage, settings: &Value) -> Result<Self, Failure> {
        let route = route(storage, settings, AJAX).await?;
        let direct = http_client(TIMEOUT).build().map_err(|_| Failure::route())?;
        let client = route.client(&direct, TIMEOUT).ok_or_else(Failure::route)?;
        let mut session = Self {
            storage,
            route,
            client,
            nonce: String::new(),
            cookies: BTreeMap::new(),
        };
        let response = session.site(Method::GET, ORIGIN, None).await?;
        let html = String::from_utf8(bounded(response, 2 * 1024 * 1024).await?)
            .map_err(|_| Failure::upstream("Invalid Veo AI Free page"))?;
        session.nonce = NONCE
            .captures(&html)
            .and_then(|capture| capture.get(1))
            .map(|nonce| nonce.as_str().to_owned())
            .ok_or_else(|| Failure::upstream("Veo AI Free did not issue a form nonce"))?;
        Ok(session)
    }

    async fn site(
        &mut self,
        method: Method,
        url: &str,
        form: Option<&[(&str, &str)]>,
    ) -> Result<Response, Failure> {
        let latest = self
            .storage
            .read_settings()
            .await
            .ok_or_else(Failure::route)?;
        if route(self.storage, &latest, url).await? != self.route {
            return Err(Failure::route());
        }
        let mut request = self
            .client
            .request(method, url)
            .header(header::USER_AGENT, "9Router")
            .header(header::REFERER, format!("{ORIGIN}/"));
        if !self.cookies.is_empty() {
            let serialized = self
                .cookies
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("; ");
            let mut cookie = header::HeaderValue::from_str(&serialized)
                .map_err(|_| Failure::upstream("Invalid Veo AI Free session"))?;
            cookie.set_sensitive(true);
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(form) = form {
            request = request.header(header::ORIGIN, ORIGIN).form(form);
        }
        let response = request
            .send()
            .await
            .map_err(|_| Failure::upstream("Veo AI Free request failed"))?;
        if !response.status().is_success() {
            return Err(Failure::new(
                if response.status() == StatusCode::TOO_MANY_REQUESTS {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::BAD_GATEWAY
                },
                "Veo AI Free rejected the request",
            ));
        }
        for cookie in response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|cookie| cookie.to_str().ok())
        {
            let Some((name, value)) = cookie
                .split(';')
                .next()
                .and_then(|pair| pair.trim().split_once('='))
            else {
                continue;
            };
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|value| value.is_ascii_alphanumeric() || b"_-".contains(&value))
            {
                continue;
            }
            if value.is_empty() {
                self.cookies.remove(name);
            } else if value.len() <= 4096 && self.cookies.len() < 24 {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
        Ok(response)
    }

    async fn ajax(&mut self, fields: &[(&str, &str)]) -> Result<String, Failure> {
        let nonce = self.nonce.clone();
        let mut form = vec![("action", "veo_video_generator"), ("nonce", &nonce)];
        form.extend_from_slice(fields);
        let response = self.site(Method::POST, AJAX, Some(&form)).await?;
        String::from_utf8(bounded(response, TEXT_LIMIT).await?)
            .map_err(|_| Failure::upstream("Invalid Veo AI Free response"))
    }

    async fn download(&self, initial: Url) -> Result<Vec<u8>, Failure> {
        for attempt in 0..12 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if let Some(bytes) = self.download_once(initial.clone(), Artifact::Video).await? {
                return Ok(bytes);
            }
        }
        Err(Failure::new(
            StatusCode::GATEWAY_TIMEOUT,
            "Veo AI Free video artifact is not ready",
        ))
    }

    async fn download_once(
        &self,
        mut target: Url,
        kind: Artifact,
    ) -> Result<Option<Vec<u8>>, Failure> {
        for _ in 0..=3 {
            if !secure(&target) {
                return Err(Failure::upstream("Invalid Veo AI Free artifact URL"));
            }
            let settings = self
                .storage
                .read_settings()
                .await
                .ok_or_else(Failure::route)?;
            if route(self.storage, &settings, AJAX).await? != self.route {
                return Err(Failure::route());
            }
            let selected = route(self.storage, &settings, target.as_str()).await?;
            let host = target.host_str().ok_or_else(Failure::route)?;
            let client = if host == HOST {
                if selected != self.route {
                    return Err(Failure::route());
                }
                self.client.clone()
            } else {
                if matches!(selected, Delivery::Tunnel(_)) {
                    return Err(Failure::route());
                }
                let resolved = tokio::time::timeout(
                    Duration::from_secs(5),
                    tokio::net::lookup_host((host, 443)),
                )
                .await
                .map_err(|_| Failure::upstream("Veo AI Free artifact DNS timed out"))?
                .map_err(|_| Failure::upstream("Could not resolve Veo AI Free artifact host"))?;
                let addresses = resolved
                    .map(|address| {
                        crate::chat_prefetch::public_address(address.ip())
                            .map(|address| SocketAddr::from((address, 443)))
                    })
                    .collect::<Option<Vec<_>>>()
                    .filter(|addresses| !addresses.is_empty())
                    .ok_or_else(|| Failure::upstream("Veo AI Free artifact host is not public"))?;
                http_client(TIMEOUT)
                    .resolve_to_addrs(host, &addresses)
                    .build()
                    .map_err(|_| Failure::route())?
            };
            let response = client
                .get(target.clone())
                .header(header::USER_AGENT, "9Router")
                .send()
                .await
                .map_err(|_| Failure::upstream("Veo AI Free artifact download failed"))?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|location| location.to_str().ok())
                    .ok_or_else(|| Failure::upstream("Invalid Veo AI Free artifact redirect"))?;
                target = target
                    .join(location)
                    .map_err(|_| Failure::upstream("Invalid Veo AI Free artifact redirect"))?;
                continue;
            }
            if matches!(response.status().as_u16(), 202 | 404 | 409 | 425) {
                return Ok(None);
            }
            if !response.status().is_success() {
                return Err(Failure::upstream(
                    "Veo AI Free artifact download was rejected",
                ));
            }
            let mime = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|mime| mime.to_str().ok())
                .and_then(|mime| mime.split(';').next())
                .map(str::trim);
            let (supported, limit) = match kind {
                Artifact::Image => (
                    matches!(
                        mime,
                        Some("image/png" | "image/jpeg" | "image/webp" | "image/gif")
                    ),
                    TEXT_LIMIT,
                ),
                Artifact::Video => (
                    matches!(mime, Some("video/mp4" | "application/octet-stream")),
                    ARTIFACT_LIMIT,
                ),
            };
            if !supported {
                return Err(Failure::upstream(
                    "Veo AI Free artifact format is not supported",
                ));
            }
            let bytes = bounded(response, limit).await?;
            if matches!(kind, Artifact::Video)
                && (bytes.len() < 12 || bytes.get(4..8) != Some(b"ftyp"))
            {
                return Err(Failure::upstream("Veo AI Free artifact is not MP4"));
            }
            return Ok(Some(bytes));
        }
        Err(Failure::upstream("Too many Veo AI Free artifact redirects"))
    }
}

async fn route(storage: &Storage, settings: &Value, url: &str) -> Result<Delivery, Failure> {
    Delivery::select(storage, settings, &Value::Null, url)
        .await
        .map_err(|_| Failure::route())
}

async fn bounded(mut response: Response, limit: usize) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Failure::upstream("Veo AI Free response was interrupted"))?
    {
        if chunk.len() > limit - bytes.len() {
            return Err(Failure::upstream("Veo AI Free response is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn secure(url: &Url) -> bool {
    url.scheme() == "https"
        && url.has_host()
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.as_str().len() <= 8192
}
