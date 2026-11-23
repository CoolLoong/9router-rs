use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::{Method, RequestBuilder, Response, StatusCode, header};
use serde_json::Value;
use url::Url;

use super::{Endpoint, Owned, Policy, Profiles, RESPONSE_LIMIT, http, read_credential_file};

const ERROR: &str = "AWS credential_source metadata request failed";
const TOKEN_ERROR: &str = "Invalid AWS credential_source metadata authorization token";
const EC2_TOKEN: &str = "x-aws-ec2-metadata-token";
const EC2_ROLE: &str = "/latest/meta-data/iam/security-credentials/";
const ECS_HOSTS: &[&str] = &["169.254.170.2", "169.254.170.23", "[fd00:ec2::23]"];
const EC2_HOSTS: &[&str] = &["169.254.169.254", "[fd00:ec2::254]"];
const ECS_BASE: &str = "http://169.254.170.2";

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Source {
    Container(Url),
    Instance { endpoint: Url, disable_v1: bool },
}

impl Source {
    pub(super) fn select(
        kind: &str,
        profiles: &Profiles,
        profile: &str,
    ) -> Result<Self, &'static str> {
        match kind {
            "EcsContainer" => {
                let endpoint = if let Some(relative) = env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
                {
                    if !relative.starts_with('/') || relative.starts_with("//") {
                        return Err(ERROR);
                    }
                    format!("{ECS_BASE}{relative}")
                } else {
                    env("AWS_CONTAINER_CREDENTIALS_FULL_URI").ok_or(ERROR)?
                };
                let endpoint = metadata_url(&endpoint, ECS_HOSTS)?;
                Ok(Self::Container(endpoint))
            }
            "Ec2InstanceMetadata" => {
                if env("AWS_EC2_METADATA_DISABLED")
                    .is_some_and(|value| value.eq_ignore_ascii_case("true"))
                {
                    return Err("AWS credential_source EC2 metadata is disabled");
                }
                let default = env("AWS_PROFILE").unwrap_or_else(|| "default".into());
                let fields = profiles.get(&default);
                let setting = |variable, key| {
                    env(variable).or_else(|| fields.and_then(|fields| fields.get(key)).cloned())
                };
                let configured = setting(
                    "AWS_EC2_METADATA_SERVICE_ENDPOINT",
                    "ec2_metadata_service_endpoint",
                );
                let endpoint = match configured {
                    Some(value) => metadata_url(&value, EC2_HOSTS)?,
                    None => {
                        let value = setting(
                            "AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE",
                            "ec2_metadata_service_endpoint_mode",
                        );
                        let default = match value.as_deref() {
                            None | Some("IPv4") => "http://169.254.169.254",
                            Some("IPv6") => "http://[fd00:ec2::254]",
                            Some(_) => {
                                return Err("Invalid AWS credential_source metadata endpoint mode");
                            }
                        };
                        Url::parse(default).map_err(|_| ERROR)?
                    }
                };
                let disable_v1 = std::env::var("AWS_EC2_METADATA_V1_DISABLED")
                    .ok()
                    .or_else(|| {
                        profiles
                            .get(profile)?
                            .get("ec2_metadata_v1_disabled")
                            .cloned()
                    })
                    .is_some_and(|value| !value.is_empty() && value != "false");
                Ok(Self::Instance {
                    endpoint,
                    disable_v1,
                })
            }
            _ => Err("AWS credential_source is not supported"),
        }
    }

    pub(super) async fn route(
        &self,
        policy: &Policy<'_>,
    ) -> Result<Option<Endpoint>, &'static str> {
        let (endpoint, hosts) = match self {
            Self::Container(endpoint) => (endpoint, ECS_HOSTS),
            Self::Instance { endpoint, .. } => (endpoint, EC2_HOSTS),
        };
        if local_host(endpoint.host_str().ok_or(ERROR)?, hosts) {
            return Ok(None);
        }
        policy
            .bind(endpoint.clone(), endpoint.to_string())
            .await
            .map(Some)
    }

    pub(super) async fn credentials(
        self,
        policy: &Policy<'_>,
        route: Option<&Endpoint>,
    ) -> Result<Owned, &'static str> {
        let response = match self {
            Self::Container(endpoint) => {
                let mut request = request(policy, route, Method::GET, endpoint).await?;
                if let Some(token) = container_token().await? {
                    request = request.header(header::AUTHORIZATION, token);
                }
                send(request).await?
            }
            Self::Instance {
                endpoint,
                disable_v1,
            } => instance(policy, route, endpoint, disable_v1).await?,
        };
        let credentials: Value =
            serde_json::from_slice(&read(response).await?).map_err(|_| ERROR)?;
        let value = |key: &str| {
            credentials[key]
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or(ERROR)
        };
        let expires: DateTime<Utc> = value("Expiration")?.parse().map_err(|_| ERROR)?;
        if expires <= Utc::now() {
            return Err("AWS credential_source metadata credentials have expired");
        }
        let result = Owned {
            access: value("AccessKeyId")?.to_owned(),
            secret: value("SecretAccessKey")?.to_owned(),
            session: Some(value("Token")?.to_owned()),
            expires: Some(expires),
        };
        result.credentials().validate().map_err(|_| ERROR)?;
        Ok(result)
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn metadata_url(value: &str, local_hosts: &[&str]) -> Result<Url, &'static str> {
    let target = Url::parse(value).map_err(|_| ERROR)?;
    let host = target.host_str().ok_or(ERROR)?;
    let permitted =
        target.scheme() == "https" || target.scheme() == "http" && local_host(host, local_hosts);
    if !permitted
        || !target.username().is_empty()
        || target.password().is_some()
        || target.fragment().is_some()
    {
        return Err(ERROR);
    }
    Ok(target)
}

