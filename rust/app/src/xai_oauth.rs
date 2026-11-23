use percent_encoding::utf8_percent_encode;
use reqwest::{Client, RequestBuilder, header};
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use url::Url;

use super::flow::{
    ApiError, Credentials, URI_COMPONENT, form, mapped_code_tokens, nonempty, quoted, receive_json,
    token_claims,
};

const REGISTRY: &str = include_str!("../../../open-sse/providers/registry/xai.js");
const CONSTANTS: &str = include_str!("../../../src/lib/oauth/constants/xai.js");

pub(super) struct Config {
    pub(super) verifier_bytes: usize,
    pub(super) port: u16,
    pub(super) callback_path: String,
    client: String,
    scope: String,
    authorize: String,
    token: String,
    discovery: String,
    endpoints: OnceCell<Endpoints>,
}

struct Endpoints {
    authorize: String,
    token: String,
}

impl Config {
    pub(super) fn pinned() -> Option<Self> {
        let issuer = constant("ISSUER")?;
        let transport = REGISTRY.split_once("\n  transport: {")?.1;
        Some(Self {
            verifier_bytes: constant_value("PKCE_VERIFIER_BYTES")?.parse().ok()?,
            port: constant_value("LOOPBACK_PORT")?.parse().ok()?,
            callback_path: constant("CALLBACK_PATH")?.into(),
            client: quoted(transport, "clientId")?,
            scope: constant("SCOPE")?.into(),
            authorize: format!("{issuer}{}", constant("AUTH_ENDPOINT_PATH")?),
            token: format!("{issuer}{}", constant("TOKEN_ENDPOINT_PATH")?),
            discovery: format!("{issuer}{}", constant("DISCOVERY_PATH")?),
            endpoints: OnceCell::new(),
        })
    }

    pub(super) async fn authorize_url(
        &self,
        client: &Client,
        redirect: &str,
        challenge: &str,
        state: &str,
    ) -> Result<String, ApiError> {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce)
            .map_err(|_| ApiError::failed("Could not generate OAuth nonce"))?;
        let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let endpoints = self.discover(client).await;
        let query = [
            ("response_type", "code"),
            ("client_id", self.client.as_str()),
            ("redirect_uri", redirect),
            ("scope", self.scope.as_str()),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("nonce", &nonce),
            ("plan", "generic"),
            ("referrer", "cli-proxy-api"),
        ]
        .map(|(key, value)| format!("{key}={}", utf8_percent_encode(value, URI_COMPONENT)))
        .join("&");
        Ok(format!("{}?{query}", endpoints.authorize))
    }

    pub(super) async fn exchange(
        &self,
        client: &Client,
        code: &str,
        redirect: &str,
        verifier: &str,
    ) -> Result<Credentials, ApiError> {
        let token = &self.discover(client).await.token;
        let (success, tokens) = receive_json(form(
            client,
            &fixture_url(token),
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &self.client),
                ("code", code),
                ("redirect_uri", redirect),
                ("code_verifier", verifier),
            ],
        ))
        .await
        .map_err(|_| ApiError::failed("xAI token exchange failed"))?;
        if !success || nonempty(&tokens["access_token"]).is_none() {
            return Err(ApiError::failed("xAI token exchange failed"));
        }
        let mut fields = mapped_code_tokens(&tokens);
        if let Some(token) = nonempty(&tokens["id_token"]) {
            let claims = token_claims(token);
            if let Some(email) = ["email", "preferred_username", "sub"]
                .into_iter()
                .find_map(|field| nonempty(&claims[field]))
            {
                fields.insert("email".into(), json!(email));
            }
            fields.insert("providerSpecificData".into(), json!({"idToken": token}));
        }
        Ok(Credentials(fields))
    }

    pub(super) async fn refresh_request(&self, client: &Client, refresh: &str) -> RequestBuilder {
        let token = &self.discover(client).await.token;
        self.refresh_at(client, &fixture_url(token), refresh)
    }

    pub(super) fn token_endpoint(&self) -> &str {
        &self.token
    }

    pub(super) fn refresh_at(
        &self,
        client: &Client,
        endpoint: &str,
        refresh: &str,
    ) -> RequestBuilder {
        form(
            client,
            endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &self.client),
                ("refresh_token", refresh),
            ],
        )
    }

    async fn discover(&self, client: &Client) -> &Endpoints {
        self.endpoints
            .get_or_init(|| async {
                let request = client
                    .get(fixture_url(&self.discovery).as_ref())
                    .header(header::ACCEPT, "application/json");
                if let Ok((true, data)) = receive_json(request).await
                    && let Some(authorize) = trusted_endpoint(&data["authorization_endpoint"])
                    && let Some(token) = trusted_endpoint(&data["token_endpoint"])
                {
                    return Endpoints { authorize, token };
                }
                Endpoints {
                    authorize: self.authorize.clone(),
                    token: self.token.clone(),
                }
            })
            .await
    }
}

fn constant_value(name: &str) -> Option<&'static str> {
    CONSTANTS.lines().find_map(|line| {
        line.strip_prefix("export const XAI_")?
            .strip_prefix(name)?
            .strip_prefix(" = ")?
            .strip_suffix(';')
    })
}

fn constant(name: &str) -> Option<&'static str> {
    constant_value(name)?.strip_prefix('"')?.strip_suffix('"')
}

fn trusted_endpoint(value: &Value) -> Option<String> {
    let value = value.as_str()?.trim();
    let url = Url::parse(value).ok()?;
    (url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url
            .host_str()
            .is_some_and(|host| host == "x.ai" || host.ends_with(".x.ai")))
    .then(|| value.to_owned())
}

fn fixture_url(endpoint: &str) -> std::borrow::Cow<'_, str> {
    if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        && let Ok(url) = Url::parse(endpoint)
    {
        return format!("http://127.0.0.1:52989/xai-oauth{}", url.path()).into();
    }
    endpoint.into()
}
