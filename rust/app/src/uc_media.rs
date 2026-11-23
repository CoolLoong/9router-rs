use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use axum::http::{StatusCode, header};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use reqwest::{Client, Method, RequestBuilder, Response, Url};
use rusqlite::TransactionBehavior;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex as AsyncMutex;

use crate::provider_connections::{by_id, now, object, patch, write};
use crate::provider_http::{Delivery, http_client};
use crate::storage::Storage;

const ORIGIN: &str = "https://uncensored.com";
const CLERK: &str = "https://clerk.uncensored.com";
const INTERNAL: &str = "https://internal.chatuncensored.ai";
const SIGNED: &str = "https://internal-6.pubyar.com/generate-signed-url";
const VERSION: &str = "5.127.1";
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_JSON: usize = 1024 * 1024;
const MAX_IMAGE: usize = 16 * 1024 * 1024;
const MAX_VIDEO: usize = 96 * 1024 * 1024;
const ASPECTS: &[(&str, &str, &str, f64)] = &[
    ("1:1", "1024", "1024", 1.0),
    ("16:9", "1024", "576", 1024.0 / 576.0),
    ("9:16", "576", "1024", 576.0 / 1024.0),
    ("4:3", "1024", "768", 1024.0 / 768.0),
    ("3:4", "768", "1024", 768.0 / 1024.0),
    ("3:2", "1152", "768", 1152.0 / 768.0),
    ("2:3", "768", "1152", 768.0 / 1152.0),
    ("4:5", "896", "1120", 896.0 / 1120.0),
    ("5:4", "1120", "896", 1120.0 / 896.0),
    ("21:9", "1344", "576", 1344.0 / 576.0),
];

static CLIENT: LazyLock<Option<Client>> = LazyLock::new(|| http_client(TIMEOUT).build().ok());
static MODELS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("uc-media-models.json")).expect("embedded UC media models")
});
type AccountLocks = Mutex<HashMap<(std::path::PathBuf, String), Weak<AsyncMutex<()>>>>;
static LOCKS: LazyLock<AccountLocks> = LazyLock::new(Mutex::default);

pub(crate) struct Failure {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}

impl Failure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn route() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "UC outbound proxy is unavailable",
        )
    }
}

pub(crate) fn supports(account: &Value) -> bool {
    account["provider"] == "uc-web"
        && matches!(account["authType"].as_str(), Some("cookie" | "apikey"))
}

