use std::time::Duration;

use reqwest::{Client, ClientBuilder, Method, RequestBuilder, Response, Url};
use serde_json::Value;

use crate::provider_connections::truthy;
use crate::proxy_transport::{self, Clients, Route, Selection};
use crate::storage::Storage;

pub(super) enum FetchError {
    Route,
    Http(reqwest::Error),
    Direct(reqwest::Error),
}

impl FetchError {
    pub(super) fn is_timeout(&self) -> bool {
        matches!(self, Self::Http(error) | Self::Direct(error) if error.is_timeout())
    }
}

pub(super) async fn fetch(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    clients: &Clients,
    builder: RequestBuilder,
    timeout: Duration,
) -> Result<Response, FetchError> {
    let (client, request) = builder.build_split();
    let request = request.map_err(FetchError::Http)?;

    let selection = select(storage, settings, account, request.url().as_str()).await?;
    send(clients, client, request, selection, timeout).await
}

pub(super) async fn select(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    url: &str,
) -> Result<Selection, FetchError> {
    let selection = proxy_transport::select_with_bypass(storage, settings, account, url)
        .await
        .map_err(|_| FetchError::Route)?;
    if (truthy(&account["providerSpecificData"]["vercelRelayUrl"])
        && !matches!(selection, Selection::Routed(Route::Relay { .. })))
        || (matches!(selection, Selection::Routed(Route::Direct))
            && (truthy(&settings["outboundProxyEnabled"])
                || proxy_transport::account_requires_tunnel(account)))
    {
        return Err(FetchError::Route);
    }
    Ok(selection)
}

pub(super) async fn select_authenticated(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    endpoint: &str,
) -> Result<Selection, FetchError> {
    let selected = select(storage, settings, account, endpoint).await?;
    match &selected {
        Selection::Routed(Route::MissingStrict) => Err(FetchError::Route),
        Selection::Routed(Route::Direct) if requires_protection(settings, account) => {
            Err(FetchError::Route)
        }
        Selection::Routed(relay @ Route::Relay { .. }) if relay.relay_secret().is_none() => {
            Err(FetchError::Route)
        }
        _ => Ok(selected),
    }
}

pub(super) fn same_egress(first: &Selection, next: &Selection) -> bool {
    match (first, next) {
        (
            Selection::Routed(first @ Route::Relay { .. }),
            Selection::Routed(next @ Route::Relay { .. }),
        ) => first.relay_identity() == next.relay_identity(),
        _ => first == next,
    }
}

pub(super) async fn send(
    clients: &Clients,
    client: reqwest::Client,
    mut request: reqwest::Request,
    selection: Selection,
    timeout: Duration,
) -> Result<Response, FetchError> {
    *request.timeout_mut() = Some(timeout);
    let direct = matches!(
        &selection,
        Selection::ExplicitBypass | Selection::Routed(Route::Direct)
    );
    let response = match &selection {
        Selection::ExplicitBypass | Selection::Routed(Route::Direct) => {
            client.execute(request).await
        }
        Selection::Routed(Route::Proxy { url, .. }) => {
            proxy_url(url).ok_or(FetchError::Route)?;
            clients
                .get(url, false, timeout)
                .map_err(FetchError::Http)?
                .execute(request)
                .await
        }
        Selection::Routed(route @ Route::Relay { .. }) => {
            relay_request(client, request, route)
                .ok_or(FetchError::Route)?
                .send()
                .await
        }
        _ => return Err(FetchError::Route),
    };
    response.map_err(if direct {
        FetchError::Direct
    } else {
        FetchError::Http
    })
}

pub(super) fn relay_request(
    client: Client,
    mut request: reqwest::Request,
    route: &Route,
) -> Option<RequestBuilder> {
    let Route::Relay { url, .. } = route else {
        return None;
    };
    *request.url_mut() = relay_url(url)?;
    Some(route.relay(RequestBuilder::from_parts(client, request)))
}

#[derive(PartialEq, Eq)]
pub(crate) enum ProtectedEgress {
    Bypass,
    Tunnel(Url),
    Relay(Route),
}

impl ProtectedEgress {
    pub(super) fn relay_secret(&self) -> Option<&str> {
        match self {
            Self::Relay(route) => route.relay_secret(),
            _ => None,
        }
    }
}

pub(super) async fn protected_egress(
    storage: &Storage,
    settings: &Value,
    account: &Value,
    endpoint: &str,
) -> Result<Option<ProtectedEgress>, FetchError> {
    match select(storage, settings, account, endpoint).await? {
        Selection::ExplicitBypass => Ok(Some(ProtectedEgress::Bypass)),
        Selection::Routed(Route::Proxy { url, .. }) => proxy_transport::tunnel_proxy_url(&url)
            .map(ProtectedEgress::Tunnel)
            .map(Some)
            .ok_or(FetchError::Route),
        Selection::Routed(route @ Route::Relay { .. }) => Ok(Some(ProtectedEgress::Relay(route))),
        Selection::Routed(Route::Direct) if !requires_protection(settings, account) => Ok(None),
        _ => Err(FetchError::Route),
    }
}

