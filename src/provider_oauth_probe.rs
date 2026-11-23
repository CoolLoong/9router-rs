use std::time::Duration;

use axum::http::StatusCode;
use reqwest::{Client, Method, Url};
use serde_json::{Value, json};

use crate::chat_output::js_text;
use crate::grok_cli_http::{account_access, identity_headers, refresh_mode};
use crate::grok_cli_models::due;
use crate::provider_connections::truthy;
use crate::provider_http::{self, Bound, http_client};
use crate::provider_imports::metadata_client;
use crate::provider_index::js_trim;
use crate::provider_oauth::{
    RefreshMode, refresh_account, refresh_due_for_provider, test_token_url,
};
use crate::proxy_transport::{Route, Selection};

const TIMEOUT: Duration = Duration::from_secs(15);
const GROK_USER: &str = "https://cli-chat-proxy.grok.com/v1/user";
const GROK_USER_FIXTURE: &str = "http://127.0.0.1:52989/grok-cli/v1/user";
const CODEX: &str = "https://chatgpt.com/backend-api/codex/responses";
const CLINE_USER: &str = "https://api.cline.bot/api/v1/users/me";
const CLOUD_CODE_ASSIST: &str = "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
const ANTIGRAVITY_USER_AGENT: &str = "antigravity/hub/2.24.0 darwin/arm64";
const GROK_LIMIT: &str = "Connected, but Grok Build credits are exhausted (spending limit). Add credits or upgrade SuperGrok.";

pub(super) async fn test(state: &super::ProviderTools, account: &Value) -> Option<Option<String>> {
    let provider = account["provider"].as_str()?;
    let token = &account["accessToken"];
    let gitlab;
    let target = match provider {
        "cursor" | "codebuddy-cn" | "codebuddy-intl" => {
            return Some((!truthy(token)).then(|| "No access token".into()));
        }
        "kimi-coding" => {
            return Some(if !truthy(token) {
                Some("No access token".into())
            } else if refresh_due_for_provider("kimi", account) {
                Some(if truthy(&account["refreshToken"]) {
                    "Token expired and refresh failed".into()
                } else {
                    "Token expired".into()
                })
            } else {
                None
            });
        }
        "kilocode" => "https://api.kilo.ai/api/profile",
        "github" => "https://api.github.com/user",
        "muse" => "https://api.meta.ai/v1/models",
        "gitlab" => {
            let Some(endpoint) = crate::gitlab_transport::user_endpoint(account) else {
                return Some(Some("Invalid GitLab endpoint".into()));
            };
            gitlab = endpoint;
            gitlab.as_str()
        }
        "qoder" => "https://openapi.qoder.sh/api/v1/userinfo",
        "qoder-cn" => "https://openapi.qoder.com.cn/api/v1/userinfo",
        "iflow" => "https://iflow.cn/api/oauth/getUserInfo",
        "kimchi" => "https://api.cast.ai/v1/llm/openai/supported-providers",
        _ => return Some(Some("Provider test not supported".into())),
    };
    if !truthy(token) {
        return Some(Some("No access token".into()));
    }
    let mut endpoint = Url::parse(target).ok()?;
    if provider == "iflow" {
        endpoint
            .query_pairs_mut()
            .append_pair("accessToken", &js_text(token));
    }
    let Some(settings) = state.storage.read_settings().await else {
        return Some(Some("fetch failed".into()));
    };
    let Ok(route) =
        crate::provider_http::select(&state.storage, &settings, account, endpoint.as_str()).await
    else {
        return Some(Some("fetch failed".into()));
    };
    let direct = matches!(
        route,
        Selection::ExplicitBypass | Selection::Routed(Route::Direct)
    );
    let client = metadata_client(&Route::Direct, TIMEOUT)?;
    let public_endpoint = provider != "gitlab" || target == "https://gitlab.com/api/v4/user";
    if direct
        && public_endpoint
        && std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1")
    {
        let mut fixture = if provider == "muse" {
            Url::parse(super::discovery_url(provider)?).ok()?
        } else {
            Url::parse(&format!("http://127.0.0.1:52989/oauth-probes/{provider}")).ok()?
        };
        fixture.set_query(endpoint.query());
        endpoint = fixture;
    }
    let mut request = client.get(endpoint);
    if provider != "iflow" {
        request = request.bearer_auth(js_text(token));
    }
    if provider == "github" {
        request = request
            .header("User-Agent", "9Router")
            .header("Accept", "application/vnd.github+json");
    }
    if provider == "muse" {
        request = request.header("x-api-version", "1.0.0");
    }
    if provider == "kimchi" {
        request = request
            .header("User-Agent", "9Router")
            .header("Accept", "application/json");
    }
    let (client, request) = request.build_split();
    let Ok(request) = request else {
        return Some(Some("fetch failed".into()));
    };
    Some(
        match crate::provider_http::send(&state.proxy_http, client, request, route, TIMEOUT).await {
            Ok(response) if response.status().is_success() => None,
            Ok(response) => Some(match response.status() {
                StatusCode::UNAUTHORIZED => "Token invalid or revoked".into(),
                StatusCode::FORBIDDEN => "Access denied".into(),
                StatusCode::PROXY_AUTHENTICATION_REQUIRED => "Proxy authentication required".into(),
                status => format!("API returned {}", status.as_u16()),
            }),
            Err(error) if error.is_timeout() => {
                Some("The operation was aborted due to timeout".into())
            }
            Err(_) => Some("fetch failed".into()),
        },
    )
}

