use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use rand::Rng;
use reqwest::{Client, Method, RequestBuilder, header::HeaderValue};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::chat_output::js_text;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::proxy_pools::{self, managed_relay};
use crate::storage::Storage;

const MAX_CLIENTS: usize = 20;
type PublicPoolCursors = HashMap<(PathBuf, String), usize>;
static PUBLIC_POOL_CURSORS: LazyLock<Mutex<PublicPoolCursors>> = LazyLock::new(Mutex::default);
static MANAGED_RELAY: LazyLock<Result<Client, reqwest::Error>> = LazyLock::new(|| {
    Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
});

#[derive(Clone, Default)]
pub struct Clients {
    clients: Arc<Mutex<VecDeque<(String, bool, Client)>>>,
}

impl Clients {
    pub fn get(
        &self,
        address: &str,
        fresh: bool,
        timeout: Duration,
    ) -> Result<Client, reqwest::Error> {
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some((_, _, client)) = clients
            .iter()
            .find(|(url, is_fresh, _)| url == address && *is_fresh == fresh)
        {
            return Ok(client.clone());
        }
        let proxy = reqwest::Proxy::all(address)?;
        let mut builder = Client::builder()
            .connect_timeout(timeout)
            .pool_idle_timeout(Duration::from_secs(4))
            .redirect(crate::upstream::redirects())
            .no_proxy()
            .proxy(proxy);
        if fresh {
            builder = builder.pool_max_idle_per_host(0);
        }
        let client = builder.build()?;
        if clients.len() == MAX_CLIENTS {
            clients.pop_front();
        }
        clients.push_back((address.to_owned(), fresh, client.clone()));
        Ok(client)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum Route {
    Direct,
    Relay {
        url: String,
        target: String,
        path: String,
        authorization: Option<HeaderValue>,
    },
    Proxy {
        url: String,
        strict: bool,
    },
    MissingStrict,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Selection {
    Routed(Route),
    ExplicitBypass,
}

impl Route {
    pub(super) fn relay_target(&self, endpoint: &Url) -> Option<Self> {
        let Self::Relay {
            url,
            target,
            authorization,
            ..
        } = self
        else {
            return None;
        };
        if endpoint.origin().ascii_serialization() != *target {
            return None;
        }
        let mut path = endpoint.path().to_owned();
        if let Some(query) = endpoint.query() {
            path.push('?');
            path.push_str(query);
        }
        Some(Self::Relay {
            url: url.clone(),
            target: target.clone(),
            path,
            authorization: authorization.clone(),
        })
    }

    pub(super) fn relay_secret(&self) -> Option<&str> {
        match self {
            Self::Relay {
                authorization: Some(value),
                ..
            } => value.to_str().ok().filter(|secret| !secret.is_empty()),
            _ => None,
        }
    }

    pub(super) fn relay_identity(&self) -> Option<[u8; 32]> {
        let Self::Relay {
            url, authorization, ..
        } = self
        else {
            return None;
        };
        let mut identity = Sha256::new();
        identity.update(url.as_bytes());
        if let Some(token) = authorization {
            identity.update([0]);
            identity.update(token.as_bytes());
        }
        Some(identity.finalize().into())
    }

    pub fn relay(&self, request: RequestBuilder) -> RequestBuilder {
        let Self::Relay {
            url,
            target,
            path,
            authorization,
        } = self
        else {
            return request;
        };
        let request = match authorization {
            None => request,
            Some(authorization) => {
                let (client, request) = request.build_split();
                let (Ok(mut request), Ok(relay_client)) = (request, MANAGED_RELAY.as_ref()) else {
                    return client.request(Method::GET, "");
                };
                if Url::parse(url).ok().as_ref() != Some(request.url()) {
                    return client.request(Method::GET, "");
                }
                let headers = request.headers_mut();
                headers.remove("x-relay-target");
                headers.remove("x-relay-path");
                headers.insert("x-relay-auth", authorization.clone());
                RequestBuilder::from_parts(relay_client.clone(), request)
            }
        };
        request
            .header("x-relay-target", target)
            .header("x-relay-path", path)
    }
}

pub(super) fn pool_relay(pool: &Value, address: &str, dest: &Url) -> Result<Route, ()> {
    let (url, authorization) = if managed_relay::managed(pool) {
        if !managed_relay::public_target(dest)
            || Url::parse(address).is_ok_and(|relay| relay.origin() == dest.origin())
        {
            return Err(());
        }
        let (url, secret) = managed_relay::validated_relay(pool, address).ok_or(())?;
        (url, Some(secret))
    } else {
        (address.to_owned(), None)
    };
    let mut path = dest.path().to_owned();
    if let Some(query) = dest.query() {
        path.push('?');
        path.push_str(query);
    }
    Ok(Route::Relay {
        url,
        target: dest.origin().ascii_serialization(),
        path,
        authorization,
    })
}

pub async fn select(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    endpoint: &str,
) -> Result<Route, ()> {
    Ok(
        match select_with_bypass(storage, settings, account, endpoint).await? {
            Selection::Routed(route) => route,
            Selection::ExplicitBypass => Route::Direct,
        },
    )
}

pub(super) async fn select_with_bypass(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    endpoint: &str,
) -> Result<Selection, ()> {
    use Selection::{ExplicitBypass, Routed};
    let details = &account["providerSpecificData"];
    let pool_id = normalized(&details["proxyPoolId"]);
    let pool_id = (!pool_id.is_empty() && pool_id != "__none__").then_some(pool_id);
    let pool = if let Some(id) = pool_id.clone() {
        storage
            .run(false, move |db| proxy_pools::by_id(db, &id))
            .await
            .map_err(|_| ())?
    } else {
        None
    }
    .unwrap_or_default();
    let pool_url = normalized(&pool["proxyUrl"]);
    let active_pool = pool["isActive"] == true && !pool_url.is_empty();
    let strict = pool["strictProxy"] == true;
    if pool["isActive"] == true && managed_relay::managed(&pool) {
        let dest = Url::parse(endpoint).map_err(|_| ())?;
        return pool_relay(&pool, &pool_url, &dest).map(Routed);
    }
    let Ok(dest) = Url::parse(endpoint) else {
        return Ok(Routed(if strict {
            Route::MissingStrict
        } else {
            Route::Direct
        }));
    };
    if active_pool && pool["type"].as_str().is_some_and(proxy_pools::relay) {
        return pool_relay(&pool, &pool_url, &dest).map(Routed);
    }

    let (enabled, url, no_proxy) = if active_pool {
        (true, pool_url, normalized(&pool["noProxy"]))
    } else {
        (
            details["connectionProxyEnabled"] == true,
            normalized(&details["connectionProxyUrl"]),
            normalized(&details["connectionNoProxy"]),
        )
    };
    let intended = pool_id.is_some() || enabled || !url.is_empty();
    let skipped_account = match (enabled && !url.is_empty()).then(|| normalize_proxy(&url)) {
        Some(url) if !bypass(&dest, &no_proxy) => return Ok(Routed(Route::Proxy { url, strict })),
        skipped => skipped,
    };
    let skipped_environment = match environment(settings, &dest) {
        Some((url, false)) => return Ok(Routed(Route::Proxy { url, strict })),
        candidate => candidate.map(|(url, _)| url),
    };
    if strict && intended {
        Ok(Routed(Route::MissingStrict))
    } else if !truthy(&details["strictProxy"])
        && skipped_account
            .or(skipped_environment)
            .as_deref()
            .and_then(tunnel_proxy_url)
            .is_some()
    {
        Ok(ExplicitBypass)
    } else {
        Ok(Routed(Route::Direct))
    }
}

pub(super) fn account_requires_tunnel(account: &Value) -> bool {
    let details = &account["providerSpecificData"];
    ["connectionProxyEnabled", "strictProxy", "vercelRelayUrl"]
        .iter()
        .any(|field| truthy(&details[*field]))
        || truthy(&details["proxyPoolId"])
            && details["proxyPoolId"].as_str().map(js_trim) != Some("__none__")
}

pub(crate) fn supports_public_proxy_pool(provider: &str) -> bool {
    matches!(provider, "opencode" | "edge-tts" | "google-tts" | "searxng")
}

pub(crate) async fn public_proxy_account(
    storage: &Storage,
    settings: &Value,
    provider: &str,
) -> Result<Value, ()> {
    let configured = &settings["providerStrategies"][provider];
    if !uses_public_proxy_rotation(settings, provider) {
        return Ok(json!({"providerSpecificData":{"proxyPoolId":configured["proxyPoolId"]}}));
    }
    let allow_relay = !matches!(provider, "edge-tts" | "google-tts");
    let ids = storage
        .run(false, move |db| {
            proxy_pools::rotation_pool_ids(db, allow_relay)
        })
        .await
        .map_err(|_| ())?;
    let chosen = if ids.len() <= 1 {
        ids.first()
    } else {
        let index = match configured["rotateStrategy"].as_str() {
            Some("round-robin") => {
                let mut cursors = PUBLIC_POOL_CURSORS.lock().map_err(|_| ())?;
                let cursor = cursors
                    .entry((storage.root().to_path_buf(), provider.to_owned()))
                    .or_insert(0);
                let index = *cursor % ids.len();
                *cursor = (index + 1) % ids.len();
                index
            }
            Some("random") => rand::thread_rng().gen_range(0..ids.len()),
            _ => 0,
        };
        ids.get(index)
    };
    Ok(json!({"providerSpecificData":{"proxyPoolId":chosen}}))
}

pub(crate) fn uses_public_proxy_rotation(settings: &Value, provider: &str) -> bool {
    let strategy = &settings["providerStrategies"][provider]["rotateStrategy"];
    truthy(strategy) && strategy != "none"
}

pub(super) async fn vendor_metadata_proxy(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    endpoint: &str,
) -> Result<Option<Url>, ()> {
    if truthy(&account["providerSpecificData"]["vercelRelayUrl"]) {
        return Err(());
    }
    match select(storage, settings, account, endpoint).await? {
        Route::Direct
            if !account_requires_tunnel(account) && !truthy(&settings["outboundProxyEnabled"]) =>
        {
            Ok(None)
        }
        Route::Proxy { url, .. } => Url::parse(&url)
            .ok()
            .filter(|proxy| {
                matches!(proxy.scheme(), "http" | "https" | "socks5" | "socks5h")
                    && proxy.has_host()
                    && matches!(proxy.path(), "" | "/")
                    && proxy.query().is_none()
                    && proxy.fragment().is_none()
            })
            .map(Some)
            .ok_or(()),
        _ => Err(()),
    }
}

pub(super) fn vendor_metadata_client(
    proxy: Url,
    builder: reqwest::ClientBuilder,
) -> Option<Client> {
    #[cfg(debug_assertions)]
    let fixture_ca = (std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        && proxy.host_str() == Some("127.0.0.1"))
    .then(|| std::env::var_os("NINEROUTER_MODEL_LOCAL_PROXY_CA"))
    .flatten();
    let builder = builder.proxy(reqwest::Proxy::all(proxy).ok()?);
    #[cfg(debug_assertions)]
    let builder = if let Some(path) = fixture_ca {
        let pem = std::fs::read(path).ok()?;
        builder.add_root_certificate(reqwest::Certificate::from_pem(&pem).ok()?)
    } else {
        builder
    };
    builder.build().ok()
}

fn normalize_proxy(address: &str) -> String {
    if Url::parse(address).is_ok_and(|url| url.has_host()) {
        address.to_owned()
    } else {
        format!("http://{address}")
    }
}

pub(super) fn tunnel_proxy_url(address: &str) -> Option<Url> {
    if address.chars().any(char::is_control) {
        return None;
    }
    Url::parse(address).ok().filter(|url| {
        matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
            && url.has_host()
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

pub(super) fn tunnel_credentials(url: &Url) -> Result<Option<(String, String)>, ()> {
    if url.username().is_empty() && url.password().is_none() {
        return Ok(None);
    }
    let decode = |value: &str| {
        percent_encoding::percent_decode_str(value)
            .decode_utf8()
            .map(|value| value.into_owned())
            .map_err(|_| ())
    };
    Ok(Some((
        decode(url.username())?,
        decode(url.password().unwrap_or_default())?,
    )))
}

fn bypass(dest: &Url, no_proxy: &str) -> bool {
    let Some(host) = dest.host_str() else {
        return false;
    };
    no_proxy
        .split(',')
        .map(js_trim)
        .filter(|pattern| !pattern.is_empty())
        .any(|pattern| {
            let pattern = pattern.to_ascii_lowercase();
            let pattern = pattern.strip_prefix('.').unwrap_or(&pattern);
            pattern == "*"
                || host == pattern
                || host
                    .strip_suffix(pattern)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
}

fn normalized(value: &Value) -> String {
    if value.is_null() {
        return String::new();
    }
    js_trim(&js_text(value)).to_owned()
}

fn env(keys: &[&str]) -> Option<String> {
    if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") {
        return None;
    }
    keys.iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
}

fn environment(settings: &Value, dest: &Url) -> Option<(String, bool)> {
    let enabled = truthy(&settings["outboundProxyEnabled"]);
    let configured = normalized(&settings["outboundProxyUrl"]);
    let configured = enabled
        .then(|| Url::parse(&configured).ok())
        .flatten()
        .filter(|url| {
            !configured.contains(['\r', '\n', '`', '$'])
                && matches!(
                    url.scheme(),
                    "http" | "https" | "socks5" | "socks5h" | "socks4" | "socks4a"
                )
        });
    let override_bypass = normalized(&settings["outboundNoProxy"]);
    let bypass_rules = if enabled && !override_bypass.is_empty() {
        override_bypass
    } else {
        env(&["NO_PROXY", "no_proxy"]).unwrap_or_default()
    };
    let address = configured.map(String::from).or_else(|| {
        let keys: &[&str] = if dest.scheme() == "https" {
            &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        } else {
            &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
        };
        env(keys).map(|url| normalize_proxy(js_trim(&url)))
    });
    address.map(|url| (url, bypass(dest, &bypass_rules)))
}
