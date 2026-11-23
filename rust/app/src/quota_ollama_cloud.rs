use chrono::{DateTime, Datelike, Months, SecondsFormat, Timelike, Utc};
use reqwest::{RequestBuilder, StatusCode, Url, header};
use serde_json::{Map, Value, json};

use super::{QuotaDelivery, coerce_quantity, parse, supplied};

pub(in super::super) struct Endpoints {
    pub(in super::super) usage: String,
    pub(in super::super) identity: String,
    pub(in super::super) balance: String,
}

impl Endpoints {
    pub(super) fn pinned() -> Self {
        Self {
            usage: "https://ollama.com/api/usage".into(),
            identity: "https://ollama.com/api/me".into(),
            balance: "https://ollama.com/api/balance".into(),
        }
    }

    pub(in super::super) fn shared_destination(&self) -> Option<&str> {
        let origin = Url::parse(&self.usage).ok()?.origin();
        [&self.identity, &self.balance]
            .into_iter()
            .all(|target| Url::parse(target).is_ok_and(|target| target.origin() == origin))
            .then_some(&self.usage)
    }

    pub(super) async fn read(&self, delivery: &QuotaDelivery, account: &Value) -> Value {
        let Some(key) = supplied(&account["apiKey"]) else {
            return json!({"message":"Ollama Cloud API key not available."});
        };
        let client = &delivery.client;
        let response = match delivery
            .send(authenticated(client.get(&self.usage), key))
            .await
        {
            Ok(response) => response,
            Err(message) => return json!({"message":format!("Ollama Cloud error: {message}")}),
        };
        let status = response.status();
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return json!({"message":"Ollama Cloud API key invalid or expired."});
        }
        if !status.is_success() {
            return json!({"message":format!("Ollama Cloud usage API error ({}).",status.as_u16())});
        }
        let Ok(usage) = response.json::<Value>().await else {
            return json!({"message":"Ollama Cloud usage response was not JSON."});
        };
        let profile = read_json(
            delivery,
            authenticated(client.post(&self.identity), key).header(header::CONTENT_LENGTH, "0"),
        )
        .await;
        let raw_plan = profile["Plan"].as_str().unwrap_or_default();
        let plan = label(raw_plan);
        let monthly = raw_plan
            .eq_ignore_ascii_case("free")
            .then(|| parse::reset_time(&profile["CreatedAt"], false))
            .flatten()
            .and_then(|created| next_signup_reset(created, Utc::now()));
        let mut quotas = ratio_windows(&usage["limits"], monthly);
        if quotas.is_empty() {
            let balance = read_json(delivery, authenticated(client.get(&self.balance), key)).await;
            quotas = balance_windows(&balance);
        }
        if quotas.is_empty() {
            return json!({"plan":plan,"message":"Ollama Cloud connected. No usage limits reported.","quotas":{}});
        }
        json!({"plan":plan,"quotas":quotas})
    }
}

fn authenticated(request: RequestBuilder, key: &str) -> RequestBuilder {
    request
        .bearer_auth(key)
        .header(header::ACCEPT, "application/json")
}

async fn read_json(delivery: &QuotaDelivery, request: RequestBuilder) -> Value {
    let Ok(response) = delivery.send(request).await else {
        return Value::Null;
    };
    if !response.status().is_success() {
        return Value::Null;
    }
    response.json().await.unwrap_or_default()
}

fn label(plan: &str) -> String {
    let mut chars = plan.chars();
    chars.next().map_or_else(
        || "Ollama Cloud".into(),
        |first| first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
    )
}

fn next_signup_reset(created: DateTime<Utc>, now: DateTime<Utc>) -> Option<String> {
    let month_diff =
        (now.year() - created.year()) * 12 + (now.month() as i32 - created.month() as i32);
    let base = created.with_nanosecond(0)?;
    (month_diff.max(0)..=month_diff + 1)
        .find_map(|months| {
            base.checked_add_months(Months::new(months as u32))
                .filter(|date| *date > now)
        })
        .map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn ratio_windows(limits: &Value, monthly: Option<String>) -> Map<String, Value> {
    [("session","Session (5h)"),("weekly","Weekly (7d)"),("monthly","Monthly")]
        .into_iter().filter_map(|(period,label)| {
            let value = limits[period].get("usage").filter(|value| !value.is_null())?;
            let ratio = coerce_quantity(value)?.clamp(0.0,1.0);
            let used = (ratio*100.0).round() as u64;
            Some((label.into(),json!({"used":used,"total":100,"remainingPercentage":100-used,
                "resetAt":(period=="monthly").then_some(monthly.as_deref()).flatten(),"unlimited":false})))
        }).collect()
}

fn balance_windows(balance: &Value) -> Map<String, Value> {
    let included = &balance["included"];
    let mut quotas: Map<String,Value> = [("session","Session (5h)"),("weekly","Weekly (7d)")]
        .into_iter().filter_map(|(period,label)| {
            let window = included.get(period).filter(|value| crate::provider_connections::truthy(value))?;
            let remaining = window.get("remaining_percent").and_then(coerce_quantity).unwrap_or(100.0).clamp(0.0,100.0);
            Some((label.into(),json!({"used":cents(100.0-remaining),"total":100,
                "remainingPercentage":parse::number(remaining),"resetAt":parse::value_or(&window["resets_at"],Value::Null),
                "unlimited":false})))
        }).collect();
    if let Some((allowance, balance)) = included
        .get("allowance_usd")
        .zip(included.get("balance_usd"))
    {
        let allowance = coerce_quantity(allowance).unwrap_or_default();
        let balance = coerce_quantity(balance).unwrap_or_default();
        let remaining = if allowance > 0.0 {
            cents(balance / allowance * 100.0)
        } else {
            json!(0)
        };
        quotas.insert("Allowance ($)".into(),json!({"used":cents((allowance-balance).max(0.0)),
            "total":parse::number(allowance),"remainingPercentage":remaining,
            "resetAt":parse::value_or(&included["period"]["until"],Value::Null),"unlimited":false,"unit":"USD"}));
    }
    if let Some(purchased) =
        coerce_quantity(&balance["purchased"]["balance_usd"]).filter(|value| *value > 0.0)
    {
        quotas.insert(
            "Purchased Credits".into(),
            json!({"used":0,"total":parse::number(purchased),
            "remainingPercentage":100,"isCreditBalance":true,"currency":"$"}),
        );
    }
    quotas
}

fn cents(value: f64) -> Value {
    parse::number(((value * 100.0 + 0.5).floor()) / 100.0)
}
