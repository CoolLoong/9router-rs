use chrono::{TimeDelta, Utc};
use reqwest::{RequestBuilder, StatusCode, header};
use serde_json::{Map, Value, json};

use super::{QuotaDelivery, parse, pinned_field};
use crate::cline_transport;
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;

#[path = "quota_codebuddy.rs"]
mod codebuddy;
pub(super) use codebuddy::{CN_USAGE, INTL_USAGE as CODEBUDDY_INTL_USAGE};
#[path = "quota_commandcode.rs"]
mod commandcode;
#[path = "quota_cursor.rs"]
mod cursor;
#[path = "quota_grok_cli.rs"]
mod grok;
#[path = "quota_key_readers.rs"]
mod keys;
#[path = "quota_kimi.rs"]
mod kimi;
#[path = "quota_kiro.rs"]
mod kiro;
#[path = "quota_ollama_cloud.rs"]
mod ollama;
#[path = "quota_plan_readers.rs"]
mod plans;
#[path = "quota_qoder.rs"]
mod qoder;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    ClinePass,
    CodeBuddyCn,
    CodeBuddyIntl,
    CommandCode,
    Cursor,
    DeepSeek,
    GeminiCli,
    GitHub,
    Glm,
    GlmCn,
    GrokCli,
    Groq,
    IFlow,
    Kimi,
    Kiro,
    MiniMax,
    MiniMaxCn,
    Ollama,
    OpenCodeGo,
    OpenCodeZen,
    Qoder,
    QoderCn,
    Synthetic,
    VercelAiGateway,
    Zed,
}

impl Provider {
    pub(super) fn from_id(id: &str) -> Option<Self> {
        match id {
            "clinepass" => Some(Self::ClinePass),
            "codebuddy-cn" => Some(Self::CodeBuddyCn),
            "codebuddy-intl" => Some(Self::CodeBuddyIntl),
            "commandcode" => Some(Self::CommandCode),
            "cursor" => Some(Self::Cursor),
            "deepseek" => Some(Self::DeepSeek),
            "gemini-cli" => Some(Self::GeminiCli),
            "github" => Some(Self::GitHub),
            "glm" => Some(Self::Glm),
            "glm-cn" => Some(Self::GlmCn),
            "grok-cli" => Some(Self::GrokCli),
            "groq" => Some(Self::Groq),
            "iflow" => Some(Self::IFlow),
            "kimi" => Some(Self::Kimi),
            "kiro" => Some(Self::Kiro),
            "minimax" => Some(Self::MiniMax),
            "minimax-cn" => Some(Self::MiniMaxCn),
            "ollama" => Some(Self::Ollama),
            "opencode-go" => Some(Self::OpenCodeGo),
            "opencode-zen" => Some(Self::OpenCodeZen),
            "qoder" => Some(Self::Qoder),
            "qoder-cn" => Some(Self::QoderCn),
            "synthetic" => Some(Self::Synthetic),
            "vercel-ai-gateway" => Some(Self::VercelAiGateway),
            "zed" => Some(Self::Zed),
            _ => None,
        }
    }

    pub(super) fn supports_credentials(self, account: &Value) -> bool {
        if self == Self::Cursor
            && supplied(&account["accessToken"]).is_some()
            && crate::cursor_identity::Account::from_connection(account).is_none()
        {
            return false;
        }
        if self == Self::Kimi
            && account["authType"] == "oauth"
            && supplied(&account["apiKey"]).is_none()
            && crate::provider_oauth::kimi_portal::saved_device_id(account).is_none()
        {
            return false;
        }
        let fields: &[&str] = match self {
            Self::ClinePass
            | Self::CodeBuddyCn
            | Self::CodeBuddyIntl
            | Self::Glm
            | Self::GlmCn
            | Self::Kimi
            | Self::Kiro
            | Self::Qoder
            | Self::QoderCn => &["apiKey", "accessToken"],
            Self::CommandCode
            | Self::DeepSeek
            | Self::Groq
            | Self::MiniMax
            | Self::MiniMaxCn
            | Self::Ollama
            | Self::OpenCodeGo
            | Self::OpenCodeZen
            | Self::Synthetic
            | Self::VercelAiGateway => &["apiKey"],
            Self::Cursor
            | Self::GitHub
            | Self::GeminiCli
            | Self::GrokCli
            | Self::IFlow
            | Self::Zed => &["accessToken"],
        };
        fields.iter().all(|key| match &account[*key] {
            Value::Null => true,
            Value::String(value) => header::HeaderValue::from_str(js_trim(value)).is_ok(),
            _ => false,
        }) && (account["authType"] != "oauth"
            || account["refreshToken"].is_null()
            || account["refreshToken"].is_string())
    }

