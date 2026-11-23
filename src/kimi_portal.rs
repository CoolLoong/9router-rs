use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::{
    Client, Method, Request, StatusCode,
    header::{self, HeaderMap, HeaderValue},
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::store::expiry;
use crate::provider_http::{self, http_client};
use crate::provider_index::js_trim;
use crate::proxy_transport::Route;
use crate::storage::Storage;

const PORTAL: &str = "https://auth.kimi.com/api/oauth";
const FIXTURE: &str = "http://127.0.0.1:52989/kimi-device";
const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const RESPONSE_LIMIT: usize = 128 * 1024;
pub(crate) const VERIFY: &str = "https://www.kimi.com/code/authorize_device";

#[derive(Clone)]
pub(crate) struct Portal {
    client: Client,
    client_id: String,
}

pub(crate) enum Endpoint<'a> {
    Register,
    Poll(&'a str),
    Refresh {
        token: &'a str,
        client_id: Option<&'a str>,
    },
}

pub(crate) struct Prepared {
    client: Client,
    request: Request,
}

impl Portal {
    pub(crate) fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: http_client(Duration::from_secs(15)).build()?,
            client_id: ["KIMI_CODING_OAUTH_CLIENT_ID", "KIMI_OAUTH_CLIENT_ID"]
                .iter()
                .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
                .unwrap_or_else(|| CLIENT_ID.into()),
        })
    }

    pub(crate) fn saved_client_id(&self) -> Option<&str> {
        (self.client_id != CLIENT_ID).then_some(self.client_id.as_str())
    }

    pub(crate) async fn prepare(
        &self,
        storage: &Storage,
        account: &Value,
        device_id: &str,
        step: Endpoint<'_>,
    ) -> Result<Prepared, ()> {
        let path = match step {
            Endpoint::Register => "device_authorization",
            _ => "token",
        };
        let destination = provider_http::bind(
            storage,
            account,
            &format!("{PORTAL}/{path}"),
            &format!("{FIXTURE}/{path}"),
            &self.client,
            Duration::from_secs(15),
            None,
        )
        .await
        .ok_or(())?;
        if matches!(&destination.route, Route::Relay { .. })
            && !matches!(step, Endpoint::Refresh { .. })
        {
            return Err(());
        }
        let request = destination
            .request(Method::POST)
            .header(header::ACCEPT, "application/json")
            .headers(device_headers(device_id).ok_or(())?);
        let client_id = self.client_id.as_str();
        let (client, request) = match step {
            Endpoint::Register => request.form(&[("client_id", client_id)]),
            Endpoint::Poll(code) => request.form(&[
                ("grant_type", DEVICE_GRANT),
                ("client_id", client_id),
                ("device_code", code),
            ]),
            Endpoint::Refresh {
                token,
                client_id: saved,
            } => request.form(&[
                ("grant_type", "refresh_token"),
                ("client_id", saved.unwrap_or(CLIENT_ID)),
                ("refresh_token", token),
            ]),
        }
        .build_split();
        let request = request.map_err(|_| ())?;
        Ok(Prepared { client, request })
    }
}

impl Prepared {
    pub(crate) async fn send(self) -> Result<(StatusCode, Value), ()> {
        let mut response = self.client.execute(self.request).await.map_err(|_| ())?;
        let status = response.status();
        if status.is_redirection() {
            return Err(());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if chunk.len() > RESPONSE_LIMIT - bytes.len() {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        let data =
            serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({"error":"invalid_response"}));
        Ok((status, data))
    }
}

pub(crate) fn token_fields(data: &Value, now: DateTime<Utc>) -> Result<Map<String, Value>, ()> {
    let access = text(&data["access_token"]).ok_or(())?;
    let expires = expiry(now, data.get("expires_in")).map_err(|_| ())?;
    let mut fields = Map::from_iter([
        ("accessToken".into(), json!(access)),
        ("expiresAt".into(), expires),
        ("expiresIn".into(), data["expires_in"].clone()),
    ]);
    if let Some(refresh) = text(&data["refresh_token"]) {
        fields.insert("refreshToken".into(), json!(refresh));
    }
    Ok(fields)
}

pub(crate) fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

pub(crate) fn saved_device_id(account: &Value) -> Option<&str> {
    let device = text(&account["providerSpecificData"]["deviceId"]).map(js_trim)?;
    Uuid::parse_str(device)
        .is_ok_and(|id| id.get_version_num() == 4)
        .then_some(device)
}

pub(crate) fn device_headers(device_id: &str) -> Option<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_str(&crate::provider_defaults::USER_AGENT).ok()?,
    );
    headers.insert("x-msh-platform", HeaderValue::from_static("9router"));
    headers.insert(
        "x-msh-version",
        HeaderValue::from_str(env!("CARGO_PKG_VERSION")).ok()?,
    );
    headers.insert("x-msh-device-name", HeaderValue::from_static("9Router"));
    headers.insert(
        "x-msh-device-model",
        HeaderValue::from_str(&device_model()).ok()?,
    );
    headers.insert("x-msh-device-id", HeaderValue::from_str(device_id).ok()?);
    Some(headers)
}

fn device_model() -> String {
    let name = match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        os => os,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        arch => arch,
    };
    format!("{name} {arch}")
}
