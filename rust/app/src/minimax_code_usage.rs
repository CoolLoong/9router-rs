use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, Local, SecondsFormat, Utc};
use md5::{Digest, Md5};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Client, StatusCode, Url};
use serde_json::{Map, Value, json};

use crate::provider_http::{Delivery, http_client};
use crate::provider_oauth::{RefreshMode, refresh_account};
use crate::storage::Storage;

const TIMEOUT: Duration = Duration::from_secs(15);
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');
const SIGNATURE_SALT: &str = "I*7Cf%WZ#S&%1RlZJ&C2";
static CLIENT: LazyLock<Option<Client>> = LazyLock::new(|| http_client(TIMEOUT).build().ok());

struct Site {
    agent: &'static str,
    platform: &'static str,
    language: &'static str,
}

impl Site {
    fn from_provider(provider: &str) -> Option<Self> {
        match provider {
            "minimax-code" => Some(Self {
                agent: "https://agent.minimaxi.com",
                platform: "https://www.minimaxi.com",
                language: "zh",
            }),
            "minimax-code-global" => Some(Self {
                agent: "https://agent.minimax.io",
                platform: "https://platform.minimax.io",
                language: "en",
            }),
            _ => None,
        }
    }
}

pub(crate) fn supports(provider: &str) -> bool {
    Site::from_provider(provider).is_some()
}

pub(crate) async fn read(storage: &Storage, mut account: Value) -> (StatusCode, Value) {
    let oauth = account["authType"] == "oauth";
    if oauth {
        match refresh_account(storage, &account, RefreshMode::Proactive).await {
            Ok(current) => account = current,
            Err(error)
                if field(&account, "accessToken").is_none()
                    || error == crate::provider_oauth::RefreshError::ReauthenticationRequired =>
            {
                return (
                    StatusCode::UNAUTHORIZED,
                    json!({"error":format!("Credential refresh failed: {error}")}),
                );
            }
            Err(_) => (),
        }
    } else if account["authType"] == "mcode-lease" {
        if let Ok(current) = super::lease::credentials(&account, None).await {
            account = current;
        }
    } else {
        return (
            StatusCode::OK,
            json!({"message":"Usage not available for this connection"}),
        );
    }

    let mut result = usage(storage, &account).await;
    if oauth
        && matches!(result, Err(Failure::Expired(_)))
        && field(&account, "refreshToken").is_some()
        && let Ok(current) = refresh_account(storage, &account, RefreshMode::Rejected).await
    {
        account = current;
        result = usage(storage, &account).await;
    }
    let result = match result {
        Ok(result) => result,
        Err(Failure::Expired(detail)) => json!({
            "message":format!("MiniMax Code sign-in expired — unauthorized, please re-authorize ({detail})"),
        }),
        Err(Failure::Other(message)) => json!({"message":message}),
    };
    (StatusCode::OK, result)
}

enum Failure {
    Expired(String),
    Other(String),
}

impl Failure {
    fn other(message: &str) -> Self {
        Self::Other(message.into())
    }
}

struct Request<'a> {
    storage: &'a Storage,
    settings: Value,
    account: &'a Value,
    token: &'a str,
    provider: &'a str,
    site: Site,
}