pub(crate) async fn image(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    body: &Value,
) -> Result<Value, Failure> {
    let prompt = prompt(body)?;
    let model = if model.is_empty() {
        "seedream-v4.5"
    } else {
        model
    };
    let spec = MODELS["image"]
        .get(model)
        .ok_or_else(|| Failure::bad(format!("Unknown UC image model \"{model}\"")))?;
    let (ratio, width, height, _) =
        aspect(body["size"].as_str().unwrap_or_default(), &spec["aspects"]);
    let mut request = json!({
        "prompt":prompt,"mode":"dev","model_version":spec["upstream"].as_str().unwrap_or(model),
        "m_n_user":true,"moderationMode":"SUPER_LIGHT","country":"US","vdiscount":false,
        "aspect_ratio":ratio,"imageWidth":width,"imageHeight":height,
    });
    if let Some(first) = spec["resolutions"]
        .as_array()
        .and_then(|values| values.first())
    {
        let wanted = body["resolution"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        request["resolution"] = if spec["resolutions"]
            .as_array()
            .is_some_and(|allowed| allowed.iter().any(|val| val == &wanted))
        {
            json!(wanted)
        } else {
            first.clone()
        };
    }
    let session = mint(storage, settings, account).await?;
    let context = Context {
        storage,
        settings,
        account: &session.account,
    };
    let endpoint = format!("{INTERNAL}/v2/image-gen");
    let result = context
        .media(&endpoint, &session.jwt, request, "image request")
        .await?;
    let result_url = result["url"]
        .as_str()
        .ok_or_else(|| Failure::upstream("UC image response carried no result URL"))?;
    let result = context
        .download(result_url, MAX_IMAGE, Duration::from_secs(300))
        .await?;
    if !result.mime.starts_with("image/") && result.mime != "application/octet-stream" {
        return Err(Failure::upstream("UC response does not contain an image"));
    }
    Ok(json!({
        "created":chrono::Utc::now().timestamp(),
        "data":[{"b64_json":STANDARD.encode(result.bytes),"mime_type":result.mime}],
    }))
}

pub(crate) async fn video(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    model: &str,
    body: &Value,
) -> Result<Value, Failure> {
    let prompt = prompt(body)?;
    let model = if model.is_empty() { "default" } else { model };
    let spec = MODELS["video"]
        .get(model)
        .ok_or_else(|| Failure::bad(format!("Unknown UC video model \"{model}\"")))?;
    let input = image_reference(body);
    if input.is_some() && spec["i2v"] == false || input.is_none() && spec["t2v"] == false {
        return Err(Failure::bad(
            "UC video model does not support this input kind",
        ));
    }
    let duration = body
        .get("seconds")
        .filter(|value| !value.is_null())
        .unwrap_or(&body["duration"]);
    let seconds = number(duration)
        .filter(|seconds| *seconds > 0.0)
        .unwrap_or_else(|| spec["seconds"][0].as_f64().unwrap_or(5.0));
    let resolution = spec["resolutions"]
        .as_array()
        .filter(|allowed| !allowed.is_empty())
        .map_or("480p", |allowed| {
            let requested = body["resolution"].as_str().unwrap_or_default();
            if allowed.iter().any(|entry| entry == requested) {
                requested
            } else {
                allowed[0].as_str().unwrap_or("480p")
            }
        });
    let mut request = json!({
        "prompt":prompt,"media_blob_name":null,"num_frames":(seconds*16.0).round()+1.0,
        "frames_per_second":16,"num_inference_steps":30,"guide_scale":5,"shift":5,
        "aspect_ratio":if spec["aspects"].as_array().is_some_and(|values| values.iter().any(|value| value == "auto")) {"auto"} else {"9:16"},
        "pro_mode":model == "wan-3.0-uncensored-pro","turbo":model == "turbo","resolution":resolution,
        "sora_resolution":resolution,"end_frame_blob_name":null,"model":model,
        "seconds":seconds,"video_to_video_duration":seconds,"vdiscount":false,
    });
    let session = mint(storage, settings, account).await?;
    let context = Context {
        storage,
        settings,
        account: &session.account,
    };
    if let Some(input) = input {
        if session.uid.is_empty() {
            return Err(Failure::new(
                StatusCode::UNAUTHORIZED,
                "UC session is missing its account ID",
            ));
        }
        let bytes = context.image_input(input).await?;
        let signed = context
            .media(
                SIGNED,
                &session.jwt,
                json!({"content_type":"image/png","user_identifier":session.uid}),
                "signed URL request",
            )
            .await?;
        let signed_url = signed["signed_url"]
            .as_str()
            .ok_or_else(|| Failure::upstream("UC returned no signed upload URL"))?;
        let blob = signed["blob_name"]
            .as_str()
            .filter(|blob| !blob.is_empty())
            .ok_or_else(|| Failure::upstream("UC returned no upload blob name"))?;
        let (client, url, _) = context.asset(signed_url, true).await?;
        let response = client
            .put(url)
            .header(header::CONTENT_TYPE, "image/png")
            .body(bytes)
            .send()
            .await
            .map_err(|_| Failure::upstream("UC input image upload failed"))?;
        if !response.status().is_success() {
            return Err(http_error(response.status(), "input image upload"));
        }
        request["media_blob_name"] = json!(blob);
    }
    let endpoint = format!("{INTERNAL}/image_to_video");
    let result = context
        .media(&endpoint, &session.jwt, request, "video request")
        .await?;
    let result_url = result["url"]
        .as_str()
        .ok_or_else(|| Failure::upstream("UC video response carried no result URL"))?;
    let timeout = number(&result["timeout_seconds"])
        .filter(|seconds| *seconds > 0.0)
        .map_or(Duration::from_secs(300), |seconds| {
            Duration::from_secs_f64(seconds.min(900.0))
        });
    let result = context.download(result_url, MAX_VIDEO, timeout).await?;
    if !result.mime.starts_with("video/") && result.mime != "application/octet-stream" {
        return Err(Failure::upstream("UC response does not contain a video"));
    }
    let time = chrono::Utc::now();
    Ok(json!({
        "id":format!("video-uc-web-{}",time.timestamp_millis()),"object":"video.generation",
        "created":time.timestamp(),"model":model,"status":"completed",
        "video":{"url":format!("data:{};base64,{}",result.mime,STANDARD.encode(result.bytes))},
    }))
}

fn prompt(body: &Value) -> Result<&str, Failure> {
    body["prompt"]
        .as_str()
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or_else(|| Failure::bad("UC media generation requires a prompt"))
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|value| value.is_finite())
}