pub(super) async fn grok(
    state: &super::ProviderTools,
    mut account: Value,
) -> (Value, Option<String>, bool, bool) {
    if account_access(&account).is_none() {
        return (account, Some("No access token".into()), false, false);
    }
    let Ok(direct) = http_client(TIMEOUT).build() else {
        return (account, Some("fetch failed".into()), false, false);
    };
    let Some(delivery) = grok_test_binding(state, &account, &direct).await else {
        return (account, Some("fetch failed".into()), false, false);
    };
    let renewable = account["refreshToken"]
        .as_str()
        .is_some_and(|token| !js_trim(token).is_empty());
    let proactive = renewable && due(&account, 300_000);
    let mut refreshed = false;
    if proactive {
        let Some(current) = refresh_grok_test(state, &account, &direct, &delivery).await else {
            return (
                account,
                Some("Token expired and refresh failed".into()),
                false,
                false,
            );
        };
        refreshed = current["lastRefreshAt"] != account["lastRefreshAt"];
        account = current;
    }
    let mut status = grok_status(&delivery, &account).await;
    if status == Ok(StatusCode::UNAUTHORIZED) && renewable && !proactive {
        let Some(current) = refresh_grok_test(state, &account, &direct, &delivery).await else {
            return (
                account,
                Some("Token invalid or revoked".into()),
                false,
                false,
            );
        };
        refreshed = current["lastRefreshAt"] != account["lastRefreshAt"];
        account = current;
        status = grok_status(&delivery, &account).await;
        if !status
            .as_ref()
            .is_ok_and(|status| status.is_success() || *status == StatusCode::PAYMENT_REQUIRED)
        {
            return (
                account,
                Some("Token invalid or revoked".into()),
                false,
                false,
            );
        }
    }
    let (error, soft) = match status {
        Ok(status) if status.is_success() => (None, false),
        Ok(StatusCode::PAYMENT_REQUIRED) => (Some(GROK_LIMIT.into()), true),
        Ok(StatusCode::UNAUTHORIZED) => (Some("Token invalid or revoked".into()), false),
        Ok(StatusCode::FORBIDDEN) => (Some("Access denied".into()), false),
        Ok(StatusCode::PROXY_AUTHENTICATION_REQUIRED) => {
            (Some("Proxy authentication required".into()), false)
        }
        Ok(status) => (Some(format!("API returned {}", status.as_u16())), false),
        Err(error) => (Some(error.into()), false),
    };
    (account, error, refreshed, soft)
}

async fn grok_test_binding(
    state: &super::ProviderTools,
    account: &Value,
    client: &Client,
) -> Option<Bound> {
    provider_http::bind(
        &state.storage,
        account,
        GROK_USER,
        GROK_USER_FIXTURE,
        client,
        TIMEOUT,
        None,
    )
    .await
}

