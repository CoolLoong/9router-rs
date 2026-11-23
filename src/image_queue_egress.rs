use std::sync::LazyLock;
use std::time::Duration;

use reqwest::{Client, Method, RequestBuilder, header};
use serde_json::Value;

use super::{ImageError, QueuedImageError, proxy_required};
use crate::proxy_transport::{self, Route, Selection};
use crate::storage::Storage;
use QueuedImageError::{BeforeSubmit, Protected};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
static HTTP: LazyLock<Result<Client, reqwest::Error>> = LazyLock::new(|| client_builder().build());

fn client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(15))
        .timeout(REQUEST_TIMEOUT)
}

pub(super) struct Endpoint {
    pub name: &'static str,
    pub origin: &'static str,
    pub direct: &'static str,
}

enum Destination {
    Queue(&'static str),
    Relay {
        url: String,
        token: header::HeaderValue,
    },
}

pub(super) struct Delivery {
    client: Client,
    destination: Destination,
    origin: &'static str,
    pub protected: bool,
}

impl Delivery {
    pub async fn select(
        storage: &Storage,
        settings: &Value,
        account: &Value,
        endpoint: Endpoint,
        path: &str,
    ) -> Result<Self, QueuedImageError> {
        let Endpoint {
            name,
            origin,
            direct,
        } = endpoint;
        let real_url = format!("{origin}{path}");
        let unavailable = || {
            Protected(ImageError::bad_gateway(format!(
                "{name} connection proxy is unavailable"
            )))
        };
        let route = crate::provider_http::select(storage, settings, account, &real_url)
            .await
            .map_err(|_| unavailable())?;
        let direct_route = |protected| {
            Ok(Self {
                client: HTTP
                    .as_ref()
                    .map_err(|_| {
                        let failure = ImageError::bad_gateway(format!(
                            "Could not initialize the {name} HTTP client"
                        ));
                        if protected {
                            Protected(failure)
                        } else {
                            BeforeSubmit(failure)
                        }
                    })?
                    .clone(),
                destination: Destination::Queue(direct),
                origin,
                protected,
            })
        };
        match route {
            Selection::Routed(Route::Direct) if !proxy_required(settings, account) => {
                direct_route(false)
            }
            Selection::ExplicitBypass => direct_route(true),
            Selection::Routed(Route::Proxy { url, .. }) => {
                let proxy = proxy_transport::tunnel_proxy_url(&url).ok_or_else(unavailable)?;
                let client = proxy_transport::vendor_metadata_client(proxy, client_builder())
                    .ok_or_else(unavailable)?;
                Ok(Self {
                    client,
                    destination: Destination::Queue(origin),
                    origin,
                    protected: true,
                })
            }
            Selection::Routed(Route::Relay {
                url,
                authorization: Some(token),
                ..
            }) => {
                let client = HTTP.as_ref().map_err(|_| unavailable())?.clone();
                Ok(Self {
                    client,
                    destination: Destination::Relay { url, token },
                    origin,
                    protected: true,
                })
            }
            Selection::Routed(Route::Relay { .. }) => Err(Protected(ImageError::bad_gateway(
                format!("{name} queue requires a validated Relay pool",),
            ))),
            _ => Err(unavailable()),
        }
    }

    pub fn request(
        &self,
        method: Method,
        path: &str,
        authorization: &header::HeaderValue,
    ) -> RequestBuilder {
        match &self.destination {
            Destination::Queue(base) => self
                .client
                .request(method, format!("{base}{path}"))
                .header(header::AUTHORIZATION, authorization.clone()),
            Destination::Relay { url, token } => {
                let route = Route::Relay {
                    url: url.clone(),
                    target: self.origin.into(),
                    path: path.to_owned(),
                    authorization: Some(token.clone()),
                };
                route.relay(
                    self.client
                        .request(method, url)
                        .timeout(REQUEST_TIMEOUT)
                        .header(header::AUTHORIZATION, authorization.clone()),
                )
            }
        }
    }

    fn relay_token(&self) -> Option<&str> {
        match &self.destination {
            Destination::Relay { token, .. } => token.to_str().ok(),
            Destination::Queue(_) => None,
        }
    }

    pub fn contains_secret(&self, text: &str, key: &str) -> bool {
        text.contains(key) || self.relay_token().is_some_and(|token| text.contains(token))
    }

    pub fn redact(&self, message: &str, key: &str) -> String {
        let message = message.replace(key, "[redacted]");
        match self.relay_token() {
            Some(token) => message.replace(token, "[redacted]"),
            None => message,
        }
    }
}
