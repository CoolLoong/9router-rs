use std::sync::LazyLock;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, RequestBuilder, StatusCode, header};
use serde_json::Value;

use crate::provider_http::{Delivery, http_client};
use crate::storage::Storage;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";
static CLIENT: LazyLock<Option<Client>> = LazyLock::new(|| {
    http_client(Duration::from_secs(15))
        .user_agent(USER_AGENT)
        .build()
        .ok()
});

pub(crate) enum Identity {
    Valid(String),
    Invalid(&'static str),
    Unknown,
}

pub(crate) async fn test_perplexity(storage: &Storage, account: &Value) -> Option<String> {
    const ENDPOINT: &str = "https://www.perplexity.ai/api/auth/session";
    const FIXTURE: &str = "http://127.0.0.1:52989/web-identity/perplexity/session";
    const INVALID: &str = "Invalid session cookie";
    const EXPIRED: &str = "Session expired — re-paste cookie";
    const FETCH_FAILED: &str = "fetch failed";
    const TIMEOUT: Duration = Duration::from_secs(15);

    let Some(cookie) = account["apiKey"].as_str().and_then(perplexity_cookie) else {
        return Some(INVALID.into());
    };
    let Some(settings) = storage.read_settings().await else {
        return Some(FETCH_FAILED.into());
    };
    let Ok(delivery) = Delivery::select(storage, &settings, account, ENDPOINT).await else {
        return Some(FETCH_FAILED.into());
    };
    let Ok(direct) = http_client(TIMEOUT).build() else {
        return Some(FETCH_FAILED.into());
    };
    let Some(client) = delivery.client(&direct, TIMEOUT) else {
        return Some(FETCH_FAILED.into());
    };
    let url = if delivery.use_fixture() {
        FIXTURE
    } else {
        ENDPOINT
    };
    let response = client
        .get(url)
        .header(header::COOKIE, cookie)
        .header(header::ACCEPT, "application/json")
        .header(header::USER_AGENT, "9Router")
        .send()
        .await;
    let response = match response {
        Ok(response) if response.status().is_success() => response,
        Ok(_) => return Some(INVALID.into()),
        Err(error) if error.is_timeout() => {
            return Some("The operation was aborted due to timeout".into());
        }
        Err(_) => return Some(FETCH_FAILED.into()),
    };
    let user = response_bytes(response)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|json| crate::provider_connections::truthy(&json["user"]));
    (!user).then(|| EXPIRED.into())
}

pub(crate) fn perplexity_cookie(input: &str) -> Option<header::HeaderValue> {
    const PREFIX: &str = "__Secure-next-auth.session-token=";
    let token = input.strip_prefix(PREFIX).unwrap_or(input);
    session_cookie("__Secure-next-auth.session-token", token)
}

fn session_cookie(name: &str, token: &str) -> Option<header::HeaderValue> {
    if token.is_empty()
        || !token.bytes().all(
            |byte| matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e),
        )
    {
        return None;
    }
    let mut cookie = header::HeaderValue::from_str(&format!("{name}={token}")).ok()?;
    cookie.set_sensitive(true);
    Some(cookie)
}

