use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

use super::super::ReadScope;
use super::{QuotaDelivery, parse, truthy};
use crate::commandcode_identity::session_headers;
use crate::provider_index::js_trim;

const DEFAULT: &str = "https://api.commandcode.ai";
const PLAN: &str = "Command Code";

pub(crate) struct Endpoints {
    pub(crate) whoami: String,
    pub(crate) credits: String,
    pub(crate) subscriptions: String,
}

impl Endpoints {
    pub(crate) fn configured() -> Self {
        let base = std::env::var("COMMAND_CODE_API_BASE_URL")
            .ok()
            .filter(|base| !base.is_empty())
            .unwrap_or_else(|| DEFAULT.into());
        let base = base.trim_end_matches('/');
        Self {
            whoami: format!("{base}/alpha/whoami"),
            credits: format!("{base}/alpha/billing/credits"),
            subscriptions: format!("{base}/alpha/billing/subscriptions"),
        }
    }

    pub(super) fn targets(&self) -> [Option<&str>; 3] {
        [
            Some(&self.whoami),
            Some(&self.credits),
            Some(&self.subscriptions),
        ]
    }

    pub(super) async fn usage(&self, read: &QuotaDelivery, account: &Value) -> Value {
        let Some(key) = account["apiKey"]
            .as_str()
            .map(js_trim)
            .filter(|key| !key.is_empty())
        else {
            return json!({"message":"Command Code API key not available. Add a key to view usage."});
        };
        if self.targets().into_iter().flatten().any(|endpoint| {
            !Url::parse(endpoint).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.has_host()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none()
            })
        }) {
            return json!({"message":"Command Code error: invalid endpoint"});
        }
        let session = Uuid::new_v4().to_string();
        let get = |endpoint: &str| {
            let endpoint = if matches!(read.scope, ReadScope::Direct | ReadScope::Bypass) {
                fixture_url(endpoint)
            } else {
                endpoint.to_owned()
            };
            session_headers(read.client.get(endpoint), &session)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .bearer_auth(key)
                .timeout(Duration::from_secs(10))
        };
        let Ok(info) = read.send(get(&self.whoami).query(&[("limits", "1")])).await else {
            return json!({"message":"Command Code error: fetch failed"});
        };
        if authenticator(info.status()) {
            return denied();
        }
        if !info.status().is_success() {
            return usage_error("usage", info.status());
        }
        let whoami = info.json::<Value>().await.unwrap_or_default();
        let organization = whoami["org"]
            .get("id")
            .filter(|id| !id.is_null())
            .map(parse::string);
        let request = |url: &str| {
            let request = get(url);
            if let Some(id) = &organization {
                request.query(&[("orgId", id)])
            } else {
                request
            }
        };
        let (Ok(credits), Ok(subs)) = tokio::join!(
            read.send(request(&self.credits)),
            read.send(request(&self.subscriptions))
        ) else {
            return json!({"message":"Command Code error: fetch failed"});
        };
        if authenticator(credits.status()) || authenticator(subs.status()) {
            return denied();
        }
        if !credits.status().is_success() {
            return usage_error("credits", credits.status());
        }
        if !subs.status().is_success() {
            return usage_error("subscriptions", subs.status());
        }
        let (credits, subscription) = tokio::join!(credits.json::<Value>(), subs.json::<Value>());
        payload(
            &credits.unwrap_or_default(),
            &subscription.unwrap_or_default(),
            key,
        )
    }
}

fn fixture_url(endpoint: &str) -> String {
    if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
        && let Some(path) = endpoint.strip_prefix(DEFAULT)
    {
        return format!("http://127.0.0.1:52989/commandcode{path}");
    }
    endpoint.to_owned()
}

fn authenticator(status: StatusCode) -> bool {
    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
}

fn denied() -> Value {
    json!({"plan":PLAN,"message":"Command Code authentication failed. Check the API key."})
}

fn usage_error(kind: &str, status: StatusCode) -> Value {
    json!({"plan":PLAN,"message":format!("Command Code {kind} API error ({})",status.as_u16())})
}

fn payload(billing: &Value, subs: &Value, key: &str) -> Value {
    let raw = subs["data"]["planId"]
        .as_str()
        .filter(|plan| !plan.is_empty() && !plan.contains(key))
        .unwrap_or(PLAN);
    let (plan, cap) = match raw.trim().to_ascii_lowercase().as_str() {
        "go" | "go-v1" | "individual-go" | "individual-go-v1" => ("Go", 10.0),
        "goat" | "individual-goat" => ("GOAT", 70.0),
        "pro" | "individual-pro" => ("Pro", 30.0),
        "individual-pro-v1" => ("Pro", 80.0),
        "provider" | "individual-provider" => ("Provider", 15.0),
        "max" | "individual-max" => ("Max", 150.0),
        "ultra" | "individual-ultra" => ("Ultra", 300.0),
        "teams-pro" => ("Teams Pro", 40.0),
        _ => (raw, 0.0),
    };
    let credits = &billing["credits"];
    let remaining = ["monthlyCredits", "purchasedCredits", "freeCredits"]
        .into_iter()
        .map(|key| parse::finite(&credits[key]))
        .sum::<f64>();
    let used = if cap > 0.0 {
        (cap - remaining).max(0.0)
    } else {
        0.0
    };
    let total = if cap > 0.0 { cap } else { remaining };
    let mut quotas = json!({"Credits":{
        "name":"Monthly","used":parse::number(used),"total":parse::number(total),
        "percentage":ratio(used,cap,0),"colorPercentage":ratio(remaining,cap,100),
        "percentageIsUsed":true,"remaining":parse::number(remaining),"unlimited":cap<=0.0,
        "resetAt":parse::reset_date(&subs["data"]["currentPeriodEnd"],true),
    }});
    for (window, field, label) in [
        ("fiveHour", "Session (5h)", "5-hour limit"),
        ("weekly", "Weekly", "Weekly limit"),
    ] {
        let data = &billing["windowLimits"][window];
        let used = parse::finite(&data["used"]);
        let total = parse::finite(&data["cap"]);
        if total <= 0.0 && used <= 0.0 {
            continue;
        }
        quotas[field] = json!({"used":parse::number(used),"total":parse::number(total),
            "percentage":ratio(used,total,0),"colorPercentage":ratio(total-used,total,100),
            "percentageIsUsed":true,"remaining":parse::number((total-used).max(0.0)),
            "unlimited":false,"resetAt":parse::reset_date(&data["resetAt"],true),
            "emptyMessage":(used == 0.0 && !truthy(&data["resetAt"])).then_some("No usage in this window"),
            "name":label});
    }
    json!({"plan":plan,"quotas":quotas})
}

fn ratio(numerator: f64, denominator: f64, fallback: i32) -> Value {
    if denominator > 0.0 {
        parse::number(((numerator / denominator) * 100.0 + 0.5).floor())
    } else {
        json!(fallback)
    }
}