    pub(super) fn github_needs_refresh(account: &Value) -> bool {
        let details = &account["providerSpecificData"];
        let expires = account
            .get("expiresAt")
            .filter(|value| truthy(value))
            .unwrap_or(&account["tokenExpiresAt"]);
        let lead = Utc::now() + TimeDelta::minutes(5);
        !truthy(&details["copilotToken"])
            || [&details["copilotTokenExpiresAt"], expires]
                .into_iter()
                .any(|value| parse::reset_time(value, false).is_some_and(|time| time < lead))
    }
}

pub(super) struct Endpoints {
    pub(super) clinepass: String,
    pub(super) clinepass_refresh: String,
    pub(super) github: String,
    pub(super) github_refresh: String,
    pub(super) copilot_refresh: String,
    pub(super) gemini_quota: String,
    pub(super) gemini_assist: String,
    pub(super) zed_user: String,
    pub(super) deepseek_balance: String,
    pub(super) synthetic_quota: String,
    pub(super) kiro: kiro::Endpoints,
    pub(super) commandcode: commandcode::Endpoints,
    pub(super) cursor: cursor::Endpoints,
    pub(super) opencode_go: String,
    pub(super) opencode_zen: String,
    pub(super) groq_models: String,
    pub(super) vercel_credits: String,
    pub(super) glm_quota: [String; 2],
    pub(super) minimax_usage: [[String; 2]; 2],
    pub(super) qoder: [qoder::Endpoints; 2],
    pub(super) ollama: ollama::Endpoints,
    pub(super) kimi_usage: String,
    github_api_version: String,
    github_agent: String,
}

impl Endpoints {
    pub(super) fn pinned() -> Option<Self> {
        let clinepass = include_str!("../../../open-sse/providers/registry/clinepass.js");
        let github = include_str!("../../../open-sse/providers/registry/github.js");
        let github_oauth = github.split_once("\n  oauth: {")?.1;
        let gemini = include_str!("../../../open-sse/providers/registry/gemini-cli.js")
            .split_once("\n    usage: {")?
            .1;
        Some(Self {
            clinepass: pinned_field(clinepass.split_once("\n    usage: {")?.1, "url:")?,
            clinepass_refresh: pinned_field(
                clinepass.split_once("\n  oauth: {")?.1,
                "refreshUrl:",
            )?,
            github: pinned_field(github.split_once("\n    usage: {")?.1, "url:")?,
            github_refresh: pinned_field(github_oauth, "tokenUrl:")?,
            copilot_refresh: pinned_field(github_oauth, "copilotTokenUrl:")?,
            github_api_version: pinned_field(github_oauth, "apiVersion:")?,
            github_agent: pinned_field(github_oauth, "userAgent:")?,
            gemini_quota: pinned_field(gemini, "quotaUrl:")?,
            gemini_assist: pinned_field(gemini, "loadCodeAssistUrl:")?,
            zed_user: "https://cloud.zed.dev/client/users/me".into(),
            deepseek_balance: "https://api.deepseek.com/user/balance".into(),
            synthetic_quota: "https://api.synthetic.new/v2/quotas".into(),
            groq_models: "https://api.groq.com/openai/v1/models".into(),
            vercel_credits: "https://ai-gateway.vercel.sh/v1/credits".into(),
            qoder: ["https://openapi.qoder.sh", "https://openapi.qoder.com.cn"]
                .map(qoder::Endpoints::new),
            ollama: ollama::Endpoints::pinned(),
            kimi_usage: "https://api.kimi.com/coding/v1/usages".into(),
            glm_quota: [
                "https://api.z.ai/api/monitor/usage/quota/limit",
                "https://open.bigmodel.cn/api/monitor/usage/quota/limit",
            ]
            .map(str::to_owned),
            minimax_usage: [
                [
                    "https://www.minimax.io/v1/token_plan/remains",
                    "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
                ],
                [
                    "https://www.minimaxi.com/v1/api/openplatform/coding_plan/remains",
                    "https://api.minimaxi.com/v1/api/openplatform/coding_plan/remains",
                ],
            ]
            .map(|urls| urls.map(str::to_owned)),
            kiro: kiro::Endpoints::pinned()?,
            commandcode: commandcode::Endpoints::configured(),
            cursor: cursor::Endpoints::pinned(),
            opencode_go: pinned_field(
                include_str!("../../../open-sse/providers/registry/opencode-go.js")
                    .split_once("\n    usage: {")?
                    .1,
                "url:",
            )?,
            opencode_zen: pinned_field(
                include_str!("../../../open-sse/providers/registry/opencode-zen.js")
                    .split_once("\n    usage: {")?
                    .1,
                "url:",
            )?,
        })
    }