impl Request<'_> {
    async fn account(
        &self,
        path: &str,
        user_id: &str,
        body: Option<Value>,
        what: &str,
    ) -> Result<Value, Failure> {
        let now = Local::now();
        let unix = now.timestamp_millis().to_string();
        let ts = now.timestamp().to_string();
        let offset = now.offset().local_minus_utc().to_string();
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            platform => platform,
        };
        let mut url = Url::parse(&format!("{}{path}", self.site.agent))
            .map_err(|_| Failure::other("Invalid MiniMax Code account endpoint"))?;
        url.query_pairs_mut().extend_pairs([
            ("device_platform", "mcode"),
            ("biz_id", "3"),
            ("app_id", "3001"),
            ("version_code", "22201"),
            ("unix", &unix),
            ("timezone_offset", &offset),
            ("sys_language", self.site.language),
            ("lang", self.site.language),
            ("device_id", "0"),
            ("os_name", os),
            ("browser_name", "mcode"),
            ("user_id", user_id),
            ("client", "mcode"),
        ]);
        let target = format!("{}?{}", url.path(), url.query().unwrap_or_default());
        let encoded = utf8_percent_encode(&target, COMPONENT);
        let body = body.map(|body| body.to_string());
        let hash = |source: &str| format!("{:x}", Md5::digest(source.as_bytes()));
        let nonce = hash(&unix);
        let yy = hash(&format!(
            "{encoded}_{}{nonce}ooui",
            body.as_deref().unwrap_or("{}")
        ));
        let signature = hash(&format!(
            "{ts}{SIGNATURE_SALT}{}",
            body.as_deref().unwrap_or_default()
        ));
        self.send(url, body, Some((yy, ts, signature)), None, what)
            .await
    }

    async fn send(
        &self,
        mut url: Url,
        body: Option<String>,
        signature: Option<(String, String, String)>,
        group: Option<&str>,
        what: &str,
    ) -> Result<Value, Failure> {
        let delivery = Delivery::select(self.storage, &self.settings, self.account, url.as_str())
            .await
            .map_err(|_| Failure::other("MiniMax Code quota route is unavailable"))?;
        if delivery.use_fixture() {
            let local = format!(
                "http://127.0.0.1:52989/minimax-code-usage/{}{}",
                self.provider,
                url.path()
            );
            let mut fixture =
                Url::parse(&local).map_err(|_| Failure::other("Invalid MiniMax Code fixture"))?;
            fixture.set_query(url.query());
            url = fixture;
        }
        let client = CLIENT
            .as_ref()
            .and_then(|client| delivery.client(client, TIMEOUT))
            .ok_or_else(|| Failure::other("MiniMax Code quota client is unavailable"))?;
        let mut request = match body {
            Some(body) => client.post(url).body(body),
            None => client.get(url),
        }
        .bearer_auth(self.token)
        .header("Accept", "application/json");
        let signed = signature.is_some();
        if let Some((yy, ts, signature)) = signature {
            request = request
                .header("Content-Type", "application/json")
                .header("User-Agent", "MiniMaxCode")
                .header("yy", yy)
                .header("x-timestamp", ts)
                .header("x-signature", signature);
        }
        if let Some(group) = group {
            request = request.header("X-Group-Id", group);
        }
        let reply = request
            .send()
            .await
            .map_err(|_| Failure::Other(format!("{what}: request failed")))?;
        let status = reply.status();
        if signed && matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(Failure::Expired(format!(
                "{what}: the sign-in was refused (HTTP {})",
                status.as_u16()
            )));
        }
        if !status.is_success() {
            return Err(Failure::Other(format!("{what}: HTTP {}", status.as_u16())));
        }
        let data: Value = reply
            .json()
            .await
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| Failure::Other(format!("{what}: not JSON")))?;
        if signed
            && let Some(code) = data["statusInfo"]["code"]
                .as_i64()
                .filter(|code| *code != 0)
        {
            if code == 1000048 {
                return Err(Failure::Expired(format!("{what}: the sign-in was refused")));
            }
            let status = &data["statusInfo"];
            let message = field(status, "message")
                .or_else(|| field(status, "msg"))
                .map_or_else(|| format!("status {code}"), str::to_owned);
            return Err(Failure::Other(self.redact(&format!("{what}: {message}"))));
        }
        if let Some(code) = data["base_resp"]["status_code"]
            .as_i64()
            .filter(|code| *code != 0)
        {
            let message = field(&data["base_resp"], "status_msg").map_or_else(
                || {
                    if signed {
                        format!("status {code}")
                    } else {
                        format!("MiniMax said {code}")
                    }
                },
                str::to_owned,
            );
            let message = if signed {
                format!("{what}: {message}")
            } else {
                message
            };
            return Err(Failure::Other(self.redact(&message)));
        }
        Ok(data)
    }

    fn redact(&self, message: &str) -> String {
        let mut message = message.replace(self.token, "[redacted]");
        if let Some(refresh) = field(self.account, "refreshToken") {
            message = message.replace(refresh, "[redacted]");
        }
        message
    }
}