fn local_host(host: &str, local_hosts: &[&str]) -> bool {
    matches!(host, "localhost" | "[::1]")
        || host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
        || local_hosts.contains(&host)
}

async fn container_token() -> Result<Option<header::HeaderValue>, &'static str> {
    let token = if let Some(path) =
        std::env::var_os("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE").filter(|path| !path.is_empty())
    {
        let path = PathBuf::from(path);
        Some(
            tokio::task::spawn_blocking(move || read_credential_file(&path, TOKEN_ERROR))
                .await
                .map_err(|_| TOKEN_ERROR)??,
        )
    } else {
        env("AWS_CONTAINER_AUTHORIZATION_TOKEN")
    };
    token
        .map(|value| token_header(value.as_bytes()))
        .transpose()
}

fn token_header(bytes: &[u8]) -> Result<header::HeaderValue, &'static str> {
    if bytes.is_empty() {
        return Err(TOKEN_ERROR);
    }
    let mut value = header::HeaderValue::from_bytes(bytes).map_err(|_| TOKEN_ERROR)?;
    value.set_sensitive(true);
    Ok(value)
}

async fn instance(
    policy: &Policy<'_>,
    route: Option<&Endpoint>,
    endpoint: Url,
    disable_v1: bool,
) -> Result<Response, &'static str> {
    let token_url = endpoint.join("/latest/api/token").map_err(|_| ERROR)?;
    let token = request(policy, route, Method::PUT, token_url)
        .await?
        .header("x-aws-ec2-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_secs(1))
        .send()
        .await;
    let token = match token {
        Ok(response) if response.status() == StatusCode::OK => {
            Some(token_header(&read(response).await?)?)
        }
        Ok(response)
            if matches!(
                response.status(),
                StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ) =>
        {
            None
        }
        Err(error) if error.is_timeout() => None,
        _ => return Err(ERROR),
    };
    if token.is_none() && disable_v1 {
        return Err("AWS credential_source IMDSv1 fallback is disabled");
    }
    let mut role_url = endpoint.join(EC2_ROLE).map_err(|_| ERROR)?;
    let get = async |url| {
        let request = request(policy, route, Method::GET, url).await?;
        let request = match &token {
            Some(token) => request.header(EC2_TOKEN, token.clone()),
            None => request,
        };
        send(request).await
    };
    let body = read(get(role_url.clone()).await?).await?;
    let role = std::str::from_utf8(&body).map_err(|_| ERROR)?.trim();
    if role.is_empty()
        || matches!(role, "." | "..")
        || role.len() > 64
        || !role
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+=,.@_-".contains(&byte))
    {
        return Err(ERROR);
    }
    role_url
        .path_segments_mut()
        .map_err(|_| ERROR)?
        .pop_if_empty()
        .push(role);
    get(role_url).await
}

async fn request(
    policy: &Policy<'_>,
    route: Option<&Endpoint>,
    method: Method,
    target: Url,
) -> Result<RequestBuilder, &'static str> {
    let Some(route) = route else {
        return Ok(http().request(method, target));
    };
    if route.target.origin() != target.origin() {
        return Err(ERROR);
    }
    Endpoint {
        direct_fixture: target.to_string(),
        target,
        delivery: route.delivery.clone(),
    }
    .request(policy, method)
    .await
}

async fn send(request: RequestBuilder) -> Result<Response, &'static str> {
    request
        .timeout(Duration::from_secs(1))
        .send()
        .await
        .map_err(|_| ERROR)
}

async fn read(mut response: Response) -> Result<Vec<u8>, &'static str> {
    if response.status() != StatusCode::OK {
        return Err(ERROR);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ERROR)? {
        if chunk.len() > RESPONSE_LIMIT - body.len() {
            return Err(ERROR);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