    pub(super) fn targets(&self, provider: Provider, oauth: bool) -> [Option<&str>; 3] {
        match provider {
            Provider::ClinePass => [
                Some(&self.clinepass),
                oauth.then_some(self.clinepass_refresh.as_str()),
                None,
            ],
            Provider::CodeBuddyCn => [Some(codebuddy::CN_USAGE), None, None],
            Provider::CodeBuddyIntl => [
                Some(CODEBUDDY_INTL_USAGE),
                oauth.then_some(crate::codebuddy_device::INTL_REFRESH_URL),
                None,
            ],
            Provider::GitHub => [
                Some(&self.github),
                Some(&self.github_refresh),
                Some(&self.copilot_refresh),
            ],
            Provider::GeminiCli => [
                Some(&self.gemini_quota),
                Some(&self.gemini_assist),
                Some("https://oauth2.googleapis.com/token"),
            ],
            Provider::Zed => [Some(&self.zed_user), None, None],
            Provider::DeepSeek => [Some(&self.deepseek_balance), None, None],
            Provider::Synthetic => [Some(&self.synthetic_quota), None, None],
            Provider::GrokCli => [Some(grok::BILLING), Some(grok::ENDPOINT), None],
            Provider::Groq => [Some(&self.groq_models), None, None],
            Provider::Glm | Provider::GlmCn => [
                Some(&self.glm_quota[usize::from(provider == Provider::GlmCn)]),
                None,
                None,
            ],
            Provider::MiniMax | Provider::MiniMaxCn => {
                let [primary, fallback] =
                    &self.minimax_usage[usize::from(provider == Provider::MiniMaxCn)];
                [Some(primary), Some(fallback), None]
            }
            Provider::VercelAiGateway => [Some(&self.vercel_credits), None, None],
            Provider::IFlow => [Some("https://iflow.cn/oauth/token"), None, None],
            Provider::Kimi => [
                Some(&self.kimi_usage),
                oauth.then_some("https://auth.kimi.com/api/oauth/token"),
                None,
            ],
            Provider::Ollama => [
                Some(&self.ollama.usage),
                Some(&self.ollama.identity),
                Some(&self.ollama.balance),
            ],
            Provider::Kiro => [
                Some(&self.kiro.codewhisperer),
                Some(&self.kiro.amazon_q),
                None,
            ],
            Provider::CommandCode => self.commandcode.targets(),
            Provider::Cursor => [
                Some(&self.cursor.period),
                Some(&self.cursor.plan),
                Some(&self.cursor.legacy),
            ],
            Provider::OpenCodeGo => [Some(&self.opencode_go), None, None],
            Provider::OpenCodeZen => [Some(&self.opencode_zen), None, None],
            Provider::Qoder | Provider::QoderCn => {
                let qoder = &self.qoder[usize::from(provider == Provider::QoderCn)];
                [
                    Some(&qoder.usage),
                    Some(&qoder.exchange),
                    Some(&qoder.identity),
                ]
            }
        }
    }

    pub(super) async fn usage(
        &self,
        read: &QuotaDelivery,
        provider: Provider,
        account: &Value,
    ) -> Result<Value, String> {
        let client = &read.client;
        match provider {
            Provider::ClinePass => Ok(clinepass(client.get(&self.clinepass), account, read).await),
            Provider::CodeBuddyCn | Provider::CodeBuddyIntl => {
                Ok(codebuddy::usage(read, account, provider == Provider::CodeBuddyIntl).await)
            }
            Provider::GeminiCli => Ok(self.gemini(read, account).await),
            Provider::GitHub => self.github(read, account).await,
            Provider::GrokCli => Ok(grok::usage(read, account).await),
            Provider::Groq => Ok(keys::groq(client.get(&self.groq_models), account, read).await),
            Provider::Glm | Provider::GlmCn => {
                let url = &self.glm_quota[usize::from(provider == Provider::GlmCn)];
                Ok(plans::glm(client.get(url), account, read).await)
            }
            Provider::MiniMax | Provider::MiniMaxCn => {
                let urls = &self.minimax_usage[usize::from(provider == Provider::MiniMaxCn)];
                Ok(plans::minimax(read, urls, account).await)
            }
            Provider::VercelAiGateway => {
                Ok(keys::vercel(client.get(&self.vercel_credits), account, read).await)
            }
            Provider::IFlow => Ok(json!({"message":"iFlow connected. Usage tracked per request."})),
            Provider::Kimi => Ok(kimi::usage(read, &self.kimi_usage, account).await),
            Provider::Ollama => Ok(self.ollama.read(read, account).await),
            Provider::Qoder | Provider::QoderCn => Ok(self.qoder
                [usize::from(provider == Provider::QoderCn)]
            .usage(read, account)
            .await),
            Provider::Zed => Ok(self.zed(read, account).await),
            Provider::Kiro => Ok(self.kiro.usage(read, account).await),
            Provider::CommandCode => Ok(self.commandcode.usage(read, account).await),
            Provider::Cursor => Ok(self.cursor.read(read, account).await),
            Provider::OpenCodeGo => Ok(opencode_usage(
                client.get(&self.opencode_go),
                account,
                "OpenCode Go",
                "subscription",
                read,
            )
            .await),
            Provider::OpenCodeZen => Ok(opencode_usage(
                client.get(&self.opencode_zen),
                account,
                "OpenCode Zen",
                "billing",
                read,
            )
            .await),
            Provider::DeepSeek => {
                Ok(
                    api_key_quota(client.get(&self.deepseek_balance), provider, account, read)
                        .await,
                )
            }
            Provider::Synthetic => {
                Ok(api_key_quota(client.get(&self.synthetic_quota), provider, account, read).await)
            }
        }
    }

