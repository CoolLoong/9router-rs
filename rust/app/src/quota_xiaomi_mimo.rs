use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::{Client, Response, StatusCode, header};
use serde_json::{Value, json};

use crate::mimo_account::{self, Error, Request};
use crate::provider_connections::truthy;
use crate::provider_http::{Delivery, http_client};
use crate::provider_index::js_trim;
use crate::storage::Storage;

use super::{Failure, proxy_unavailable};

const PLAN: &str = "Xiaomi MiMo Desktop";
const USAGE_URL: &str = "https://aistudio.xiaomimimo.com/open-apis/v1/user/usage";
const FIXTURE_URL: &str = "http://127.0.0.1:52989/mimo-key-quota/open-apis/v1/user/usage";
const TIMEOUT: Duration = Duration::from_secs(10);
static CLIENT: LazyLock<Client> = LazyLock::new(|| {
    http_client(TIMEOUT)
        .build()
        .expect("MiMo quota HTTP client")
});

pub(super) async fn read(storage: &Storage, account: &Value) -> Result<Value, Failure> {
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(proxy_unavailable)?;
    match mimo_account::send(storage, &settings, account, Request::Usage).await {
        Ok(response) if response.status().is_success() => {
            if let Some(data) = usage_data(response).await
                && let Some(percent) = percent(&data)
            {
                return Ok(weekly(percent, reset_at(&data, true)));
            }
        }
        Ok(response)
            if matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) => {}
        Ok(response) => return Ok(api_error(response.status())),
        Err(Error::Missing | Error::Rejected) => {}
        Err(Error::Route) => return Err(proxy_unavailable()),
        Err(Error::Unavailable) => return Ok(network_error()),
    }

    let details = &account["providerSpecificData"];
    let token = &account["accessToken"];
    let key = if truthy(token) {
        token
    } else {
        &details["apiKey"]
    };
    let Some(key) = key
        .as_str()
        .map(js_trim)
        .filter(|key| key.starts_with("sk-") && key.len() > 3 && details["mimoPassToken"] != *key)
    else {
        return Ok(
            json!({"message":"Xiaomi MiMo Desktop not connected. Add credentials to view usage."}),
        );
    };
    let delivery = Delivery::select(storage, &settings, account, USAGE_URL)
        .await
        .map_err(|_| proxy_unavailable())?;
    let client = delivery
        .client(&CLIENT, TIMEOUT)
        .ok_or_else(proxy_unavailable)?;
    let url = if delivery.use_fixture() {
        FIXTURE_URL
    } else {
        USAGE_URL
    };
    let response = client
        .get(url)
        .bearer_auth(key)
        .header("X-Mimo-Source", "mimocode-cli")
        .header(header::ACCEPT, "application/json")
        .send()
        .await;
    let Ok(response) = response else {
        return Ok(network_error());
    };
    if response.status() == StatusCode::UNAUTHORIZED {
        return Ok(json!({
            "plan":PLAN,
            "message":"Weekly quota requires Xiaomi account session. API key alone is insufficient.",
        }));
    }
    if !response.status().is_success() {
        return Ok(api_error(response.status()));
    }
    let Some(data) = usage_data(response).await else {
        return Ok(json!({"plan":PLAN,"message":"Usage endpoint returned unexpected response."}));
    };
    let Some(percent) = percent(&data) else {
        return Ok(json!({"plan":PLAN,"message":"Usage data missing percent field."}));
    };
    Ok(weekly(percent, reset_at(&data, false)))
}

async fn usage_data(response: Response) -> Option<Value> {
    let mut envelope: Value = response.json().await.ok()?;
    (envelope["code"].as_f64() == Some(0.0) && truthy(&envelope["data"]))
        .then(|| envelope["data"].take())
}

fn percent(data: &Value) -> Option<u64> {
    data["percent"]
        .as_f64()
        .filter(|percent| percent.is_finite())
        .map(|percent| percent.round().clamp(0.0, 100.0) as u64)
}

fn reset_at(data: &Value, session: bool) -> Option<String> {
    let date = if session && let Some(seconds) = data["resetAt"].as_f64().filter(|time| *time > 0.0)
    {
        let millis = seconds * 1000.0;
        (millis <= 8.64e15)
            .then(|| DateTime::<Utc>::from_timestamp_millis(millis as i64))
            .flatten()
    } else {
        let day = data["resetDate"].as_str()?;
        DateTime::parse_from_rfc3339(&format!("{day}T00:00:00Z"))
            .ok()
            .map(|date| date.with_timezone(&Utc))
    };
    date.map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn weekly(remaining: u64, reset: Option<String>) -> Value {
    json!({
        "plan":PLAN,
        "quotas":{"Weekly":{
            "used":100-remaining,"total":100,"remainingPercentage":remaining,"resetAt":reset,"unlimited":false,
        }},
    })
}

fn api_error(status: StatusCode) -> Value {
    json!({"plan":PLAN,"message":format!("Usage API error ({})",status.as_u16())})
}

fn network_error() -> Value {
    json!({"message":"Xiaomi MiMo Desktop usage error: fetch failed"})
}