pub(crate) async fn inspect_grok(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    credential: &str,
) -> Identity {
    const ENDPOINT: &str = "https://grok.com/api/auth/session";
    const FIXTURE: &str = "http://127.0.0.1:52989/web-identity/grok/session";
    const TIMEOUT: Duration = Duration::from_secs(15);
    let credential = normalize("grok-web", credential);
    let token = cookie(&credential, "sso").or_else(|| {
        (credential.contains('.')
            && !credential.trim_end_matches('=').contains('=')
            && !credential.contains(';'))
        .then_some(credential.as_str())
    });
    let Some(cookie) = token.and_then(|token| session_cookie("sso", token)) else {
        return Identity::Invalid("Missing or invalid sso cookie");
    };
    let Ok(delivery) = Delivery::select(storage, settings, account, ENDPOINT).await else {
        return Identity::Unknown;
    };
    let Ok(direct) = http_client(TIMEOUT).build() else {
        return Identity::Unknown;
    };
    let Some(client) = delivery.client(&direct, TIMEOUT) else {
        return Identity::Unknown;
    };
    let url = if delivery.use_fixture() {
        FIXTURE
    } else {
        ENDPOINT
    };
    let Ok(response) = client
        .get(url)
        .header(header::COOKIE, cookie)
        .header(header::ACCEPT, "application/json")
        .header(header::USER_AGENT, "9Router")
        .send()
        .await
    else {
        return Identity::Unknown;
    };
    if response.headers().contains_key("cf-mitigated") {
        return Identity::Unknown;
    }
    if response.status() == StatusCode::UNAUTHORIZED {
        return Identity::Invalid("Invalid SSO cookie");
    }
    if !response.status().is_success() {
        return Identity::Unknown;
    }
    let Some(data) = response_bytes(response)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return Identity::Unknown;
    };
    match data["status"].as_str() {
        Some("authenticated") => Identity::Valid(
            data["session"]["email"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        ),
        Some("unauthenticated") => Identity::Invalid("Grok session is not signed in"),
        _ => Identity::Unknown,
    }
}

pub(crate) async fn inspect(
    storage: &Storage,
    account: &Value,
    provider: &str,
    credential: &str,
) -> Identity {
    if provider == "grok-web" {
        let Some(settings) = storage.read_settings().await else {
            return Identity::Unknown;
        };
        return inspect_grok(storage, &settings, account, credential).await;
    }
    let Some(client) = CLIENT.as_ref() else {
        return Identity::Unknown;
    };
    if credential.trim().is_empty() {
        return Identity::Invalid(match provider {
            "deepseek-web" => "Missing userToken",
            "chatgpt-web" => "Missing session cookie",
            "gemini-web" => "Missing __Secure-1PSID cookie",
            "muse-web" => "Missing hatch_sess cookie",
            "uc-web" => "Missing __client cookie",
            _ => "Missing session credentials",
        });
    }
    let Some(settings) = storage.read_settings().await else {
        return Identity::Unknown;
    };
    let route = Probe {
        storage,
        account,
        settings,
    };
    let credential = normalize(provider, credential);
    let credential = credential.as_str();
    let (request, invalid) = match provider {
        "deepseek-web" => (
            client
                .get("https://chat.deepseek.com/api/v0/users/current")
                .bearer_auth(credential)
                .header(header::ACCEPT, "*/*")
                .header(header::ORIGIN, "https://chat.deepseek.com")
                .header(header::REFERER, "https://chat.deepseek.com/")
                .header("X-Client-Bundle-Id", "com.deepseek.chat")
                .header("X-Client-Platform", "web")
                .header("X-Client-Version", "2.0.0"),
            "userToken is invalid or expired",
        ),
        "chatgpt-web" => (
            client
                .get("https://chatgpt.com/api/auth/session")
                .header(header::COOKIE, credential)
                .header(header::ACCEPT, "application/json"),
            "Session cookie is invalid or expired",
        ),
        "gemini-web" => {
            if cookie(credential, "__Secure-1PSID").is_none() {
                return Identity::Invalid("Missing __Secure-1PSID cookie");
            }
            let cookie = ["__Secure-1PSID", "__Secure-1PSIDTS"]
                .into_iter()
                .filter_map(|name| cookie(credential, name).map(|value| format!("{name}={value}")))
                .collect::<Vec<_>>()
                .join("; ");
            (
                client
                    .get("https://gemini.google.com/app")
                    .header(header::COOKIE, cookie)
                    .header(header::ACCEPT, "text/html,application/xhtml+xml"),
                "__Secure-1PSID cookie is invalid or expired",
            )
        }
        "muse-web" => (
            client
                .get("https://muse.ai/api/session")
                .header(header::COOKIE, credential)
                .header(header::ACCEPT, "application/json")
                .header(header::ORIGIN, "https://muse.ai")
                .header(header::REFERER, "https://muse.ai/thread/new"),
            "muse.ai session cookie is invalid or expired",
        ),
        "uc-web" => return uncensored(client, &route, credential).await,
        _ => return Identity::Unknown,
    };
    let body = match route.fetch(request, invalid).await {
        Ok(body) => body,
        Err(outcome) => return outcome,
    };
    if provider == "muse-web" {
        return Identity::Valid(String::new());
    }
    if provider == "gemini-web" {
        static MARKER: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(r#""SNlM0e"\s*:\s*"[^"]+""#).expect("Gemini Web session marker")
        });
        return if MARKER.is_match(&body) {
            Identity::Valid(String::new())
        } else {
            Identity::Invalid("Gemini session is not signed in")
        };
    }
    let Ok(data) = serde_json::from_str::<Value>(&body) else {
        return Identity::Unknown;
    };
    let email = match provider {
        "deepseek-web" => {
            if data["code"] == 40003 || data["code"] == "40003" {
                return Identity::Invalid(invalid);
            }
            let account = &data["data"]["biz_data"];
            if !crate::provider_connections::truthy(&account["token"]) {
                return Identity::Invalid("DeepSeek rejected the userToken");
            }
            &account["email"]
        }
        "chatgpt-web" => {
            let email = &data["user"]["email"];
            if email.as_str().is_none_or(str::is_empty) {
                return Identity::Invalid("ChatGPT session is not signed in");
            }
            email
        }
        _ => return Identity::Unknown,
    };
    Identity::Valid(email.as_str().unwrap_or_default().to_owned())
}