    async fn zed(&self, read: &QuotaDelivery, account: &Value) -> Value {
        let Some(token) = account["accessToken"]
            .as_str()
            .map(js_trim)
            .filter(|token| !token.is_empty())
        else {
            return json!({"message":"Zed access token not available. Re-connect Zed to view quota."});
        };
        let details = &account["providerSpecificData"];
        if !truthy(&details["userId"]) {
            return json!({"message":"Zed credential is missing user id. Re-connect Zed to view quota."});
        }
        let fail = "Zed error: Failed to fetch quota";
        let Ok(mut authorization) = header::HeaderValue::from_str(&format!(
            "{} {token}",
            parse::string(&details["userId"])
        )) else {
            return json!({"message":fail});
        };
        authorization.set_sensitive(true);
        let mut request = read
            .client
            .get(&self.zed_user)
            .header(header::AUTHORIZATION, authorization)
            .header(header::ACCEPT, "application/json");
        if truthy(&details["systemId"]) {
            request = request.header("x-zed-system-id", parse::string(&details["systemId"]));
        }
        let Ok(response) = read.send(request).await else {
            return json!({"message":fail});
        };
        let status = response.status();
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return json!({"message":"Zed authentication failed. Sign in again from the dashboard or Zed editor."});
        }
        let raw = read.redact_relay(response.text().await.unwrap_or_default());
        let response: Value = serde_json::from_str(&raw).unwrap_or_default();
        if !status.is_success() {
            let detail = [
                &response["message"],
                &response["error"]["message"],
                &response["error"],
            ]
            .into_iter()
            .find_map(|value| value.as_str().filter(|value| !value.is_empty()))
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if raw.is_empty() {
                    format!("HTTP {}", status.as_u16())
                } else {
                    raw
                }
            });
            let detail = redact(detail, account);
            return json!({"message":format!("Zed error: {detail}")});
        }
        let mut usage = zed_usage(&response);
        for field in ["plan", "planId"] {
            if let Some(value) = usage[field].as_str() {
                usage[field] = json!(redact(value.to_owned(), account));
            }
        }
        usage
    }

    async fn gemini(&self, read: &QuotaDelivery, account: &Value) -> Value {
        let Some(token) = account["accessToken"]
            .as_str()
            .filter(|token| !token.is_empty())
        else {
            return json!({"plan":"Free","message":"Gemini CLI access token not available."});
        };
        let request = |url: &str, body: Value| {
            read.client
                .post(url)
                .bearer_auth(token)
                .json(&body)
                .timeout(std::time::Duration::from_secs(10))
        };
        let mut plan = "Free".to_owned();
        let mut project = crate::google_code_assist::saved_project(account).map(str::to_owned);
        if project.is_none()
            && let Ok(response) = read.send(request(
                &self.gemini_assist,
                json!({
                    "metadata":{"ideType":9,"platform":crate::google_code_assist::platform(),"pluginType":2},
                }),
            ))
            .await
            && response.status().is_success()
            && let Ok(info) = response.json::<Value>().await
        {
            project = crate::google_code_assist::project_id(&info["cloudaicompanionProject"])
                .map(str::to_owned);
            if let Some(tier) = info["currentTier"]["name"]
                .as_str()
                .filter(|tier| !tier.is_empty())
            {
                plan = tier.replace(token, "[redacted]");
            }
        }
        let Some(project) = project else {
            return json!({"plan":plan,"message":"Gemini CLI project ID not available. Reconnect Gemini CLI, or configure a Google Cloud project with Gemini Code Assist access before checking quota."});
        };
        let Ok(response) = read
            .send(request(&self.gemini_quota, json!({"project":project})))
            .await
        else {
            return json!({"message":"Gemini CLI error: fetch failed"});
        };
        if !response.status().is_success() {
            return json!({"plan":plan,"message":format!("Gemini CLI quota error ({}).", response.status().as_u16())});
        }
        let Ok(data) = response.json::<Value>().await else {
            return json!({"message":"Gemini CLI error: invalid provider response"});
        };
        let quotas: Map<String, Value> = data["buckets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|bucket| {
                let model = bucket["modelId"]
                    .as_str()
                    .filter(|model| !model.is_empty() && !model.contains(token))?;
                let raw = &bucket["remainingFraction"];
                if raw.is_null() {
                    return None;
                }
                let fraction = if raw == true { 1.0 } else { parse::finite(raw) };
                let remaining = (1000.0 * fraction + 0.5).floor();
                Some((
                    model.to_owned(),
                    json!({
                        "used":parse::number((1000.0 - remaining).max(0.0)),
                        "total":1000,
                        "resetAt":parse::reset_date(&bucket["resetTime"], true),
                        "remainingPercentage":parse::number(fraction * 100.0),
                        "unlimited":false,
                    }),
                ))
            })
            .collect();
        json!({"plan":plan,"quotas":quotas})
    }

    async fn github(&self, read: &QuotaDelivery, account: &Value) -> Result<Value, String> {
        let fail = |reason: &str| format!("Failed to fetch GitHub usage: {reason}");
        let token = account["accessToken"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                fail("No GitHub access token available. Please re-authorize the connection.")
            })?;
        let mut authorization = header::HeaderValue::from_str(&format!("token {token}"))
            .map_err(|_| fail("fetch failed"))?;
        authorization.set_sensitive(true);
        let request = read
            .client
            .get(&self.github)
            .header(header::AUTHORIZATION, authorization)
            .header(header::ACCEPT, "application/json")
            .header("X-GitHub-Api-Version", &self.github_api_version)
            .header(header::USER_AGENT, &self.github_agent)
            .header("Editor-Version", "vscode/1.100.0")
            .header("Editor-Plugin-Version", "copilot-chat/0.26.7");
        let response = read.send(request).await.map_err(|_| fail("fetch failed"))?;
        if !response.status().is_success() {
            let reason = response.text().await.map_err(|_| fail("fetch failed"))?;
            return Err(fail(&format!(
                "GitHub API error: {}",
                redact(read.redact_relay(reason), account)
            )));
        }
        let data = response
            .json::<Value>()
            .await
            .map_err(|_| fail("invalid provider response"))?;
        if data.is_null() {
            return Err(fail(
                "Cannot read properties of null (reading 'quota_snapshots')",
            ));
        }
        Ok(github_usage(&data))
    }
}

