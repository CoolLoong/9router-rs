use std::path::{Path, PathBuf};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use p256::SecretKey;
use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use p256::pkcs8::DecodePrivateKey;
use reqwest::{Method, StatusCode, header::HeaderValue};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use super::{Owned, Policy, cache, response_bytes};
use crate::bedrock_transport::valid_region;

const NO_SESSION: &str = "The AWS CLI login session is missing or invalid";
const RENEWAL_FAILED: &str = "Could not renew the AWS CLI login session";
const DENIED: &str = "AWS denied the CLI login session renewal; run aws login again";
const LEAD: TimeDelta = TimeDelta::minutes(5);

pub(super) struct Session {
    file: PathBuf,
    region: Option<String>,
}

impl Session {
    pub(super) fn new(name: &str, region: Option<String>, home: &Path) -> Self {
        let directory = std::env::var_os("AWS_LOGIN_CACHE_DIRECTORY")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".aws/login/cache"));
        let file = directory.join(format!("{:x}.json", Sha256::digest(name.as_bytes())));
        Self { file, region }
    }

    pub(super) fn cache_file(&self) -> &Path {
        &self.file
    }

    pub(super) async fn credentials(self, policy: &Policy<'_>) -> Result<Owned, &'static str> {
        let (_, token) = cache::read(&self.file, NO_SESSION).await?;
        let initial = cached(&token)?;
        if ready(&initial, LEAD) {
            return Ok(initial);
        }
        let lock = match cache::lock(&self.file, RENEWAL_FAILED).await {
            Ok(lock) => lock,
            Err(error) => return available(initial, error),
        };
        let (before, token) = match cache::read(&self.file, NO_SESSION).await {
            Ok(entry) => entry,
            Err(error) => return available(initial, error),
        };
        let current = cached(&token)?;
        if ready(&current, LEAD) {
            return Ok(current);
        }
        let staged = match cache::stage(&self.file, RENEWAL_FAILED).await {
            Ok(staged) => staged,
            Err(error) => return available(current, error),
        };
        let updated = match self.renew(token, policy).await {
            Ok(updated) => updated,
            Err(DENIED) => return Err(DENIED),
            Err(error) => return available(current, error),
        };
        let persisted = tokio::task::spawn_blocking(move || {
            let _lock = lock;
            cache::persist(staged, &self.file, &before, updated, RENEWAL_FAILED)
        })
        .await
        .map_err(|_| RENEWAL_FAILED)
        .and_then(|value| value);
        match persisted {
            Ok(token) => available(cached(&token)?, RENEWAL_FAILED),
            Err(error) => available(current, error),
        }
    }

    async fn renew(&self, mut token: Value, policy: &Policy<'_>) -> Result<Value, &'static str> {
        let env_region = std::env::var("AWS_REGION").ok();
        let region = nonempty(&token["signInRegion"])
            .or(self.region.as_deref())
            .or(env_region.as_deref())
            .filter(|region| valid_region(region))
            .ok_or(RENEWAL_FAILED)?;
        let endpoint = signin_endpoint(region)?;
        let proof = proof(required(&token["dpopKey"])?, &endpoint)?;
        let destination = policy
            .bind(
                endpoint,
                format!("http://127.0.0.1:52989/bedrock-profile/signin/{region}/v1/token"),
            )
            .await?;
        let refresh = required(&token["refreshToken"])?;
        let request = destination
            .request(policy, Method::POST)
            .await?
            .header("dpop", proof)
            .json(&json!({
                "clientId": required(&token["clientId"])?,
                "grantType": "refresh_token",
                "refreshToken": refresh,
            }));
        cache::mark_attempt(&self.file, refresh, RENEWAL_FAILED)?;
        let response = request.send().await.map_err(|_| RENEWAL_FAILED)?;
        if response.status() == StatusCode::FORBIDDEN {
            return Err(DENIED);
        }
        let response: Value =
            serde_json::from_slice(&response_bytes(response).await.ok_or(RENEWAL_FAILED)?)
                .map_err(|_| RENEWAL_FAILED)?;
        let seconds = match response.get("expiresIn") {
            None | Some(Value::Null) => 900,
            Some(value) => value
                .as_i64()
                .filter(|seconds| *seconds > 0)
                .ok_or(RENEWAL_FAILED)?,
        };
        let expires = TimeDelta::try_seconds(seconds)
            .and_then(|delta| Utc::now().checked_add_signed(delta))
            .ok_or(RENEWAL_FAILED)?;
        let access = &response["accessToken"];
        for key in ["accessKeyId", "secretAccessKey", "sessionToken"] {
            token["accessToken"][key] = json!(nonempty(&access[key]).ok_or(RENEWAL_FAILED)?);
        }
        token["accessToken"]["expiresAt"] =
            json!(expires.to_rfc3339_opts(SecondsFormat::Millis, true));
        token["refreshToken"] = json!(nonempty(&response["refreshToken"]).ok_or(RENEWAL_FAILED)?);
        cached(&token).map_err(|_| RENEWAL_FAILED)?;
        Ok(token)
    }
}

fn signin_endpoint(region: &str) -> Result<Url, &'static str> {
    let suffix = if region.starts_with("cn-") {
        "amazonaws.cn"
    } else if region.starts_with("us-gov-") {
        "amazonaws-us-gov.com"
    } else if region.starts_with("us-iso-") {
        "c2shome.ic.gov"
    } else if region.starts_with("us-isob-") {
        "sc2shome.sgov.gov"
    } else if region.starts_with("eu-isoe-") {
        "csphome.adc-e.uk"
    } else if region.starts_with("us-isof-") {
        "csphome.hci.ic.gov"
    } else if region.starts_with("eusc-") {
        "amazonaws-eusc.eu"
    } else {
        "aws.amazon.com"
    };
    Url::parse(&format!("https://{region}.signin.{suffix}/v1/token")).map_err(|_| RENEWAL_FAILED)
}