fn aspect(
    input: &str,
    allowed: &Value,
) -> &'static (&'static str, &'static str, &'static str, f64) {
    let valid = |ratio: &str| {
        allowed
            .as_array()
            .is_none_or(|allowed| allowed.is_empty() || allowed.iter().any(|entry| entry == ratio))
    };
    let input = input.trim();
    if let Some(result) = ASPECTS
        .iter()
        .find(|&&(ratio, ..)| ratio == input && valid(ratio))
    {
        return result;
    }
    if let Some((width, height)) = input.split_once('x')
        && let (Ok(width), Ok(height)) = (width.trim().parse::<f64>(), height.trim().parse::<f64>())
        && width.is_finite()
        && height.is_finite()
        && width > 0.0
        && height > 0.0
    {
        let wanted = width / height;
        if let Some(result) = ASPECTS
            .iter()
            .filter(|&&(ratio, ..)| valid(ratio))
            .min_by(|a, b| (a.3 - wanted).abs().total_cmp(&(b.3 - wanted).abs()))
        {
            return result;
        }
    }
    let first = allowed[0].as_str().unwrap_or("1:1");
    ASPECTS
        .iter()
        .find(|&&(ratio, ..)| ratio == first)
        .unwrap_or(&ASPECTS[0])
}

fn image_reference(body: &Value) -> Option<&str> {
    let singular = body
        .get("image")
        .filter(|value| !value.is_null())
        .or_else(|| body.get("image_url").filter(|value| !value.is_null()));
    singular
        .and_then(|value| nonempty(value).or_else(|| value.as_array()?.iter().find_map(nonempty)))
        .or_else(|| body["images"].as_array()?.iter().find_map(nonempty))
        .or_else(|| body["image_urls"].as_array()?.iter().find_map(nonempty))
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

struct Session {
    account: Value,
    jwt: String,
    uid: String,
}

async fn mint(storage: &Storage, settings: &Value, observed: &Value) -> Result<Session, Failure> {
    let id = observed["id"]
        .as_str()
        .ok_or_else(|| Failure::upstream("UC account has no ID"))?
        .to_owned();
    let lock = {
        let mut locks = LOCKS.lock().unwrap_or_else(|error| error.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        let key = (storage.root().to_owned(), id.clone());
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(AsyncMutex::new(()));
            locks.insert(key, Arc::downgrade(&lock));
            lock
        }
    };
    let _lease = lock.lock().await;
    let mut account = storage
        .run(false, {
            let id = id.clone();
            move |db| by_id(db, &id)
        })
        .await
        .ok()
        .flatten()
        .filter(|account| supports(account) && account["isActive"] != false)
        .ok_or_else(|| Failure::new(StatusCode::UNAUTHORIZED, "UC account is unavailable"))?;
    let mut jar = cookie_jar(account["apiKey"].as_str().unwrap_or_default());
    let client_cookie = nonempty(&jar["__client"]).ok_or_else(|| {
        Failure::new(
            StatusCode::UNAUTHORIZED,
            "UC credential is missing the __client cookie",
        )
    })?;
    let legacy = crate::provider_oauth::token_claims(client_cookie)["sid"]
        .as_str()
        .map(str::to_owned)
        .filter(|id| !id.is_empty());
    let context = Context {
        storage,
        settings,
        account: &account,
    };
    let mid = if let Some(sid) = legacy {
        sid
    } else {
        let endpoint = format!("{CLERK}/v1/client?_clerk_js_version={VERSION}");
        let response = context.clerk(&endpoint, Method::GET, &jar).await?;
        let rotated = rotated_cookie(&response);
        persist_rotation(storage, &id, &mut account, &mut jar, rotated).await?;
        let data = json_response(response).await?;
        let profile = data
            .get("response")
            .filter(|value| !value.is_null())
            .or_else(|| data.get("client").filter(|value| !value.is_null()))
            .unwrap_or(&data);
        let sid = profile["sessions"]
            .as_array()
            .and_then(|sessions| {
                sessions
                    .iter()
                    .find(|row| row["status"] == "active")
                    .or_else(|| sessions.first())
            })
            .and_then(|row| nonempty(&row["id"]))
            .ok_or_else(|| {
                Failure::new(
                    StatusCode::UNAUTHORIZED,
                    "UC Clerk session has no active login",
                )
            })?;
        sid.to_owned()
    };
    let mut endpoint = Url::parse(&format!("{CLERK}/v1/client/sessions"))
        .map_err(|_| Failure::upstream("Invalid UC Clerk endpoint"))?;
    endpoint
        .path_segments_mut()
        .map_err(|_| Failure::upstream("Invalid UC Clerk endpoint"))?
        .push(&mid)
        .push("tokens");
    endpoint
        .query_pairs_mut()
        .append_pair("_clerk_js_version", VERSION);
    let context = Context {
        storage,
        settings,
        account: &account,
    };
    let response = context.clerk(endpoint.as_str(), Method::POST, &jar).await?;
    let rotated = rotated_cookie(&response);
    persist_rotation(storage, &id, &mut account, &mut jar, rotated).await?;
    let data = json_response(response).await?;
    let jwt = nonempty(&data["jwt"])
        .filter(|jwt| jwt.bytes().all(|byte| byte.is_ascii_graphic()))
        .ok_or_else(|| Failure::upstream("UC Clerk token mint returned no valid JWT"))?;
    let uid = crate::provider_oauth::token_claims(jwt)["uid"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    Ok(Session {
        account,
        jwt: jwt.to_owned(),
        uid,
    })
}

fn cookie_jar(header: &str) -> Value {
    Value::Object(
        header
            .split(';')
            .filter_map(|entry| {
                let (key, value) = entry.split_once('=')?;
                let key = key.trim();
                (!key.is_empty()).then(|| (key.to_owned(), json!(value.trim())))
            })
            .collect::<Map<_, _>>(),
    )
}

fn cookie_header(jar: &Value) -> Result<header::HeaderValue, Failure> {
    let serialized = jar
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| Some(format!("{name}={}", value.as_str()?)))
        .collect::<Vec<_>>()
        .join("; ");
    let mut header = header::HeaderValue::from_str(&serialized)
        .map_err(|_| Failure::new(StatusCode::UNAUTHORIZED, "Invalid UC session cookie"))?;
    header.set_sensitive(true);
    Ok(header)
}

fn rotated_cookie(response: &Response) -> Option<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|header| {
            let (name, value) = header.to_str().ok()?.split(';').next()?.split_once('=')?;
            let value = value.trim();
            (name.trim() == "__client" && !value.is_empty()).then(|| value.to_owned())
        })
        .next()
}