async fn refresh_grok_test(
    state: &super::ProviderTools,
    account: &Value,
    client: &Client,
    delivery: &Bound,
) -> Option<Value> {
    let current = refresh_account(&state.storage, account, refresh_mode(delivery))
        .await
        .ok()?;
    let route = grok_test_binding(state, &current, client).await?;
    (delivery.same_route(&route) && account_access(&current).is_some()).then_some(current)
}

async fn refresh_test(
    state: &super::ProviderTools,
    account: &Value,
    delivery: &Bound,
    client: &Client,
    addresses: (&str, &str),
) -> Option<Value> {
    let mode = delivery.route.relay_identity().map_or(
        RefreshMode::Routed(delivery.protected),
        RefreshMode::RelayRouted,
    );
    let current = refresh_account(&state.storage, account, mode).await.ok()?;
    let route = provider_http::bind(
        &state.storage,
        &current,
        addresses.0,
        addresses.1,
        client,
        TIMEOUT,
        None,
    )
    .await?;
    (delivery.same_route(&route)
        && test_access(&current, current["provider"].as_str().unwrap_or_default()).is_some())
    .then_some(current)
}

pub(super) async fn official(
    state: &super::ProviderTools,
    mut account: Value,
) -> (Value, Option<String>, bool, bool) {
    let provider = match account["provider"].as_str() {
        Some("claude") => "claude",
        Some("codex") => "codex",
        Some("cline") => "cline",
        Some("antigravity") => "antigravity",
        _ => "gemini-cli",
    };
    if test_access(&account, provider).is_none() {
        return (account, Some("No access token".into()), false, false);
    }
    let renewable = account["refreshToken"]
        .as_str()
        .is_some_and(|token| !js_trim(token).is_empty());
    let due = refresh_due_for_provider(provider, &account);
    if provider == "claude" && (!due || !renewable) {
        return (account, due.then(|| "Token expired".into()), false, false);
    }
    let addresses = match provider {
        "claude" => {
            let endpoint = test_token_url(provider).unwrap_or_default();
            (endpoint, endpoint)
        }
        "codex" => (CODEX, "http://127.0.0.1:52989/oauth-probes/codex"),
        "cline" => (CLINE_USER, "http://127.0.0.1:52989/oauth-probes/cline"),
        "antigravity" => (
            CLOUD_CODE_ASSIST,
            "http://127.0.0.1:52989/oauth-probes/antigravity",
        ),
        _ => (
            CLOUD_CODE_ASSIST,
            "http://127.0.0.1:52989/oauth-probes/gemini-cli",
        ),
    };
    let Ok(client) = http_client(TIMEOUT).build() else {
        return (account, Some("fetch failed".into()), false, false);
    };
    let Some(delivery) = provider_http::bind(
        &state.storage,
        &account,
        addresses.0,
        addresses.1,
        &client,
        TIMEOUT,
        None,
    )
    .await
    else {
        return (account, Some("fetch failed".into()), false, false);
    };
    let mut prior = super::secrets(&account);
    prior.extend(delivery.relay_secret().map(str::to_owned));
    let proactive = renewable && due;
    let mut refreshed = false;
    if proactive {
        let Some(current) = refresh_test(state, &account, &delivery, &client, addresses).await
        else {
            return (
                account,
                Some("Token expired and refresh failed".into()),
                false,
                false,
            );
        };
        refreshed = current["lastRefreshAt"] != account["lastRefreshAt"]
            || current["accessToken"] != account["accessToken"];
        account = current;
    }
    if provider == "claude" {
        return (account, None, refreshed, false);
    }
    let mut response = official_status(&delivery, &account, provider).await;
    if !proactive
        && renewable
        && response
            .as_ref()
            .is_ok_and(|(status, _)| *status == StatusCode::UNAUTHORIZED)
    {
        let Some(current) = refresh_test(state, &account, &delivery, &client, addresses).await
        else {
            return (
                account,
                Some("Token invalid or revoked".into()),
                false,
                false,
            );
        };
        refreshed = current["lastRefreshAt"] != account["lastRefreshAt"]
            || current["accessToken"] != account["accessToken"];
        account = current;
        response = official_status(&delivery, &account, provider).await;
        if provider == "codex" && !response.as_ref().is_ok_and(|(_, error)| error.is_none()) {
            return (
                account,
                Some("Token invalid or revoked".into()),
                false,
                false,
            );
        }
    }
    let error = match response {
        Ok((_, error)) => error.map(|error| super::redact(error, &prior)),
        Err(error) => Some(error.into()),
    };
    (account, error, refreshed, false)
}