fn proxy_url(url: &str) -> Option<Url> {
    let proxy = Url::parse(url).ok()?;
    (proxy.has_host()
        && matches!(
            proxy.scheme(),
            "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
        ))
    .then_some(proxy)
}

fn relay_url(url: &str) -> Option<Url> {
    let destination = Url::parse(url).ok()?;
    (matches!(destination.scheme(), "http" | "https")
        && destination.has_host()
        && destination.username().is_empty()
        && destination.password().is_none())
    .then_some(destination)
}

pub(super) struct Bound {
    client: Client,
    endpoint: String,
    timeout: Duration,
    pub(super) route: Route,
    pub(super) protected: bool,
}

impl Bound {
    pub(super) fn same_route(&self, other: &Self) -> bool {
        self.route == other.route && self.protected == other.protected
    }

    pub(super) fn relay_secret(&self) -> Option<&str> {
        self.route.relay_secret()
    }

    pub(super) fn request(&self, method: Method) -> RequestBuilder {
        self.route.relay(
            self.client
                .request(method, &self.endpoint)
                .timeout(self.timeout),
        )
    }
}

pub(super) async fn bind(
    storage: &Storage,
    account: &Value,
    endpoint: &str,
    local: &str,
    direct: &Client,
    timeout: Duration,
    expected: Option<bool>,
) -> Option<Bound> {
    let settings = storage.read_settings().await?;
    let selection = select(storage, &settings, account, endpoint).await.ok()?;
    bind_selection(selection, endpoint, local, direct, timeout, expected)
}

pub(super) fn bind_selection(
    selection: Selection,
    endpoint: &str,
    local: &str,
    direct: &Client,
    timeout: Duration,
    expected: Option<bool>,
) -> Option<Bound> {
    let protected = !matches!(selection, Selection::Routed(Route::Direct));
    if expected.is_some_and(|expected| expected != protected) {
        return None;
    }
    let route = match selection {
        Selection::ExplicitBypass => Route::Direct,
        Selection::Routed(route) => route,
    };
    let (client, endpoint) = match &route {
        Route::Direct => (
            direct.clone(),
            if fixture() { local } else { endpoint }.into(),
        ),
        Route::Proxy { url, .. } => (
            proxy_transport::vendor_metadata_client(proxy_url(url)?, http_client(timeout))?,
            endpoint.into(),
        ),
        Route::Relay { url, .. } => (direct.clone(), relay_url(url)?.into()),
        Route::MissingStrict => return None,
    };
    Some(Bound {
        client,
        endpoint,
        timeout,
        route,
        protected,
    })
}

pub(super) async fn bind_relay(
    storage: &Storage,
    account: &Value,
    endpoint: &str,
    client: &Client,
    timeout: Duration,
    identity: [u8; 32],
) -> Option<Bound> {
    let binding = bind(
        storage,
        account,
        endpoint,
        endpoint,
        client,
        timeout,
        Some(true),
    )
    .await?;
    (binding.route.relay_identity() == Some(identity)).then_some(binding)
}

#[derive(PartialEq, Eq)]
pub(crate) enum Delivery {
    Direct,
    Bypass,
    Tunnel(Url),
}

impl Delivery {
    pub(crate) async fn select(
        storage: &Storage,
        settings: &Value,
        account: &Value,
        endpoint: &str,
    ) -> Result<Self, ()> {
        if truthy(&account["providerSpecificData"]["vercelRelayUrl"]) {
            return Err(());
        }
        match proxy_transport::select_with_bypass(storage, settings, account, endpoint).await? {
            Selection::ExplicitBypass => Ok(Self::Bypass),
            Selection::Routed(Route::Direct) if !requires_protection(settings, account) => {
                Ok(Self::Direct)
            }
            Selection::Routed(Route::Proxy { url, .. }) => proxy_transport::tunnel_proxy_url(&url)
                .map(Self::Tunnel)
                .ok_or(()),
            _ => Err(()),
        }
    }

    pub(crate) fn protected(&self) -> bool {
        !matches!(self, Self::Direct)
    }

    pub(crate) fn use_fixture(&self) -> bool {
        !matches!(self, Self::Tunnel(_)) && fixture()
    }

    pub(crate) fn client(&self, direct: &Client, timeout: Duration) -> Option<Client> {
        match self {
            Self::Tunnel(proxy) => {
                proxy_transport::vendor_metadata_client(proxy.clone(), http_client(timeout))
            }
            _ => Some(direct.clone()),
        }
    }
}

pub(super) fn requires_protection(settings: &Value, account: &Value) -> bool {
    proxy_transport::account_requires_tunnel(account)
        || truthy(&settings["outboundProxyEnabled"])
        || !fixture()
            && ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
                .iter()
                .any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()))
}

pub(crate) fn fixture() -> bool {
    std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
}

pub(crate) fn http_client(timeout: Duration) -> ClientBuilder {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(timeout)
}