async fn opencode_usage(
    request: RequestBuilder,
    account: &Value,
    plan: &str,
    entitlement: &str,
    read: &QuotaDelivery,
) -> Value {
    let Some(key) = account["apiKey"]
        .as_str()
        .map(js_trim)
        .filter(|key| !key.is_empty())
    else {
        return json!({"message":format!("{plan} API key not available. Add a key to view usage.")});
    };
    let request = request
        .bearer_auth(key)
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(10));
    let Ok(response) = read.send(request).await else {
        return json!({"message":format!("{plan} error: fetch failed")});
    };
    let code = response.status();
    let answer = |message: String| json!({"plan":plan,"message":message});
    if code == StatusCode::UNAUTHORIZED {
        return answer(format!("{plan} authentication failed. Check the API key."));
    }
    if code == StatusCode::FORBIDDEN {
        let response = response.json::<Value>().await.unwrap_or_default();
        let detail = if response["error"]["type"] == "EntitlementError" {
            format!("{entitlement} required")
        } else {
            "access forbidden".to_owned()
        };
        return answer(format!("{plan} {detail} for this API key."));
    }
    if !code.is_success() {
        return answer(format!("{plan} usage API error ({}).", code.as_u16()));
    }
    let data = response.json::<Value>().await.unwrap_or_default();
    let usage = &data["usage"];
    if !usage.is_object() && !usage.is_array() {
        return answer(format!("{plan} usage response did not contain quota data."));
    }
    let quotas: Map<String,Value> = [("rolling","Rolling"),("weekly","Weekly"),("monthly","Monthly")]
        .into_iter().filter_map(|(period,name)| {
            let row = &usage[period];
            let used = parse::finite_number(&row["percent"])?.clamp(0.0,100.0);
            let remaining = parse::number(100.0-used);
            Some((name.into(),json!({"used":parse::number(used),"total":100,
                "remaining":remaining,"remainingPercentage":remaining,"resetAt":parse::reset_date(&row["resetsAt"],true),
                "unlimited":false})))
        }).collect();
    if quotas.is_empty() {
        return answer(format!(
            "{plan} usage response did not contain valid quota data."
        ));
    }
    json!({"plan":plan,"quotas":quotas})
}