fn test_access<'a>(account: &'a Value, provider: &str) -> Option<&'a str> {
    if provider == "cline" {
        account["accessToken"]
            .as_str()
            .map(js_trim)
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= 16 * 1024
                    && token.bytes().all(|byte| byte.is_ascii_graphic())
            })
    } else {
        account_access(account)
    }
}

async fn official_status(
    delivery: &Bound,
    account: &Value,
    provider: &str,
) -> Result<(StatusCode, Option<String>), &'static str> {
    let token = test_access(account, provider).ok_or("No access token")?;
    let request = if provider == "codex" {
        delivery
            .request(Method::POST)
            .bearer_auth(token)
            .header("originator", "codex_cli_rs")
            .header(
                "User-Agent",
                format!("codex_cli_rs/{}", crate::codex_executor::cli_version()),
            )
            .json(&json!({"model":"gpt-5.3-codex","input":[],"stream":false,"store":false}))
    } else if provider == "cline" {
        crate::cline_transport::oauth_headers(delivery.request(Method::GET), Some(token))
            .header("Accept", "application/json")
    } else {
        let agent = if provider == "antigravity" {
            ANTIGRAVITY_USER_AGENT
        } else {
            "google-api-nodejs-client/9.15.1 gemini-cli/0.34.0"
        };
        delivery.request(Method::POST).bearer_auth(token).header("User-Agent", agent)
            .json(&json!({"metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}))
    };
    let mut response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            "The operation was aborted due to timeout"
        } else {
            "fetch failed"
        }
    })?;
    let status = response.status();
    if status.is_success() || provider == "codex" && status == StatusCode::BAD_REQUEST {
        return Ok((status, None));
    }
    let fallback = format!("API returned {}", status.as_u16());
    let message = match (provider, status) {
        (_, StatusCode::PROXY_AUTHENTICATION_REQUIRED) => "Proxy authentication required".into(),
        ("codex" | "cline", StatusCode::UNAUTHORIZED) => "Token invalid or revoked".into(),
        ("codex" | "cline", StatusCode::FORBIDDEN) => "Access denied".into(),
        ("codex" | "cline", _) => fallback,
        _ => {
            let mut bytes = Vec::new();
            while let Ok(Some(chunk)) = response.chunk().await {
                if chunk.len() > 16 * 1024 - bytes.len() {
                    return Ok((status, Some(fallback)));
                }
                bytes.extend_from_slice(&chunk);
            }
            let text = String::from_utf8_lossy(&bytes);
            let parsed = serde_json::from_str::<Value>(&text).ok();
            parsed
                .as_ref()
                .and_then(|body| {
                    [&body["error"]["message"], &body["message"], &body["error"]]
                        .into_iter()
                        .find(|value| truthy(value))
                })
                .map(|value| {
                    value
                        .as_str()
                        .map(js_trim)
                        .filter(|text| !text.is_empty())
                        .map_or_else(|| value.to_string(), str::to_owned)
                })
                .or_else(|| (!js_trim(&text).is_empty()).then(|| js_trim(&text).to_owned()))
                .unwrap_or(fallback)
        }
    };
    Ok((status, Some(message)))
}

async fn grok_status(delivery: &Bound, account: &Value) -> Result<StatusCode, &'static str> {
    let token = account_access(account).ok_or("No access token")?;
    let response = identity_headers(delivery.request(Method::GET), token, None, None)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                "The operation was aborted due to timeout"
            } else {
                "fetch failed"
            }
        })?;
    Ok(response.status())
}