struct Probe<'a> {
    storage: &'a Storage,
    settings: Value,
    account: &'a Value,
}

impl Probe<'_> {
    async fn fetch(
        &self,
        request: RequestBuilder,
        invalid: &'static str,
    ) -> Result<String, Identity> {
        let (client, request) = request.build_split();
        let request = request.map_err(|_| Identity::Unknown)?;
        let delivery = Delivery::select(
            self.storage,
            &self.settings,
            self.account,
            request.url().as_str(),
        )
        .await
        .map_err(|_| Identity::Unknown)?;
        let client = delivery
            .client(&client, Duration::from_secs(15))
            .ok_or(Identity::Unknown)?;
        let response = client
            .execute(request)
            .await
            .map_err(|_| Identity::Unknown)?;
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(Identity::Invalid(invalid));
        }
        if !response.status().is_success() {
            return Err(Identity::Unknown);
        }
        let bytes = response_bytes(response)
            .await
            .map_err(|_| Identity::Unknown)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

async fn response_bytes(mut response: reqwest::Response) -> Result<Vec<u8>, reqwest::Error> {
    const LIMIT: usize = 2 * 1024 * 1024;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > LIMIT - bytes.len() {
            bytes.extend_from_slice(&chunk[..LIMIT - bytes.len()]);
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(crate) fn normalize(provider: &str, credential: &str) -> String {
    let mut raw = credential.trim();
    if provider == "deepseek-web" {
        return serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|data| data["value"].as_str().map(str::to_owned))
            .unwrap_or_else(|| raw.into());
    }
    if let Some(rest) = raw.get(7..)
        && raw
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        raw = rest.trim_start();
    }
    if let Some(rest) = raw.get(7..)
        && raw
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
    {
        raw = rest.trim();
    }
    if let Ok(data) = serde_json::from_str::<Value>(raw) {
        if let Some(rows) = data.as_array().or_else(|| data["cookies"].as_array()) {
            return rows
                .iter()
                .filter_map(|row| {
                    Some(format!(
                        "{}={}",
                        row["name"].as_str()?,
                        row["value"].as_str()?
                    ))
                })
                .collect::<Vec<_>>()
                .join("; ");
        }
        if provider == "gemini-web"
            && let Some(cookies) = data["cookies"].as_object().or_else(|| data.as_object())
        {
            return cookies
                .iter()
                .filter_map(|(name, value)| Some(format!("{name}={}", value.as_str()?.trim())))
                .collect::<Vec<_>>()
                .join("; ");
        }
    }
    if !raw.contains('=') {
        let key = match provider {
            "chatgpt-web" => "__Secure-next-auth.session-token",
            "gemini-web" => "__Secure-1PSID",
            "grok-web" => "sso",
            _ => return raw.into(),
        };
        format!("{key}={raw}")
    } else {
        raw.into()
    }
}