async fn api_key_quota(
    request: RequestBuilder,
    provider: Provider,
    account: &Value,
    read: &QuotaDelivery,
) -> Value {
    let deepseek = provider == Provider::DeepSeek;
    let label = if deepseek { "DeepSeek" } else { "Synthetic" };
    let Some(key) = account["apiKey"]
        .as_str()
        .map(js_trim)
        .filter(|key| !key.is_empty())
    else {
        return json!({"message":format!("{label} API key not available. Add a key to view usage.")});
    };
    let mut request = request
        .bearer_auth(key)
        .header(header::ACCEPT, "application/json")
        .timeout(std::time::Duration::from_secs(10));
    if deepseek {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let Ok(response) = read.send(request).await else {
        return if deepseek {
            json!({"message":"DeepSeek error: fetch failed"})
        } else {
            json!({"plan":"Synthetic","message":"Synthetic error: fetch failed"})
        };
    };
    let status = response.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return json!({"plan":label,"message":format!("{label} authentication failed. Check the API key.")});
    }
    if !status.is_success() {
        let status = status.as_u16();
        if !deepseek {
            return json!({"plan":label,"message":format!("Synthetic usage API error ({status})")});
        }
        let detail = response.text().await.unwrap_or_default();
        let detail: String = redact(read.redact_relay(detail), account)
            .chars()
            .take(120)
            .collect();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        return json!({"plan":label,"message":format!("DeepSeek balance API error ({status}){suffix}")});
    }
    let data = response.json::<Value>().await.unwrap_or_default();
    if deepseek {
        deepseek_balance(&data, key)
    } else {
        synthetic_usage(&data)
    }
}

fn deepseek_balance(data: &Value, key: &str) -> Value {
    if !data.is_object() && !data.is_array() {
        return json!({"message":"DeepSeek balance response was not JSON."});
    }
    let quotas: Map<String, Value> = data["balance_infos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|balance| {
            let currency = balance["currency"]
                .as_str()
                .filter(|value| !value.is_empty() && !value.contains(key))?
                .to_uppercase();
            let value = balance
                .get("total_balance")
                .filter(|value| !value.is_null())
                .unwrap_or(&balance["totalBalance"]);
            let balance = parse::finite(value).max(0.0);
            Some((
                format!("Balance ({currency})"),
                json!({"used":0,"total":parse::number(balance),
            "remainingPercentage":if balance>0.0 {100} else {0},"resetAt":null,"unlimited":false,
            "isCreditBalance":true,"currency":currency}),
            ))
        })
        .collect();
    if quotas.is_empty() {
        return json!({"plan":"DeepSeek","message":"DeepSeek connected. No balance data returned."});
    }
    let plan = if data["is_available"] == true || data["isAvailable"] == true {
        "DeepSeek"
    } else {
        "DeepSeek (Insufficient Balance)"
    };
    json!({"plan":plan,"quotas":quotas})
}

fn synthetic_usage(data: &Value) -> Value {
    let subscription = &data["subscription"];
    if !subscription.is_object() && !subscription.is_array() {
        return json!({"plan":"Synthetic","message":"Synthetic returned no subscription quota."});
    }
    let total = parse::finite(&subscription["limit"]);
    if total <= 0.0 {
        return json!({"plan":"Synthetic","message":"No Synthetic subscription quota found for this key."});
    }
    let used = parse::finite(&subscription["requests"]);
    let remaining = (total - used).max(0.0);
    let percent = |value: f64| parse::number(((value / total) * 100.0 + 0.5).floor());
    json!({"plan":"Synthetic","quotas":{"Requests":{
        "name":"Subscription requests","used":parse::number(used),"total":parse::number(total),
        "percentage":percent(used),"colorPercentage":percent(remaining),"percentageIsUsed":true,
        "remaining":parse::number(remaining),"unlimited":false,"resetAt":parse::reset_date(&subscription["renewsAt"],true),
    }}})
}

