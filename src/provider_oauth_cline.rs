use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use reqwest::{Client, RequestBuilder, header};
use serde_json::{Map, Value, json};
use url::Url;

use super::flow::{ApiError, Credentials, nonempty, receive_json, text};

pub(super) struct Config {
    pub(super) authorize: String,
    pub(super) token: String,
    pub(super) user: String,
    pub(super) refresh: String,
}

impl Config {
    pub(super) fn pinned(oauth: &Value, token_field: &str) -> Option<Self> {
        Some(Self {
            authorize: text(oauth, "authorizeUrl")?,
            token: text(oauth, token_field)?,
            user: format!("{}/api/v1/users/me", text(oauth, "apiBaseUrl")?),
            refresh: text(oauth, "refreshUrl")?,
        })
    }

    pub(super) fn authorize(&self, redirect: &str, state: &str) -> Result<String, ApiError> {
        let mut url = Url::parse(&self.authorize)
            .map_err(|_| ApiError::failed("Invalid authorization endpoint"))?;
        url.query_pairs_mut().extend_pairs([
            ("client_type", "extension"),
            ("callback_url", redirect),
            ("redirect_uri", redirect),
            ("state", state),
        ]);
        Ok(url.into())
    }

    pub(super) async fn exchange(
        &self,
        client: &Client,
        code: &str,
        redirect: &str,
    ) -> Result<Credentials, ApiError> {
        if let Some(envelope) = direct_envelope(code) {
            return self.exchange_envelope(client, &envelope).await;
        }
        let request = client
            .post(&self.token)
            .header(header::ACCEPT, "application/json")
            .json(&json!({
                "grant_type":"authorization_code","code":code,"client_type":"extension","redirect_uri":redirect,
            }));
        let invalid = || ApiError::failed("Cline token exchange failed");
        let data = provider_json(request).await.ok_or_else(invalid)?;
        let tokens = &data["data"];
        let access = nonempty(&tokens["accessToken"])
            .or_else(|| nonempty(&data["accessToken"]))
            .ok_or_else(invalid)?;
        let refresh = nonempty(&tokens["refreshToken"]).or_else(|| nonempty(&data["refreshToken"]));
        let expires = nonempty(&tokens["expiresAt"]).or_else(|| nonempty(&data["expiresAt"]));
        let email = nonempty(&tokens["userInfo"]["email"]);
        credentials(access, refresh, expires, email, &Value::Null)
    }

    async fn exchange_envelope(
        &self,
        client: &Client,
        envelope: &Value,
    ) -> Result<Credentials, ApiError> {
        let invalid = || ApiError::bad("Could not verify the Cline OAuth credentials");
        let access = nonempty(&envelope["accessToken"]).ok_or_else(invalid)?;
        let profile = self.profile(client, access).await.ok_or_else(invalid)?;
        let Some(refresh) = nonempty(&envelope["refreshToken"]) else {
            return credentials(access, None, None, profile.email(), &profile.data);
        };
        let data = provider_json(refresh_request(client.post(&self.refresh), refresh))
            .await
            .ok_or_else(invalid)?;
        let tokens = data
            .get("data")
            .filter(|data| !data.is_null())
            .unwrap_or(&data);
        let current = nonempty(&tokens["accessToken"]).ok_or_else(invalid)?;
        let current_profile = self.profile(client, current).await.ok_or_else(invalid)?;
        if profile.identity != current_profile.identity {
            return Err(invalid());
        }
        credentials(
            current,
            nonempty(&tokens["refreshToken"]).or(Some(refresh)),
            nonempty(&tokens["expiresAt"]),
            current_profile.email(),
            &current_profile.data,
        )
    }

    async fn profile(&self, client: &Client, token: &str) -> Option<Profile> {
        let request = crate::cline_transport::oauth_headers(
            client
                .get(&self.user)
                .header(header::ACCEPT, "application/json"),
            Some(token),
        )
        .build()
        .ok()?;
        if !request.headers().contains_key(header::AUTHORIZATION) {
            return None;
        }
        let response = client.execute(request).await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let data = response.json::<Value>().await.ok()?;
        if data["success"] == false {
            return None;
        }
        let verified = data
            .get("data")
            .filter(|profile| profile.is_object())
            .unwrap_or(&data);
        let data = ["user", "userInfo"]
            .into_iter()
            .find_map(|key| verified.get(key).filter(|profile| profile.is_object()))
            .unwrap_or(verified);
        let identity = ["id", "userId"]
            .into_iter()
            .find_map(|field| nonempty(&data[field]).filter(|id| !id.trim().is_empty()))
            .map(|id| format!("id:{id}"))
            .or_else(|| {
                nonempty(&data["email"])
                    .filter(|address| !address.trim().is_empty())
                    .map(|email| format!("email:{}", email.to_lowercase()))
            })?;
        Some(Profile {
            identity,
            data: data.clone(),
        })
    }
}

struct Profile {
    identity: String,
    data: Value,
}

impl Profile {
    fn email(&self) -> Option<&str> {
        nonempty(&self.data["email"])
    }
}

fn direct_envelope(code: &str) -> Option<Value> {
    let bytes = [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD]
        .into_iter()
        .find_map(|base64| base64.decode(code).ok())?;
    let value = serde_json::from_slice::<Value>(&bytes).ok()?;
    (value.get("accessToken").is_some() || value.get("refreshToken").is_some()).then_some(value)
}

pub(super) fn refresh_request(request: RequestBuilder, refresh: &str) -> RequestBuilder {
    request
        .header(header::ACCEPT, "application/json")
        .json(&json!({"refreshToken":refresh,"grantType":"refresh_token","clientType":"extension"}))
}

async fn provider_json(request: RequestBuilder) -> Option<Value> {
    let (ok, body) = receive_json(request).await.ok()?;
    (ok && body["success"] != false).then_some(body)
}

fn credentials(
    access: &str,
    refresh: Option<&str>,
    expires: Option<&str>,
    email: Option<&str>,
    profile: &Value,
) -> Result<Credentials, ApiError> {
    let duration = if let Some(expires) = expires {
        DateTime::parse_from_rfc3339(expires)
            .map_err(|_| ApiError::failed("Invalid Cline token expiry"))?
            .signed_duration_since(Utc::now())
            .num_seconds()
    } else {
        3600
    };
    let details: Map<String, Value> = ["firstName", "lastName"]
        .into_iter()
        .filter_map(|key| {
            profile
                .get(key)
                .filter(|value| value.is_string())
                .map(|value| (key.into(), value.clone()))
        })
        .collect();
    let mut fields = Map::from_iter([
        ("accessToken".into(), json!(access)),
        ("expiresIn".into(), json!(duration)),
        ("email".into(), json!(email.unwrap_or_default())),
        ("providerSpecificData".into(), Value::Object(details)),
    ]);
    if let Some(refresh) = refresh {
        fields.insert("refreshToken".into(), json!(refresh));
    }
    Ok(Credentials(fields))
}