async fn usage(storage: &Storage, account: &Value) -> Result<Value, Failure> {
    let provider = account["provider"].as_str().unwrap_or_default();
    let (Some(site), Some(token)) = (Site::from_provider(provider), field(account, "accessToken"))
    else {
        return Ok(json!({"message":format!("Usage API not implemented for {provider}")}));
    };
    let settings = storage
        .read_settings()
        .await
        .ok_or_else(|| Failure::other("MiniMax Code quota settings are unavailable"))?;
    let request = Request {
        storage,
        settings,
        account,
        token,
        provider,
        site,
    };
    let saved_user = field(&account["providerSpecificData"], "realUserID");
    let identity = if saved_user.is_none() {
        request
            .account("/v1/api/user/info", "0", None, "account")
            .await?
    } else {
        Value::Null
    };
    let user_info = [
        &identity["data"]["userInfo"],
        &identity["data"]["user_info"],
        &identity["userInfo"],
        &identity["user_info"],
    ]
    .into_iter()
    .find(|value| value.is_object())
    .unwrap_or(&Value::Null);
    let user_id = saved_user
        .or_else(|| field(user_info, "realUserID"))
        .or_else(|| field(user_info, "real_user_id"))
        .ok_or_else(|| Failure::other("account: no user id in the reply"))?;
    let membership = membership(&request, user_id).await?;
    let user = [
        "userEmail",
        "email",
        "userMail",
        "user_email",
        "name",
        "userName",
        "user_name",
    ]
    .into_iter()
    .find_map(|key| field(user_info, key))
    .unwrap_or(user_id);
    let mut quotas = Map::new();
    if let Some(balance) = membership.get("balance") {
        quotas.insert(
            "Credits".into(),
            json!({
                "used":0,"total":balance.as_f64()
                    .or_else(|| balance.as_str()?.parse::<f64>().ok())
                    .filter(|balance| balance.is_finite()).unwrap_or(0.0),
                "isCreditBalance":true,"currency":"credits",
            }),
        );
    }
    let has_plan = membership.get("has_token_plan").and_then(Value::as_bool);
    if has_plan == Some(true)
        && let Some(group) = membership.get("op_group_id").and_then(Value::as_str)
    {
        let url = Url::parse(&format!(
            "{}/v1/api/openplatform/coding_plan/remains",
            request.site.platform
        ))
        .map_err(|_| Failure::other("Invalid MiniMax Code plan endpoint"))?;
        match request
            .send(url, None, None, Some(group), "M Plan")
            .await
            .and_then(|data| windows(&data))
        {
            Ok(windows) => quotas.extend(windows),
            Err(Failure::Expired(message) | Failure::Other(message)) => {
                quotas.insert(
                    "M Plan windows".into(),
                    json!({
                        "name":"M Plan windows","used":0,"total":0,"resetAt":null,"message":message,
                    }),
                );
            }
        }
    }
    if quotas.is_empty() {
        let message = if has_plan == Some(false) {
            "Free account (no M Plan)"
        } else {
            "No usage data returned"
        };
        quotas.insert(
            "Plan".into(),
            json!({"name":"Plan","used":0,"total":0,"resetAt":null,"message":message}),
        );
    }
    let plan = if has_plan == Some(true) {
        membership
            .get("token_plan_tier")
            .and_then(Value::as_str)
            .map_or_else(|| "M Plan".to_owned(), |tier| format!("M Plan {tier}"))
    } else {
        "Free".into()
    };
    let mut result = json!({"quotas":quotas,"plan":plan,"user":user});
    if let Some(deadline) = membership
        .get("token_plan_expires_at")
        .and_then(Value::as_i64)
        .and_then(timestamp)
    {
        result["planExpiresAt"] = json!(deadline);
    }
    Ok(result)
}

async fn membership(request: &Request<'_>, user: &str) -> Result<Map<String, Value>, Failure> {
    const MEMBERSHIP: &str = "/matrix/api/v1/commerce/get_membership_info";
    let env = match request
        .account(
            "/matrix/api/v1/user/get_user_extra_info",
            user,
            Some(json!({})),
            "workspace",
        )
        .await
    {
        Ok(env) => env,
        Err(Failure::Other(_)) => {
            return request
                .account(MEMBERSHIP, user, Some(json!({})), "membership")
                .await
                .map(|body| plan(&body));
        }
        Err(error) => return Err(error),
    };
    let workspace = env["workspaces"]
        .as_array()
        .or_else(|| env["data"]["workspaces"].as_array())
        .into_iter()
        .flatten()
        .filter(|workspace| workspace["workspace_type"] == 0)
        .find(|workspace| {
            workspace["workspace_id"].as_u64().is_some()
                || field(workspace, "workspace_id").is_some()
        });
    let Some(workspace) = workspace else {
        return Ok(Map::new());
    };
    let mut out = plan(workspace);
    let id = field(workspace, "workspace_id")
        .map_or_else(|| workspace["workspace_id"].clone(), |id| json!(id));
    match request
        .account(
            MEMBERSHIP,
            user,
            Some(json!({"workspace_id":id})),
            "membership",
        )
        .await
    {
        Ok(body) => {
            let mut detailed = plan(&body);
            if out.get("op_group_id").is_some() {
                detailed.shift_remove("op_group_id");
            }
            if out.get("has_token_plan").and_then(Value::as_bool) == Some(true) {
                detailed.shift_remove("has_token_plan");
            }
            out.extend(detailed);
        }
        Err(Failure::Other(_)) => (),
        Err(error) => return Err(error),
    }
    Ok(out)
}