fn zed_usage(user: &Value) -> Value {
    let plan = &user["plan"];
    let plan_id = [
        &plan["plan_v3"],
        &plan["plan_v2"],
        &plan["plan"],
        &user["plan_v3"],
    ]
    .into_iter()
    .find(|value| truthy(value));
    let reset = [
        &plan["subscription_period"]["ended_at"],
        &plan["subscriptionPeriod"]["endedAt"],
    ]
    .into_iter()
    .map(|value| parse::reset_date(value, true))
    .find(|value| !value.is_null())
    .unwrap_or_default();
    let trial = truthy(&plan["trial_started_at"]) || truthy(&plan["trialStartedAt"]);
    let overdue = truthy(&plan["has_overdue_invoices"]) || truthy(&plan["hasOverdueInvoices"]);
    let mut label = plan_id
        .map(parse::string)
        .map(|id| zed_plan_label(&id))
        .unwrap_or_else(|| "Zed".into());
    if trial && !label.to_ascii_lowercase().contains("trial") {
        label.push_str(" (Trial active)");
    }
    let usage = &plan["usage"];
    let mut quotas = Map::new();
    let mut token_billing = false;
    for (name, bucket_names) in [
        ("Edit Predictions", ["edit_predictions", "editPredictions"]),
        ("Hosted Model Requests", ["model_requests", "modelRequests"]),
    ] {
        let Some(bucket) = bucket_names
            .into_iter()
            .map(|key| &usage[key])
            .find(|bucket| truthy(bucket))
        else {
            continue;
        };
        let (unlimited, total) = zed_limit(&bucket["limit"]);
        if name == "Hosted Model Requests" && !unlimited && total <= 0.0 {
            token_billing = true;
            continue;
        }
        let used = if bucket["used"] == true {
            1.0
        } else {
            parse::finite(&bucket["used"]).max(0.0)
        };
        let used = if total > 0.0 { used.min(total) } else { used };
        let remaining = if unlimited {
            100.0
        } else if total > 0.0 {
            (total - used) / total * 100.0
        } else {
            0.0
        };
        quotas.insert(
            name.into(),
            json!({"used":parse::number(used),"total":parse::number(total),
            "remainingPercentage":parse::number(remaining),"resetAt":reset,"unlimited":unlimited}),
        );
    }
    let message = if overdue {
        Some(
            "This Zed account has overdue invoices. Usage may be blocked until billing is resolved.",
        )
    } else if token_billing {
        Some(
            "Hosted AI models are billed per token (not request count). Edit Predictions are tracked below. Token spend is on dashboard.zed.dev.",
        )
    } else {
        None
    };
    json!({"plan":label,"quotas":quotas,"message":message,
        "hasOverdueInvoices":overdue,"trialStarted":trial,"planId":plan_id,"resetAt":reset})
}

fn zed_limit(value: &Value) -> (bool, f64) {
    if value
        .as_str()
        .is_some_and(|limit| limit.trim() == "unlimited")
        || value["unlimited"] == true
    {
        return (true, 0.0);
    }
    let total = match value {
        Value::String(_) | Value::Number(_) => parse::finite(value),
        Value::Object(_) => value
            .get("limited")
            .filter(|field| !field.is_null())
            .unwrap_or(&value["Limited"])
            .as_f64()
            .unwrap_or(0.0),
        _ => 0.0,
    };
    (false, total.max(0.0))
}