fn proof(pem: &str, endpoint: &Url) -> Result<HeaderValue, &'static str> {
    let secret = SecretKey::from_sec1_pem(pem)
        .or_else(|_| SecretKey::from_pkcs8_pem(pem))
        .map_err(|_| RENEWAL_FAILED)?;
    let key = SigningKey::from(secret);
    let coordinates = key.verifying_key().to_encoded_point(false);
    let header = json!({"alg":"ES256","typ":"dpop+jwt","jwk":{
        "kty":"EC","crv":"P-256",
        "x":URL_SAFE_NO_PAD.encode(coordinates.x().ok_or(RENEWAL_FAILED)?),
        "y":URL_SAFE_NO_PAD.encode(coordinates.y().ok_or(RENEWAL_FAILED)?),
    }});
    let payload = json!({
        "jti":Uuid::new_v4().to_string(),"htm":"POST","htu":endpoint.as_str(),"iat":Utc::now().timestamp(),
    });
    let message = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(payload.to_string())
    );
    let signature: Signature = key.sign(message.as_bytes());
    let mut proof = HeaderValue::from_str(&format!(
        "{message}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
    .map_err(|_| RENEWAL_FAILED)?;
    proof.set_sensitive(true);
    Ok(proof)
}

fn cached(token: &Value) -> Result<Owned, &'static str> {
    for key in ["clientId", "refreshToken", "dpopKey"] {
        required(&token[key])?;
    }
    let access = &token["accessToken"];
    required(&access["accountId"])?;
    let expires: DateTime<Utc> = required(&access["expiresAt"])?
        .parse()
        .map_err(|_| NO_SESSION)?;
    let result = Owned {
        access: required(&access["accessKeyId"])?.to_owned(),
        secret: required(&access["secretAccessKey"])?.to_owned(),
        session: Some(required(&access["sessionToken"])?.to_owned()),
        expires: Some(expires),
    };
    result.credentials().validate().map_err(|_| NO_SESSION)?;
    Ok(result)
}

fn ready(credentials: &Owned, lead: TimeDelta) -> bool {
    credentials
        .expires
        .is_some_and(|expiry| expiry - Utc::now() > lead)
}

fn available(credentials: Owned, error: &'static str) -> Result<Owned, &'static str> {
    ready(&credentials, TimeDelta::zero())
        .then_some(credentials)
        .ok_or(error)
}

fn required(value: &Value) -> Result<&str, &'static str> {
    nonempty(value).ok_or(NO_SESSION)
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}