fn cookie<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    header
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, value)| name.trim() == key && !value.is_empty())
        .map(|(_, value)| value.trim())
        .next_back()
}

fn claims(token: &str) -> Option<Value> {
    let payload = token
        .trim_start_matches("base64-")
        .split('.')
        .nth(1)?
        .trim_end_matches('=');
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
}

async fn uncensored(client: &Client, route: &Probe<'_>, credential: &str) -> Identity {
    const ORIGIN: &str = "https://uncensored.com";
    const INVALID: &str = "Clerk session is invalid or expired";
    let Some(cookie) = cookie(credential, "__client") else {
        return Identity::Invalid("Missing __client cookie");
    };
    let cookie = format!("__client={cookie}");
    let claims_cookie = claims(cookie.trim_start_matches("__client="));
    let sid = claims_cookie
        .as_ref()
        .and_then(|claims| claims["sid"].as_str())
        .filter(|sid| !sid.is_empty());
    let sid = if let Some(sid) = sid {
        sid.to_owned()
    } else {
        let request = client
            .get("https://clerk.uncensored.com/v1/client?_clerk_js_version=5.127.1")
            .header(header::COOKIE, &cookie)
            .header(header::ACCEPT, "application/json")
            .header(header::ORIGIN, ORIGIN)
            .header(header::REFERER, "https://uncensored.com/");
        let body = match route.fetch(request, INVALID).await {
            Ok(body) => body,
            Err(outcome) => return outcome,
        };
        let Ok(data) = serde_json::from_str::<Value>(&body) else {
            return Identity::Unknown;
        };
        let sessions = data
            .get("response")
            .filter(|value| !value.is_null())
            .or_else(|| data.get("client").filter(|value| !value.is_null()))
            .unwrap_or(&data)["sessions"]
            .as_array();
        let sid = sessions
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["status"] == "active")
                    .or_else(|| rows.first())
            })
            .and_then(|session| session["id"].as_str())
            .filter(|sid| !sid.is_empty());
        let Some(sid) = sid else {
            return Identity::Invalid("Not signed in to uncensored.com — use Login via Browser");
        };
        sid.to_owned()
    };
    let Ok(mut url) = reqwest::Url::parse("https://clerk.uncensored.com/v1/client/sessions") else {
        return Identity::Unknown;
    };
    let Ok(mut segments) = url.path_segments_mut() else {
        return Identity::Unknown;
    };
    segments.push(&sid).push("tokens");
    drop(segments);
    url.set_query(Some("_clerk_js_version=5.127.1"));
    let request = client
        .post(url)
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body("");
    let body = match route.fetch(request, INVALID).await {
        Ok(body) => body,
        Err(outcome) => return outcome,
    };
    let jwt = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|data| data["jwt"].as_str().map(str::to_owned));
    let Some(jwt) = jwt.filter(|jwt| !jwt.is_empty()) else {
        return Identity::Invalid("Clerk did not mint a session token");
    };
    let identity = claims(&jwt).and_then(|claims| {
        claims["uid"]
            .as_str()
            .filter(|uid| uid.contains('@'))
            .map(str::to_owned)
    });
    Identity::Valid(identity.unwrap_or_default())
}
