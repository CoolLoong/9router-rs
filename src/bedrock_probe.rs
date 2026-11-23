use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;
use url::Url;

use super::{
    Egress, Redactions, fixture, http, region, resolve, select_destination, sign, tunnel_client,
};
use crate::storage::Storage;

const TIMEOUT: Duration = Duration::from_secs(8);
const LIMIT: usize = 128 * 1024;
const TIMED_OUT: &str = "AWS credential probe timed out";
const TRANSPORT_ERROR: &str = "fetch failed";

pub(crate) async fn check(storage: &Storage, account: &Value, saved: bool) -> Option<String> {
    let region = match region(account) {
        Ok(region) => region,
        Err(error) => return Some(error.to_owned()),
    };
    let Ok(endpoint) = Url::parse(&format!(
        "https://bedrock.{region}.amazonaws.com/foundation-models"
    )) else {
        return Some("Invalid Bedrock endpoint".into());
    };
    let Some(settings) = storage.read_settings().await else {
        return Some(TRANSPORT_ERROR.into());
    };
    let route_account = if saved { account } else { &Value::Null };
    let delivery = match select_destination(storage, &settings, route_account, &endpoint).await {
        Ok(delivery) => delivery,
        Err("Bedrock requires a direct connection or an HTTP or SOCKS tunnel") => {
            return Some(
                "AWS credential probes require an HTTP or SOCKS proxy, or a direct connection"
                    .into(),
            );
        }
        Err("Bedrock connection proxy is unavailable") => {
            return Some("AWS credential probe proxy is unavailable".into());
        }
        Err(_) => return Some(TRANSPORT_ERROR.into()),
    };
    let (client, url) = match &delivery {
        Egress::Direct | Egress::Bypassed => (
            http().clone(),
            if fixture() {
                format!("http://127.0.0.1:52989/bedrock-probe/{region}/foundation-models")
            } else {
                endpoint.to_string()
            },
        ),
        Egress::Tunnel(proxy) => match tunnel_client(proxy.as_str(), &endpoint) {
            Some(selected) => selected,
            None => return Some(TRANSPORT_ERROR.into()),
        },
    };
    let resolved = match resolve(storage, &settings, account, route_account).await {
        Ok(resolved) => resolved,
        Err(error) => return Some(error.to_owned()),
    };
    let Some(current) = storage.read_settings().await else {
        return Some(TRANSPORT_ERROR.into());
    };
    if !matches!(select_destination(storage, &current, route_account, &endpoint).await, Ok(actual) if actual == delivery)
    {
        return Some("AWS credential probe proxy changed before dispatch".into());
    }
    let credentials = resolved.credentials();
    let redactions = Redactions::for_credentials(credentials);
    let headers = match sign(
        &endpoint,
        "GET",
        &resolved.region,
        "bedrock",
        BTreeMap::from([("accept", "application/json")]),
        b"",
        credentials,
    ) {
        Ok(headers) => headers,
        Err(error) => return Some(error.to_owned()),
    };
    let response = match client
        .get(url)
        .headers(headers)
        .timeout(TIMEOUT)
        .send()
        .await
    {
        Ok(response) if !response.status().is_redirection() => response,
        Ok(_) => return Some(TRANSPORT_ERROR.into()),
        Err(error) if error.is_timeout() => return Some(TIMED_OUT.into()),
        Err(_) => return Some(TRANSPORT_ERROR.into()),
    };
    let status = response.status();
    let error_type = response
        .headers()
        .get("x-amzn-errortype")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(':').next())
        .filter(|value| !value.is_empty() && value.len() <= 128);
    if status.is_success() || error_type == Some("AccessDeniedException") {
        return None;
    }
    let code = error_type
        .map(str::to_owned)
        .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
    let mut problem = format!("AWS rejected the credentials ({code})");
    if let Some(message) = read_message(response).await {
        problem.push_str(": ");
        problem.push_str(&message);
    }
    Some(redactions.redact(&problem))
}

async fn read_message(mut response: reqwest::Response) -> Option<String> {
    if response
        .content_length()
        .is_some_and(|length| length > LIMIT as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > LIMIT {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let payload: Value = serde_json::from_slice(&bytes).ok()?;
    payload["message"]
        .as_str()
        .filter(|message| !message.is_empty())
        .map(str::to_owned)
}