fn plan(body: &Value) -> Map<String, Value> {
    let pick = |name: &str| {
        body.get(name)
            .filter(|value| !value.is_null())
            .unwrap_or(&body["data"][name])
    };
    let mut out = Map::new();
    if let Some(has) = pick("has_token_plan").as_bool() {
        out.insert("has_token_plan".into(), json!(has));
    }
    for name in ["op_group_id", "token_plan_tier"] {
        if let Some(text) = pick(name)
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            out.insert(name.into(), json!(text));
        }
    }
    if let Some(time) = pick("token_plan_expires_at")
        .as_i64()
        .filter(|time| *time > 0)
    {
        out.insert("token_plan_expires_at".into(), json!(time));
    }
    let balances = [
        body.get("op_credit_summary"),
        body["data"].get("op_credit_summary"),
    ]
    .into_iter()
    .flatten()
    .find(|value| value.is_object());
    let balance = balances
        .and_then(|value| field(value, "total_remaining_amount"))
        .map(|text| json!(text))
        .or_else(|| {
            let balance = pick("opcredit_balance");
            balance
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| json!(text))
                .or_else(|| balance.is_number().then(|| balance.clone()))
        });
    if let Some(balance) = balance {
        out.insert("balance".into(), balance);
    }
    out
}

fn windows(data: &Value) -> Result<Map<String, Value>, Failure> {
    if !data["base_resp"].is_object() && !data["model_remains"].is_array() {
        return Err(Failure::other("no plan in the reply"));
    }
    let mut out = Map::new();
    for model in data["model_remains"].as_array().into_iter().flatten() {
        let Some(name) = field(model, "model_name") else {
            continue;
        };
        for (week, status, remaining, start, end) in [
            (
                false,
                "current_interval_status",
                "current_interval_remaining_percent",
                "start_time",
                "end_time",
            ),
            (
                true,
                "current_weekly_status",
                "current_weekly_remaining_percent",
                "weekly_start_time",
                "weekly_end_time",
            ),
        ] {
            let status = model[status].as_i64().unwrap_or_default();
            let left = match (status, model[remaining].as_f64()) {
                (3, _) => continue,
                (2, _) => 0.0,
                (_, Some(left)) => left.clamp(0.0, 100.0),
                _ => continue,
            };
            let start = model[start].as_i64().unwrap_or_default();
            let end = model[end].as_i64().unwrap_or_default();
            let span = if start > 0 && end > start {
                millis(end).saturating_sub(millis(start))
            } else {
                0
            };
            let label = if week {
                "7 days".into()
            } else if span > 86_400_000 && span % 86_400_000 == 0 {
                format!("{} days", span / 86_400_000)
            } else if span >= 3_600_000 && span % 3_600_000 == 0 {
                format!("{} hours", span / 3_600_000)
            } else if span > 0 {
                format!("{} minutes", span / 60_000)
            } else {
                "Allowance".into()
            };
            let name = if name.eq_ignore_ascii_case("general") {
                label
            } else {
                let mut chars = name.chars();
                let first = chars
                    .next()
                    .map_or_else(String::new, |first| first.to_uppercase().to_string());
                format!("{first}{} · {label}", chars.as_str())
            };
            out.insert(
                name.clone(),
                json!({
                    "name":name,"used":100.0-left,"total":100,"remaining":left,
                    "remainingPercentage":left,"resetAt":timestamp(end),
                }),
            );
        }
    }
    Ok(out)
}

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn millis(time: i64) -> i64 {
    if time < 1_000_000_000_000 {
        time.saturating_mul(1000)
    } else {
        time
    }
}

fn timestamp(time: i64) -> Option<String> {
    (time > 0)
        .then_some(time)
        .and_then(|time| DateTime::<Utc>::from_timestamp_millis(millis(time)))
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Millis, true))
}
