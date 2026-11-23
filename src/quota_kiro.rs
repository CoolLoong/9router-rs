use std::time::Duration;

use axum::http::StatusCode;
use reqwest::RequestBuilder;
use serde_json::{Map, Value, json};

use super::{QuotaDelivery, parse, pinned_field, redact, truthy};

pub(crate) struct Endpoints {
    pub(crate) codewhisperer: String,
    pub(crate) amazon_q: String,
    path: String,
}

impl Endpoints {
    pub(super) fn pinned() -> Option<Self> {
        Some(Self {
            codewhisperer: pinned_field("kiro.usage", "cwHost")?,
            amazon_q: pinned_field("kiro.usage", "qHost")?,
            path: pinned_field("kiro.usage", "limitsPath")?,
        })
    }

    pub(super) async fn usage(&self, read: &QuotaDelivery, account: &Value) -> Value {
        let client = &read.client;
        let details = &account["providerSpecificData"];
        let method = details["authMethod"]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or("builder-id");
        let api_key = method == "api_key"
            || matches!(account["authType"].as_str(), Some("api_key" | "apikey"));
        let token = account["accessToken"]
            .as_str()
            .filter(|value| !value.is_empty())
            .or_else(|| api_key.then(|| account["apiKey"].as_str()).flatten());
        let Some(token) = token.filter(|value| !value.is_empty()) else {
            return json!({"message":"Kiro access token not available. Re-connect the provider.","quotas":{}});
        };
        let profile = details["profileArn"]
            .as_str()
            .filter(|arn| arn.starts_with("arn:aws:codewhisperer:"));
        let auth = |request: RequestBuilder| {
            let request = request
                .bearer_auth(token)
                .header("Accept", "application/json")
                .timeout(Duration::from_secs(10));
            if api_key {
                request.header("TokenType", "API_KEY")
            } else if method == "external_idp" {
                request.header("TokenType", "EXTERNAL_IDP")
            } else {
                request
            }
        };
        let mut post_body = json!({"origin":"AI_EDITOR","resourceType":"AGENTIC_REQUEST"});
        let mut query = vec![("origin", "AI_EDITOR"), ("resourceType", "AGENTIC_REQUEST")];
        if let Some(profile) = profile {
            post_body["profileArn"] = json!(profile);
            query.push(("profileArn", profile));
        }
        let attempts = [
            (
                "codewhisperer-get",
                auth(client.get(format!("{}{}", self.codewhisperer, self.path)))
                    .header("x-amz-user-agent", "aws-sdk-js/1.0.0 KiroIDE")
                    .query(&[
                        ("isEmailRequired", "true"),
                        ("origin", "AI_EDITOR"),
                        ("resourceType", "AGENTIC_REQUEST"),
                    ]),
            ),
            (
                "codewhisperer-post",
                auth(client.post(&self.codewhisperer))
                    .header("Content-Type", "application/x-amz-json-1.0")
                    .header("x-amz-target", "AmazonCodeWhispererService.GetUsageLimits")
                    .json(&post_body),
            ),
            (
                "q-get",
                auth(client.get(format!("{}{}", self.amazon_q, self.path))).query(&query),
            ),
        ];
        let mut auth_error = false;
        let mut last_error = String::new();
        for (name, request) in attempts {
            let Ok(response) = read.send(request).await else {
                last_error = format!("({name}:fetch failed)");
                continue;
            };
            let status = response.status();
            if !status.is_success() {
                auth_error |= matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
                let raw = redact(
                    read.redact_relay(response.text().await.unwrap_or_default()),
                    account,
                );
                let detail = raw
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(160)
                    .collect::<String>();
                let suffix = if detail.is_empty() {
                    String::new()
                } else {
                    format!(":{detail}")
                };
                last_error = format!("({name}:{}{suffix})", status.as_u16());
                continue;
            }
            if let Ok(data) = response.json::<Value>().await {
                return usage(&data, token);
            }
            last_error = format!("({name}:invalid provider response)");
        }
        let message = if auth_error {
            match method {
                "idc" => "Kiro quota API is unavailable for the current AWS IAM Identity Center session. Chat may still work. If this persists after renewing your session, reconnect Kiro.",
                "google" | "github" => "Kiro quota API authentication expired. Chat may still work.",
                _ => "Kiro quota API rejected the current token. Chat may still work.",
            }.to_owned()
        } else {
            format!("Unable to fetch Kiro usage right now. {last_error}")
                .trim_end()
                .to_owned()
        };
        json!({"message":message,"quotas":{}})
    }
}

fn usage(data: &Value, token: &str) -> Value {
    let reset = data
        .get("nextDateReset")
        .filter(|time| truthy(time))
        .unwrap_or(&data["resetDate"]);
    let reset = parse::reset_date(reset, true);
    let mut quotas = Map::new();
    for breakdown in data["usageBreakdownList"].as_array().into_iter().flatten() {
        let name = breakdown["resourceType"]
            .as_str()
            .filter(|name| !name.is_empty() && !name.contains(token))
            .unwrap_or("unknown")
            .to_lowercase();
        quotas.insert(name.clone(), quota(breakdown, &reset));
        if let Some(trial) = breakdown.get("freeTrialInfo").filter(|trial| truthy(trial)) {
            let expiry = trial
                .get("freeTrialExpiry")
                .filter(|expiry| truthy(expiry))
                .map(|expiry| parse::reset_date(expiry, true))
                .unwrap_or_else(|| reset.clone());
            quotas.insert(format!("{name}_freetrial"), quota(trial, &expiry));
        }
    }
    let plan = data["subscriptionInfo"]["subscriptionTitle"]
        .as_str()
        .filter(|plan| !plan.is_empty() && !plan.contains(token))
        .unwrap_or("Kiro");
    json!({"plan":plan,"quotas":quotas})
}

fn quota(item: &Value, reset: &Value) -> Value {
    let used = parse::value_or(&item["currentUsageWithPrecision"], json!(0));
    let total = parse::value_or(&item["usageLimitWithPrecision"], json!(0));
    json!({"used":used,"total":total,"remaining":parse::number(parse::finite(&total)-parse::finite(&used)),
        "resetAt":reset,"unlimited":false})
}
