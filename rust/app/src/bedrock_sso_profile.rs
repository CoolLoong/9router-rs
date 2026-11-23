use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeDelta, Utc};
use reqwest::Method;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use url::Url;

use super::{Endpoint, Fields, Owned, Policy, Profiles, cache, response_bytes};
use crate::bedrock_transport::valid_region;

const BAD_CONFIGURATION: &str = "Invalid AWS IAM Identity Center profile settings";
const UNAVAILABLE: &str = "AWS IAM Identity Center credentials are not available";
const NO_SESSION: &str = "The AWS IAM Identity Center session is missing or invalid";
const EXPIRED: &str = "The AWS IAM Identity Center session has expired";
const RENEWAL_FAILED: &str = "Could not renew the AWS IAM Identity Center session";

pub(super) struct Session {
    cache_file: PathBuf,
    region: String,
    account: String,
    role: String,
    start: String,
    named: bool,
}

#[derive(PartialEq, Eq, Clone)]
pub(super) struct Endpoints {
    portal: Endpoint,
    oidc: Option<Result<Endpoint, &'static str>>,
}

impl Session {
    pub fn select(fields: &Fields, sessions: &Profiles, home: &Path) -> Result<Self, &'static str> {
        let named = fields.get("sso_session").filter(|name| !name.is_empty());
        let settings = match named {
            Some(name) => sessions.get(name).ok_or(BAD_CONFIGURATION)?,
            None => fields,
        };
        let start = settings
            .get("sso_start_url")
            .filter(|value| {
                Url::parse(value).is_ok_and(|url| {
                    url.scheme() == "https"
                        && url.has_host()
                        && url.username().is_empty()
                        && url.password().is_none()
                })
            })
            .ok_or(BAD_CONFIGURATION)?;
        let region = settings
            .get("sso_region")
            .filter(|value| valid_region(value))
            .ok_or(BAD_CONFIGURATION)?;
        let account = fields
            .get("sso_account_id")
            .filter(|value| value.len() == 12 && value.bytes().all(|value| value.is_ascii_digit()))
            .ok_or(BAD_CONFIGURATION)?;
        let role = fields
            .get("sso_role_name")
            .filter(|value| !value.is_empty())
            .ok_or(BAD_CONFIGURATION)?;
        let key = named.unwrap_or(start);
        let file = format!("{:x}.json", Sha1::digest(key.as_bytes()));
        Ok(Self {
            cache_file: home.join(".aws").join("sso").join("cache").join(file),
            region: region.clone(),
            account: account.clone(),
            role: role.clone(),
            start: start.clone(),
            named: named.is_some(),
        })
    }

    pub(super) async fn routes(&self, policy: &Policy<'_>) -> Result<Endpoints, &'static str> {
        let region = &self.region;
        let mut url = Url::parse(&format!(
            "https://portal.sso.{region}.amazonaws.com/federation/credentials"
        ))
        .map_err(|_| UNAVAILABLE)?;
        url.query_pairs_mut()
            .append_pair("account_id", &self.account)
            .append_pair("role_name", &self.role);
        let portal = policy
            .bind(
                url,
                format!(
                    "http://127.0.0.1:52989/bedrock-profile/sso/{region}/federation/credentials"
                ),
            )
            .await?;
        let oidc = if self.named {
            let url = Url::parse(&format!("https://oidc.{region}.amazonaws.com/token"))
                .map_err(|_| BAD_CONFIGURATION)?;
            Some(
                policy
                    .bind(
                        url,
                        format!("http://127.0.0.1:52989/bedrock-profile/oidc/{region}/token"),
                    )
                    .await,
            )
        } else {
            None
        };
        Ok(Endpoints { portal, oidc })
    }

    pub(super) async fn credentials(
        self,
        policy: &Policy<'_>,
        routes: &Endpoints,
    ) -> Result<Owned, &'static str> {
        let access = self.access(policy, routes).await?;
        let mut authorization =
            reqwest::header::HeaderValue::from_str(&access).map_err(|_| NO_SESSION)?;
        authorization.set_sensitive(true);
        let response = routes
            .portal
            .request(policy, Method::GET)
            .await?
            .header("x-amz-sso_bearer_token", authorization)
            .send()
            .await
            .map_err(|_| UNAVAILABLE)?;
        let data: Value =
            serde_json::from_slice(&response_bytes(response).await.ok_or(UNAVAILABLE)?)
                .map_err(|_| UNAVAILABLE)?;
        let credentials = &data["roleCredentials"];
        let expiry = credentials["expiration"]
            .as_i64()
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .filter(|expires| *expires > Utc::now())
            .ok_or(UNAVAILABLE)?;
        let result = Owned {
            access: nonempty(&credentials["accessKeyId"])
                .ok_or(UNAVAILABLE)?
                .to_owned(),
            secret: nonempty(&credentials["secretAccessKey"])
                .ok_or(UNAVAILABLE)?
                .to_owned(),
            session: Some(
                nonempty(&credentials["sessionToken"])
                    .ok_or(UNAVAILABLE)?
                    .to_owned(),
            ),
            expires: Some(expiry),
        };
        result.credentials().validate().map_err(|_| UNAVAILABLE)?;
        Ok(result)
    }

    fn cached_access<'a>(
        &self,
        token: &'a Value,
    ) -> Result<(&'a str, DateTime<Utc>), &'static str> {
        if token
            .get("startUrl")
            .is_some_and(|url| url.as_str() != Some(self.start.as_str()))
            || self.named
                && token
                    .get("region")
                    .is_some_and(|region| region.as_str() != Some(self.region.as_str()))
        {
            return Err(NO_SESSION);
        }
        let access = nonempty(&token["accessToken"]).ok_or(NO_SESSION)?;
        let expiry = timestamp(&token["expiresAt"]).ok_or(NO_SESSION)?;
        Ok((access, expiry))
    }

    async fn access(
        &self,
        policy: &Policy<'_>,
        routes: &Endpoints,
    ) -> Result<String, &'static str> {
        let (_, token) = cache::read(&self.cache_file, NO_SESSION).await?;
        let (access, expiry) = self.cached_access(&token)?;
        if expiry > Utc::now() {
            return Ok(access.to_owned());
        }
        if !self.named {
            return Err(EXPIRED);
        }

        let file = self.cache_file.clone();
        let lock = cache::lock(&file, RENEWAL_FAILED).await?;
        let (before, token) = cache::read(&file, NO_SESSION).await?;
        let (access, expiry) = self.cached_access(&token)?;
        if expiry > Utc::now() {
            return Ok(access.to_owned());
        }
        let oidc = routes
            .oidc
            .as_ref()
            .ok_or(EXPIRED)?
            .as_ref()
            .map_err(|error| *error)?;
        let staged = cache::stage(&file, RENEWAL_FAILED).await?;
        let renewed = self.renew(token, policy, oidc).await?;
        let token = tokio::task::spawn_blocking(move || {
            let _lock = lock;
            cache::persist(staged, &file, &before, renewed, RENEWAL_FAILED)
        })
        .await
        .map_err(|_| RENEWAL_FAILED)??;
        let (access, expiry) = self.cached_access(&token)?;
        (expiry > Utc::now())
            .then(|| access.to_owned())
            .ok_or(EXPIRED)
    }

    async fn renew(
        &self,
        mut token: Value,
        policy: &Policy<'_>,
        oidc: &Endpoint,
    ) -> Result<Value, &'static str> {
        if token["startUrl"] != self.start
            || token["region"] != self.region
            || !timestamp(&token["registrationExpiresAt"]).is_some_and(|date| date > Utc::now())
        {
            return Err(EXPIRED);
        }
        let client_id = opaque(&token["clientId"]).ok_or(EXPIRED)?;
        let client_secret = opaque(&token["clientSecret"]).ok_or(EXPIRED)?;
        let refresh = opaque(&token["refreshToken"]).ok_or(EXPIRED)?;
        let request = oidc.request(policy, Method::POST).await?.json(&json!({
            "clientId": client_id,
            "clientSecret": client_secret,
            "grantType": "refresh_token",
            "refreshToken": refresh,
        }));
        cache::mark_attempt(&self.cache_file, refresh, RENEWAL_FAILED)?;
        let response = request.send().await.map_err(|_| RENEWAL_FAILED)?;
        let data: Value =
            serde_json::from_slice(&response_bytes(response).await.ok_or(RENEWAL_FAILED)?)
                .map_err(|_| RENEWAL_FAILED)?;
        let access = opaque(&data["accessToken"]).ok_or(RENEWAL_FAILED)?;
        let expiry = data["expiresIn"]
            .as_i64()
            .filter(|seconds| *seconds > 0)
            .and_then(TimeDelta::try_seconds)
            .and_then(|duration| Utc::now().checked_add_signed(duration))
            .ok_or(RENEWAL_FAILED)?;
        if let Some(kind) = data.get("tokenType")
            && !nonempty(kind).is_some_and(|kind| kind.eq_ignore_ascii_case("Bearer"))
        {
            return Err(RENEWAL_FAILED);
        }
        if let Some(rotated) = data.get("refreshToken") {
            token["refreshToken"] = json!(opaque(rotated).ok_or(RENEWAL_FAILED)?);
        }
        token["accessToken"] = json!(access);
        token["expiresAt"] = json!(expiry.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        Ok(token)
    }
}

fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    let raw = nonempty(value)?;
    DateTime::parse_from_rfc3339(raw)
        .or_else(|error| {
            if let Some(date) = raw.strip_suffix("UTC") {
                DateTime::parse_from_rfc3339(&format!("{date}Z"))
            } else {
                Err(error)
            }
        })
        .ok()
        .map(|date| date.with_timezone(&Utc))
}

fn opaque(value: &Value) -> Option<&str> {
    nonempty(value).filter(|text| text.bytes().all(|byte| byte.is_ascii_graphic()))
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}