async fn persist_rotation(
    storage: &Storage,
    id: &str,
    account: &mut Value,
    jar: &mut Value,
    rotated: Option<String>,
) -> Result<(), Failure> {
    let Some(value) = rotated.filter(|value| jar["__client"] != *value) else {
        return Ok(());
    };
    jar["__client"] = json!(value);
    let value = cookie_header(jar)?
        .to_str()
        .map_err(|_| Failure::new(StatusCode::UNAUTHORIZED, "Invalid UC rotated cookie"))?
        .to_owned();
    let id = id.to_owned();
    let prior = account["apiKey"].clone();
    let changed = storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(current) = by_id(&tx, &id)?
                .filter(|row| supports(row) && row["isActive"] != false && row["apiKey"] == prior)
            else {
                return Ok(None);
            };
            let updated = patch(current, object(json!({"apiKey":value})), &now());
            write(&tx, &updated)?;
            tx.commit()?;
            Ok(Some(updated))
        })
        .await
        .ok()
        .flatten()
        .ok_or_else(|| {
            Failure::new(
                StatusCode::CONFLICT,
                "UC credentials changed during session renewal",
            )
        })?;
    *account = changed;
    Ok(())
}

struct Context<'a> {
    storage: &'a Storage,
    settings: &'a Value,
    account: &'a Value,
}