fn zed_plan_label(plan: &str) -> String {
    match plan.trim().to_ascii_lowercase().as_str() {
        "" => "Zed".into(),
        "zed_free" => "Zed Free".into(),
        "zed_pro" => "Zed Pro".into(),
        "zed_pro_trial" => "Zed Pro Trial".into(),
        "zed_student" => "Zed Student".into(),
        "zed_business" => "Zed Business".into(),
        _ => plan
            .trim()
            .replace('_', " ")
            .split_whitespace()
            .map(|word| {
                let mut chars = word.chars();
                let first = chars.next().expect("nonempty word");
                first
                    .to_uppercase()
                    .chain(chars.flat_map(char::to_lowercase))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

async fn clinepass(request: RequestBuilder, account: &Value, read: &QuotaDelivery) -> Value {
    let request = request.header(header::ACCEPT, "application/json");
    let request = if let Some(key) = supplied(&account["apiKey"]) {
        request.bearer_auth(key)
    } else if let Some(token) = supplied(&account["accessToken"]) {
        cline_transport::oauth_headers(request, Some(token))
    } else {
        return json!({"message": "ClinePass access token or API key not available."});
    };
    let Ok(response) = read.send(request).await else {
        return json!({"message":"ClinePass usage error: fetch failed"});
    };
    let status = response.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return json!({
            "plan":"ClinePass","message":"ClinePass authentication expired. Please re-authorize.",
        });
    }
    if !status.is_success() {
        let err = redact(
            read.redact_relay(response.text().await.unwrap_or_default()),
            account,
        );
        let mut remaining = 120;
        let reason: String = err
            .chars()
            .take_while(|ch| {
                if ch.len_utf16() <= remaining {
                    remaining -= ch.len_utf16();
                    true
                } else {
                    false
                }
            })
            .collect();
        let details = if reason.is_empty() {
            String::new()
        } else {
            format!(": {reason}")
        };
        return json!({
            "plan":"ClinePass","message":format!("ClinePass usage API error ({}){details}", status.as_u16()),
        });
    }
    match response.json::<Value>().await {
        Ok(value) if value.is_object() || value.is_array() => clinepass_usage(&value),
        _ => json!({"message":"ClinePass usage response was not JSON."}),
    }
}

pub(super) fn supplied(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(js_trim)
        .filter(|value| !value.is_empty())
}

fn clinepass_usage(data: &Value) -> Value {
    let limits = data["data"]["limits"].as_array();
    let mut quotas = Map::new();
    for (kind, name) in [
        ("five_hour", "Session (5h)"),
        ("weekly", "Weekly (7d)"),
        ("monthly", "Monthly"),
    ] {
        let item = limits.and_then(|items| items.iter().rev().find(|item| item["type"] == kind));
        if let Some(item) = item {
            let used = parse::finite(&item["percentUsed"]).clamp(0.0, 100.0);
            quotas.insert(name.into(), json!({
                "used":parse::number(used),"total":100,"remainingPercentage":parse::number(100.0-used),
                "resetAt":parse::reset_date(&item["resetsAt"],true),
            }));
        }
    }
    if quotas.is_empty() {
        return json!({
            "plan":"ClinePass","message":"ClinePass connected. No usage limits reported for this plan.","quotas":{},
        });
    }
    json!({"plan":"ClinePass","quotas":quotas})
}

fn github_usage(data: &Value) -> Value {
    let paid = truthy(&data["quota_snapshots"]);
    if !paid && !truthy(&data["monthly_quotas"]) && !truthy(&data["limited_user_quotas"]) {
        return json!({"message":"GitHub Copilot connected. Unable to parse quota data."});
    }
    let plan = if paid || truthy(&data["copilot_plan"]) {
        data.get("copilot_plan")
    } else {
        data.get("access_type_sku")
    };
    let reset = data.get(if paid {
        "quota_reset_date"
    } else {
        "limited_user_reset_date"
    });
    let reset_at = parse::reset_date(reset.unwrap_or(&Value::Null), true);
    let quotas = if paid {
        let snapshots = &data["quota_snapshots"];
        ["chat", "completions", "premium_interactions"]
            .into_iter()
            .map(|kind| (kind.into(), github_paid(&snapshots[kind], &reset_at)))
            .collect()
    } else {
        ["chat", "completions"]
            .into_iter()
            .map(|kind| {
                (
                    kind.into(),
                    json!({
                        "used":parse::value_or(&data["limited_user_quotas"][kind],json!(0)),
                        "total":parse::value_or(&data["monthly_quotas"][kind],json!(0)),
                        "unlimited":false,"resetAt":reset_at,
                    }),
                )
            })
            .collect::<Map<_, _>>()
    };
    let mut output = Map::new();
    if let Some(plan) = plan {
        output.insert("plan".into(), plan.clone());
    }
    if let Some(reset) = reset {
        output.insert("resetDate".into(), reset.clone());
    }
    output.insert("quotas".into(), Value::Object(quotas));
    Value::Object(output)
}

fn github_paid(quota: &Value, reset_at: &Value) -> Value {
    if !truthy(quota) {
        return json!({"used":0,"total":0,"unlimited":true,"resetAt":reset_at});
    }
    let total = quota.get("entitlement");
    let remaining = quota.get("remaining");
    let used = total
        .and_then(coerce_quantity)
        .zip(remaining.and_then(coerce_quantity))
        .map_or(Value::Null, |(total, remaining)| {
            parse::number(total - remaining)
        });
    let mut value = Map::from_iter([
        ("used".into(), used),
        (
            "unlimited".into(),
            parse::value_or(&quota["unlimited"], json!(false)),
        ),
        ("resetAt".into(), reset_at.clone()),
    ]);
    if let Some(total) = total {
        value.insert("total".into(), total.clone());
    }
    if let Some(remaining) = remaining {
        value.insert("remaining".into(), remaining.clone());
    }
    Value::Object(value)
}

fn coerce_quantity(value: &Value) -> Option<f64> {
    match value {
        Value::Null | Value::Bool(false) => Some(0.0),
        Value::Bool(true) => Some(1.0),
        Value::String(text) if js_trim(text).is_empty() => Some(0.0),
        _ => parse::finite_number(value),
    }
}

fn redact(mut reason: String, account: &Value) -> String {
    let details = &account["providerSpecificData"];
    let mut credentials: Vec<&str> = [
        &account["apiKey"],
        &account["accessToken"],
        &account["refreshToken"],
        &account["idToken"],
        &details["copilotToken"],
    ]
    .into_iter()
    .filter_map(Value::as_str)
    .map(js_trim)
    .filter(|value| !value.is_empty())
    .collect();
    if let Some(token) = supplied(&account["accessToken"])
        && token
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("workos:"))
        && let Some(token) = token.get(7..).filter(|token| !token.is_empty())
    {
        credentials.push(token);
    }
    credentials.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for credential in credentials {
        reason = reason.replace(credential, "[redacted]");
    }
    reason
}