impl Context<'_> {
    async fn fixed(&self, endpoint: &str, method: Method) -> Result<RequestBuilder, Failure> {
        let route = Delivery::select(self.storage, self.settings, self.account, endpoint)
            .await
            .map_err(|_| Failure::route())?;
        let client = route
            .client(CLIENT.as_ref().ok_or_else(Failure::route)?, TIMEOUT)
            .ok_or_else(Failure::route)?;
        let url = if route.use_fixture() {
            let original = Url::parse(endpoint).map_err(|_| Failure::route())?;
            let host = original.host_str().ok_or_else(Failure::route)?;
            let mut url = Url::parse(&format!(
                "http://127.0.0.1:52989/uc/{host}{}",
                original.path()
            ))
            .map_err(|_| Failure::route())?;
            url.set_query(original.query());
            url
        } else {
            Url::parse(endpoint).map_err(|_| Failure::route())?
        };
        Ok(client
            .request(method, url)
            .header(header::USER_AGENT, "9Router")
            .header(header::ORIGIN, ORIGIN)
            .header(header::REFERER, format!("{ORIGIN}/"))
            .header(header::ACCEPT, "application/json"))
    }

    async fn clerk(
        &self,
        endpoint: &str,
        method: Method,
        jar: &Value,
    ) -> Result<Response, Failure> {
        let mut request = self
            .fixed(endpoint, method.clone())
            .await?
            .header(header::COOKIE, cookie_header(jar)?);
        if method == Method::POST {
            request = request
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body("");
        }
        let response = request
            .send()
            .await
            .map_err(|_| Failure::upstream("UC Clerk session request failed"))?;
        if !response.status().is_success() {
            return Err(http_error(response.status(), "Clerk session request"));
        }
        Ok(response)
    }

    async fn media(
        &self,
        endpoint: &str,
        jwt: &str,
        body: Value,
        operation: &str,
    ) -> Result<Value, Failure> {
        let response = self
            .fixed(endpoint, Method::POST)
            .await?
            .bearer_auth(jwt)
            .json(&body)
            .send()
            .await
            .map_err(|_| Failure::upstream(format!("UC {operation} failed")))?;
        if !response.status().is_success() {
            return Err(http_error(response.status(), operation));
        }
        json_response(response).await
    }

    async fn asset(
        &self,
        reference: &str,
        provider_url: bool,
    ) -> Result<(Client, Url, bool), Failure> {
        let url = Url::parse(reference).map_err(|_| Failure::upstream("Invalid UC media URL"))?;
        if reference.len() > 8192
            || url.scheme() != "https"
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(Failure::upstream("Invalid UC media URL"));
        }
        let host = url
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| Failure::upstream("UC media host is missing"))?;
        let route = Delivery::select(self.storage, self.settings, self.account, reference)
            .await
            .map_err(|_| Failure::route())?;
        if !provider_url && matches!(route, Delivery::Tunnel(_)) {
            return Err(Failure::bad(
                "UC image inputs require embedded data on a proxied connection",
            ));
        }
        if route.use_fixture() && host == "uc-assets.fixture.invalid" {
            let mut local = Url::parse(&format!("http://127.0.0.1:52989/uc-assets{}", url.path()))
                .map_err(|_| Failure::route())?;
            local.set_query(url.query());
            return Ok((
                CLIENT.as_ref().ok_or_else(Failure::route)?.clone(),
                local,
                true,
            ));
        }
        let resolved =
            tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, 443)))
                .await
                .map_err(|_| Failure::upstream("UC media URL resolution timed out"))?
                .map_err(|_| Failure::upstream("Could not resolve UC media host"))?;
        let mut addresses = Vec::new();
        for address in resolved {
            let ip = crate::chat_prefetch::public_address(address.ip())
                .ok_or_else(|| Failure::upstream("UC media URL targets a non-public address"))?;
            addresses.push(SocketAddr::from((ip, 443)));
        }
        if addresses.is_empty() {
            return Err(Failure::upstream("UC media host returned no addresses"));
        }
        let direct = http_client(TIMEOUT)
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|_| Failure::route())?;
        let client = route.client(&direct, TIMEOUT).ok_or_else(Failure::route)?;
        Ok((client, url, false))
    }

    async fn download(&self, url: &str, limit: usize, timeout: Duration) -> Result<Media, Failure> {
        let (client, url, fixture) = self.asset(url, true).await?;
        let pause = if fixture {
            Duration::from_millis(10)
        } else {
            Duration::from_secs(5)
        };
        tokio::time::timeout(timeout, async {
            loop {
                tokio::time::sleep(pause).await;
                let probe = client
                    .head(url.clone())
                    .header(header::REFERER, format!("{ORIGIN}/"))
                    .send()
                    .await;
                if probe.is_ok_and(|response| response.status() == StatusCode::OK) {
                    break;
                }
            }
            let response = client
                .get(url)
                .header(header::REFERER, format!("{ORIGIN}/"))
                .send()
                .await
                .map_err(|_| Failure::upstream("UC result download failed"))?;
            if !response.status().is_success() {
                return Err(http_error(response.status(), "result download"));
            }
            let mime = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .filter(|value| !value.is_empty())
                .unwrap_or("application/octet-stream")
                .to_owned();
            let bytes = response_bytes(response, limit).await?;
            Ok(Media { bytes, mime })
        })
        .await
        .map_err(|_| {
            Failure::new(
                StatusCode::GATEWAY_TIMEOUT,
                "UC result did not become available in time",
            )
        })?
    }

    async fn image_input(&self, input: &str) -> Result<Vec<u8>, Failure> {
        if input.starts_with("http://") || input.starts_with("https://") {
            let (client, url, _) = self.asset(input, false).await?;
            let response = client
                .get(url)
                .header(header::ACCEPT, "image/*")
                .send()
                .await
                .map_err(|_| Failure::upstream("UC input image download failed"))?;
            if !response.status().is_success() {
                return Err(http_error(response.status(), "input image download"));
            }
            return response_bytes(response, MAX_IMAGE).await;
        }
        let value = input
            .strip_prefix("data:")
            .and_then(|data| data.split_once(";base64,"))
            .map_or(input, |(_, data)| data);
        if value.len() > MAX_IMAGE.div_ceil(3) * 4 {
            return Err(Failure::bad("UC input image is too large"));
        }
        let bytes = STANDARD
            .decode(value)
            .or_else(|_| STANDARD_NO_PAD.decode(value))
            .map_err(|_| Failure::bad("UC image-to-video could not decode the input image"))?;
        if bytes.is_empty() || bytes.len() > MAX_IMAGE {
            return Err(Failure::bad("UC input image is empty or too large"));
        }
        Ok(bytes)
    }
}

fn http_error(status: StatusCode, operation: &str) -> Failure {
    let status = if status.is_redirection() {
        StatusCode::BAD_GATEWAY
    } else {
        status
    };
    Failure::new(
        status,
        format!("UC {operation} failed (HTTP {})", status.as_u16()),
    )
}

async fn json_response(response: Response) -> Result<Value, Failure> {
    let bytes = response_bytes(response, MAX_JSON).await?;
    serde_json::from_slice(&bytes).map_err(|_| Failure::upstream("Invalid UC JSON response"))
}

async fn response_bytes(mut response: Response, limit: usize) -> Result<Vec<u8>, Failure> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(Failure::upstream("UC response is too large"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Failure::upstream("Could not read UC response"))?
    {
        if chunk.len() > limit - bytes.len() {
            return Err(Failure::upstream("UC response is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

struct Media {
    bytes: Vec<u8>,
    mime: String,
}
