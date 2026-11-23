use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{StreamExt, stream};
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde_json::{Value, json};
use url::Url;

use crate::chat::GithubRoute;
use crate::chat_format::ChatFormat;
use crate::chat_input::{PreparedChat, apply_schema_fallback};
use crate::codex_executor;
use crate::provider_connections::{self as connections, read_all, truthy};
use crate::proxy_transport::{self, Route};
use crate::static_chat::{self, Endpoint};
use crate::vertex_transport;
use crate::{github_claude, github_executor, github_responses};

use super::{Input, OPENAI_BASE, Translator, bad, failure, gemini, reply};

const DEFAULT_ANTHROPIC_BASE: &str = "https://api.anthropic.com/v1";
const STANDARD_BETA: &str = "claude-code-20250219,interleaved-thinking-2025-05-14";

#[derive(Clone)]
pub(super) struct Clients {
    pooled: reqwest::Client,
    fresh: reqwest::Client,
    proxy: proxy_transport::Clients,
}

impl Clients {
    pub fn new() -> Self {
        let builder = || {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(crate::upstream::redirects())
        };
        Self {
            pooled: builder().build().expect("translator HTTP client"),
            fresh: builder()
                .pool_max_idle_per_host(0)
                .build()
                .expect("translator reconnect client"),
            proxy: proxy_transport::Clients::default(),
        }
    }
}

enum Transport {
    OpenAi,
    Anthropic { compatible: bool, official: bool },
    Gemini,
    GeminiCli,
    Azure,
    Gitlab,
    IFlow,
    KiloCode,
    Ollama,
    OpenCode(crate::opencode_chat::Endpoint),
    CommandCode,
    Codex(codex_executor::Exchange),
    Github(GithubRoute),
    Static(Endpoint),
}

struct Plan {
    destination: String,
    transport: Transport,
    stream: bool,
    body: Value,
    timeout: Duration,
}

pub(super) async fn route(State(state): State<Translator>, request: Request) -> Response {
    let api_key = crate::catalog_guard::api_key(&request);
    let Some(input) = Input::read(request).await else {
        return failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read translator request",
        );
    };
    let Some(value) = input.json().filter(|value| !value.is_null()) else {
        return bad("Invalid JSON body");
    };
    let (provider, model, body) = (&value["provider"], &value["model"], &value["body"]);
    if !truthy(provider) || !truthy(model) || !truthy(body) {
        return bad("provider, model, and body required");
    }
    let (Some(provider), Some(model)) = (provider.as_str(), model.as_str()) else {
        return bad("Invalid provider or model");
    };
    if matches!(provider, "mimo-free" | "mmf") {
        return failure(StatusCode::GONE, crate::chat::MIMO_FREE_RETIRED);
    }
    if crate::opencode_chat::requires_official_client(provider, model) {
        return failure(
            StatusCode::FORBIDDEN,
            crate::opencode_chat::FREE_CLIENT_REQUIRED,
        );
    }
    let database_provider = provider.to_owned();
    let selected = state
        .storage
        .run(false, move |db| {
            Ok(read_all(db, Some(&database_provider))?
                .into_iter()
                .find(|connection| connection["isActive"] != false))
        })
        .await;
    let mut account = match selected {
        Ok(Some(account)) => account,
        Ok(None) => return bad(&format!("No active connection for provider: {provider}")),
        Err(_) => {
            let name = match provider {
                "zed" => "Zed",
                "windsurf" => "Windsurf",
                "grok-web" => "Grok Web",
                "grok-cli" => "Grok CLI",
                "deepseek-web" => "DeepSeek Web",
                "vertex" | "vertex-partner" => "Vertex",
                _ if crate::cline_transport::is_provider(provider) => "Cline",
                _ => provider,
            };
            return failure(
                StatusCode::BAD_GATEWAY,
                &format!("Could not load the saved {name} account"),
            );
        }
    };
    if provider == "devin-cli" {
        let Some(settings) = state.storage.read_settings().await else {
            return failure(StatusCode::BAD_GATEWAY, "Could not load provider settings");
        };
        let headers = &input.parts.headers;
        return crate::devin_acp::respond(
            &state.storage,
            &settings,
            crate::devin_acp::Caller {
                direct_loopback: crate::catalog_guard::is_direct_loopback_parts(
                    headers,
                    &input.parts.extensions,
                ),
                key: api_key.as_deref(),
                format: ChatFormat::Chat,
                accept: headers
                    .get(header::ACCEPT)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default(),
            },
            body.clone(),
            model,
        )
        .await;
    }
    if let Some(message) = crate::chat::media_chat_message(provider) {
        return upstream_error_reply(
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":message,"type":"invalid_request"}}).to_string(),
            &[],
        );
    }
    if provider == "zed" {
        return send_zed(&state, &account, model, body).await;
    }
    if provider == "github" {
        return send_github(&state, &account, model, body).await;
    }
    if provider == "kiro" {
        return send_kiro(&state, &account, model, body, &input.parts.headers).await;
    }
    if provider == "grok-cli" {
        return send_grok_cli(&state, &account, model, body, &input.parts.headers).await;
    }
    if provider == "antigravity" {
        return send_antigravity(&state, &account, model, body, &input.parts.headers).await;
    }
    if provider == "commandcode" {
        return send_commandcode(&state, &account, model, body, &input.parts.headers).await;
    }
    if provider == "claude" {
        return send_claude(&state, &account, model, body).await;
    }
    if provider == "nous" {
        return send_nous(&state, &account, model, body).await;
    }
    if crate::qoder_models::supports(provider) {
        return send_qoder(&state, &account, model, body).await;
    }
    if crate::bedrock_transport::is_provider(provider) {
        return send_bedrock(&state, &account, provider, model, body).await;
    }
    if is_minimax(provider) {
        return send_minimax(&state, &account, provider, model, body).await;
    }
    if provider == "windsurf" {
        let Some(settings) = state.storage.read_settings().await else {
            return failure(StatusCode::BAD_GATEWAY, "Could not load Windsurf settings");
        };
        return match crate::windsurf_chat::complete(
            &state.storage,
            &settings,
            &account,
            model,
            body,
        )
        .await
        {
            Ok(response) => upstream_reply(response, selected_secrets(&account)).await,
            Err(error) => failure(error.status, &error.message),
        };
    }
    if is_web_chat_provider(provider) || matches!(provider, "cursor" | "trae") {
        let Some(settings) = state.storage.read_settings().await else {
            return failure(
                StatusCode::BAD_GATEWAY,
                "Could not load the saved provider settings",
            );
        };
        let headers = &input.parts.headers;
        let stream = body["stream"] != false;
        let result = if provider == "perplexity-web" {
            let caller = crate::dashboard_session::authorization_owner(&state.storage, headers)
                .await
                .map(|owner| format!("translator/{}", URL_SAFE_NO_PAD.encode(owner)));
            crate::perplexity_web_chat::complete(
                &state.storage,
                &settings,
                &account,
                caller.as_deref(),
                model,
                body,
                stream,
            )
            .await
            .map_err(|error| (error.status, error.message))
        } else if provider == "muse-web" {
            crate::muse_web::complete(&state.storage, &settings, &account, model, body, stream)
                .await
                .map_err(|error| (error.status, error.message))
        } else if provider == "deepseek-web" {
            let caller = if crate::dashboard_session::has_session(&state.storage, headers) {
                crate::dashboard_session::authorization_owner(&state.storage, headers)
                    .await
                    .map(|owner| format!("translator/{}", URL_SAFE_NO_PAD.encode(owner)))
            } else {
                None
            };
            crate::deepseek_web::complete(
                &state.storage,
                &settings,
                &account,
                caller.as_deref(),
                model,
                body,
                stream,
            )
            .await
            .map_err(|error| (error.status, error.message))
        } else if provider == "grok-web" {
            crate::grok_web::complete(&state.storage, &settings, &account, model, body, stream)
                .await
                .map_err(|error| (error.status, error.message))
        } else if provider == "gemini-web" {
            crate::gemini_web::complete(&state.storage, &settings, &account, model, body, stream)
                .await
                .map_err(|error| (error.status, error.message))
        } else if provider == "chatgpt-web" {
            crate::chatgpt_web::complete(&state.storage, &settings, &account, model, body, stream)
                .await
                .map_err(|error| (error.status, error.message))
        } else if provider == "trae" {
            crate::trae_chat::complete(&state.storage, &settings, &account, model, body, stream)
                .await
                .map_err(|error| (error.status, error.message))
        } else {
            crate::cursor_chat::complete(
                &state.storage,
                &settings,
                &account,
                model,
                headers,
                body,
                stream,
            )
            .await
            .map_err(|error| (error.status, error.message))
        };
        return match result {
            Ok(response) => upstream_reply(response, selected_secrets(&account)).await,
            Err((status, message)) => failure(status, &message),
        };
    }
    if crate::cline_transport::is_provider(provider) {
        return send_cline(&state, &account, provider, model, body).await;
    }
    if matches!(provider, "vertex" | "vertex-partner") {
        return send_vertex(&state, &account, provider, model, body).await;
    }
    if provider == "xiaomi-mimo"
        && crate::mimo_account::account_model(model)
        && crate::mimo_account::has_session(&account)
    {
        return send_mimo_desktop(&state, &account, body).await;
    }
    let codex = provider == "codex";
    let chat_routed = codex || provider == "gemini-cli";
    let xai_oauth = provider == "xai" && account["authType"] == "oauth";
    let routed_oauth = chat_routed || xai_oauth;
    let iflow = provider == "iflow";
    if !supports_account(provider, &account) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Missing provider account credentials",
        );
    }

    let mut plan = if codex {
        let session = state
            .sessions
            .pick(body, &account, &input.parts.headers, None);
        let Some(mut plan) = codex_plan(body.clone(), model, &account, session) else {
            return bad("Invalid Codex request body");
        };
        let images =
            crate::chat_prefetch::apply(&mut plan.body, crate::chat_prefetch::Source::Responses)
                .await;
        if images.refused {
            return bad("Image URL must resolve to a public HTTP address");
        }
        if images.unavailable {
            return failure(
                StatusCode::BAD_GATEWAY,
                "Could not prepare linked image for Codex",
            );
        }
        plan
    } else {
        match plan(provider, model, body.clone(), &account) {
            Some(plan) => plan,
            None if provider == "gemini-cli" => {
                return bad("Invalid Gemini CLI request body or missing project");
            }
            None => return bad("Invalid provider request body or endpoint"),
        }
    };
    if matches!(plan.transport, Transport::GeminiCli) {
        let Some(request) = plan.body.get_mut("request").and_then(Value::as_object_mut) else {
            return bad("Invalid Gemini CLI request envelope");
        };
        request.entry("sessionId").or_insert_with(|| {
            let session = state
                .sessions
                .pick(body, &account, &input.parts.headers, None);
            json!(crate::gemini_cli_transport::numeric_session(session))
        });
    }
    if provider == "kimchi" {
        crate::kimchi_chat::prepare(&state.storage, &account, model, &mut plan.body);
    }
    let Ok(url) = Url::parse(&plan.destination) else {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, "Invalid provider URL");
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, "Invalid provider URL");
    }

    let Some(settings) = state.storage.read_settings().await else {
        return failure(StatusCode::BAD_GATEWAY, "Could not load provider settings");
    };
    let route = match outbound_route(&state, &settings, &account, &plan.destination).await {
        Ok(Route::Direct) if account["providerSpecificData"]["strictProxy"] == true => {
            return failure(
                StatusCode::BAD_GATEWAY,
                "Required provider proxy is unavailable",
            );
        }
        Ok(Route::MissingStrict) | Err(_) => {
            return failure(
                StatusCode::BAD_GATEWAY,
                "Required provider proxy is unavailable",
            );
        }
        Ok(route) => route,
    };
    let kimi_oauth = provider == "kimi" && account["authType"] == "oauth";
    let codebuddy_oauth =
        static_chat::codebuddy::is_provider(provider) && account["authType"] == "oauth";
    if kimi_oauth && matches!(route, Route::Relay { .. }) {
        return failure(
            StatusCode::BAD_GATEWAY,
            "Kimi OAuth requires direct or tunneled access",
        );
    }
    let mut iflow_refresh = iflow && crate::iflow_transport::can_refresh(&account);
    let mut secrets = selected_secrets(&account);
    if iflow || chat_routed {
        secrets.extend(
            route
                .relay_secret()
                .map(|secret| secret.as_bytes().to_vec()),
        );
    }
    let mut refreshed_before_send = false;
    let chat_relay = chat_routed.then(|| route.relay_identity()).flatten();
    if chat_routed
        && (account["accessToken"].as_str().is_none_or(str::is_empty)
            || chat_relay.is_some()
                && account["authType"] == "oauth"
                && truthy(&account["refreshToken"])
                && crate::provider_oauth::refresh_due_for_provider(provider, &account))
    {
        let Ok(updated) = crate::provider_oauth::refresh_account(
            &state.storage,
            &account,
            chat_relay.map_or(
                crate::provider_oauth::RefreshMode::ChatRouted { rejected: false },
                crate::provider_oauth::RefreshMode::RelayRouted,
            ),
        )
        .await
        else {
            return failure(
                StatusCode::UNAUTHORIZED,
                "Provider OAuth credentials could not be renewed",
            );
        };
        if updated["accessToken"].as_str().is_none_or(str::is_empty) {
            return failure(
                StatusCode::UNAUTHORIZED,
                "Provider OAuth credentials could not be renewed",
            );
        }
        if !same_outbound_route(&state, &route, &updated, &plan.destination).await {
            return failure(
                StatusCode::BAD_GATEWAY,
                "Provider outbound route changed during credential refresh",
            );
        }
        if let Transport::Codex(exchange) = &mut plan.transport {
            exchange.refresh_binding(&updated);
        }
        secrets.extend(selected_secrets(&updated));
        account = updated;
        refreshed_before_send = true;
    }
    if xai_oauth
        && (!truthy(&account["accessToken"])
            || crate::provider_oauth::refresh_due_for_provider(provider, &account))
        && truthy(&account["refreshToken"])
        && let Ok(updated) =
            crate::provider_oauth::refresh_xai_routed(&state.storage, &settings, &account).await
        && updated["isActive"] != false
        && truthy(&updated["accessToken"])
    {
        if !same_outbound_route(&state, &route, &updated, &plan.destination).await {
            return failure(
                StatusCode::BAD_GATEWAY,
                "xAI outbound route changed during credential refresh",
            );
        }
        secrets.extend(selected_secrets(&updated));
        account = updated;
        refreshed_before_send = true;
    }
    if xai_oauth && !truthy(&account["accessToken"]) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "xAI OAuth credentials are unavailable or expired",
        );
    }
    if iflow && !crate::iflow_transport::supports(&account) {
        let updated = if iflow_refresh {
            refresh_for_retry(&state, &settings, &account, &route, false).await
        } else {
            None
        };
        let Some(updated) = updated.filter(|updated| {
            updated["isActive"] != false && crate::iflow_transport::supports(updated)
        }) else {
            return failure(
                StatusCode::UNAUTHORIZED,
                "iFlow provider credentials have expired",
            );
        };
        if !same_outbound_route(&state, &route, &updated, &plan.destination).await {
            return failure(
                StatusCode::BAD_GATEWAY,
                "iFlow outbound route changed during credential refresh",
            );
        }
        secrets.extend(selected_secrets(&updated));
        account = updated;
        iflow_refresh = false;
    }
    if provider == "azure" {
        secrets.extend(crate::azure_transport::credential(&account).map(String::into_bytes));
    }
    let mut upstream = execute(&state.send, &route, &plan, provider, model, &account).await;
    if (kimi_oauth || codebuddy_oauth || routed_oauth || iflow_refresh)
        && !refreshed_before_send
        && (!matches!(route, Route::Relay { .. }) || xai_oauth || iflow_refresh || chat_routed)
        && truthy(&account["refreshToken"])
        && upstream.as_ref().is_ok_and(|response| {
            matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            )
        })
        && let Some(updated) =
            refresh_for_retry(&state, &settings, &account, &route, routed_oauth).await
        && updated["isActive"] != false
        && supports_account(provider, &updated)
        && (updated["accessToken"] != account["accessToken"]
            || iflow_refresh && updated["apiKey"] != account["apiKey"])
        && (!iflow_refresh || crate::iflow_transport::supports(&updated))
        && same_outbound_route(&state, &route, &updated, &plan.destination).await
    {
        if let Transport::Codex(exchange) = &mut plan.transport {
            exchange.refresh_binding(&updated);
        }
        secrets.extend(selected_secrets(&updated));
        drop(upstream);
        upstream = execute(&state.send, &route, &plan, provider, model, &updated).await;
    }
    match upstream {
        Ok(response) => upstream_reply(response, secrets).await,
        Err(message) => failure(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}

async fn same_outbound_route(
    state: &Translator,
    route: &Route,
    account: &Value,
    destination: &str,
) -> bool {
    let Some(settings) = state.storage.read_settings().await else {
        return false;
    };
    matches!(outbound_route(state, &settings, account, destination).await, Ok(next) if &next == route)
        && (*route != Route::Direct || account["providerSpecificData"]["strictProxy"] != true)
}

async fn outbound_route(
    state: &Translator,
    settings: &Value,
    account: &Value,
    destination: &str,
) -> Result<Route, ()> {
    if matches!(
        account["provider"].as_str(),
        Some("xai" | "iflow" | "codex" | "gemini-cli")
    ) && matches!(account["authType"].as_str(), Some("oauth" | "access_token"))
    {
        return match crate::provider_http::select(&state.storage, settings, account, destination)
            .await
            .map_err(|_| ())?
        {
            proxy_transport::Selection::ExplicitBypass => Ok(Route::Direct),
            proxy_transport::Selection::Routed(route) => Ok(route),
        };
    }
    proxy_transport::select(&state.storage, settings, account, destination).await
}

async fn refresh_for_retry(
    state: &Translator,
    settings: &Value,
    account: &Value,
    route: &Route,
    routed: bool,
) -> Option<Value> {
    use crate::provider_oauth::{RefreshMode, refresh_account, test_token_url};

    let mode = if matches!(account["provider"].as_str(), Some("codex" | "gemini-cli"))
        && let Some(identity) = route.relay_identity()
    {
        RefreshMode::RelayRouted(identity)
    } else if account["provider"] == "iflow" {
        RefreshMode::ChatRouted { rejected: true }
    } else if routed {
        let endpoint = test_token_url(account["provider"].as_str()?)?;
        let delivery =
            crate::provider_http::Delivery::select(&state.storage, settings, account, endpoint)
                .await
                .ok()?;
        RefreshMode::Routed(delivery.protected())
    } else {
        RefreshMode::Rejected
    };
    refresh_account(&state.storage, account, mode).await.ok()
}

async fn send_github(state: &Translator, account: &Value, model: &str, body: &Value) -> Response {
    use crate::provider_http::{same_egress, select_authenticated};
    use crate::provider_oauth::{RefreshMode, refresh_account};
    use crate::proxy_transport::Selection;

    if github_executor::token(account).is_none()
        && !crate::provider_oauth::supports_chat_credentials("github", account)
    {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Missing GitHub Copilot account credentials",
        );
    }
    let Some(mut settings) = state.storage.read_settings().await else {
        return failure(
            StatusCode::BAD_GATEWAY,
            "Could not load GitHub Copilot settings",
        );
    };
    let stream = body["stream"] != false;
    let mut mode = GithubRoute::preferred(model, &state.github_routes);
    let mut outbound = None;
    let mut account = std::borrow::Cow::Borrowed(account);
    let mut secrets = selected_secrets(&account);
    let mut renewed = false;
    loop {
        let (destination, request) = match mode {
            GithubRoute::Messages => (
                github_claude::url(),
                github_claude::prepare(body.clone(), model),
            ),
            GithubRoute::Responses => (
                github_responses::url(),
                github_responses::prepare(body.clone(), model),
            ),
            GithubRoute::Chat => {
                let mut request = body.clone();
                let normalized = github_executor::prepare(&mut request, model);
                (github_executor::url(), normalized.map(|()| request))
            }
        };
        let Some(request) = request else {
            return bad("Invalid GitHub Copilot request body");
        };
        let Ok(selected) =
            select_authenticated(&state.storage, &settings, &account, destination).await
        else {
            return failure(
                StatusCode::BAD_GATEWAY,
                "GitHub Copilot proxy is unavailable",
            );
        };
        if outbound
            .as_ref()
            .is_some_and(|first| !same_egress(first, &selected))
        {
            return failure(
                StatusCode::BAD_GATEWAY,
                "GitHub Copilot outbound route changed",
            );
        }
        outbound.get_or_insert_with(|| selected.clone());
        let route = match &selected {
            Selection::ExplicitBypass => Route::Direct,
            Selection::Routed(route) => route.clone(),
        };
        let relay = route.relay_identity();
        if let Some(secret) = route.relay_secret() {
            secrets.push(secret.as_bytes().to_vec());
        }
        if github_executor::token(&account).is_none() {
            let Ok(updated) =
                refresh_account(&state.storage, &account, RefreshMode::github(false, relay)).await
            else {
                return failure(
                    StatusCode::UNAUTHORIZED,
                    "GitHub Copilot credentials could not be renewed",
                );
            };
            if github_executor::token(&updated).is_none() {
                return failure(
                    StatusCode::UNAUTHORIZED,
                    "GitHub Copilot credentials could not be renewed",
                );
            }
            let current = state.storage.read_settings().await;
            if !matches!(&current, Some(settings) if matches!(
                select_authenticated(&state.storage, settings, &updated, destination).await,
                Ok(actual) if actual == selected
            )) {
                return failure(
                    StatusCode::BAD_GATEWAY,
                    "GitHub Copilot outbound route changed during credential refresh",
                );
            }
            renewed = true;
            secrets.extend(selected_secrets(&updated));
            account = std::borrow::Cow::Owned(updated);
            settings = current.expect("verified Copilot outbound settings");
        }
        let plan = Plan {
            destination: destination.to_owned(),
            transport: Transport::Github(mode),
            stream,
            body: request,
            timeout: connect_timeout(),
        };
        let Ok(response) = execute(&state.send, &route, &plan, "github", model, &account).await
        else {
            return failure(StatusCode::BAD_GATEWAY, "GitHub Copilot request failed");
        };
        let status = response.status();
        if !renewed
            && matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
            && let Ok(updated) =
                refresh_account(&state.storage, &account, RefreshMode::github(true, relay)).await
            && updated["isActive"] != false
            && github_executor::token(&updated)
                .is_some_and(|token| Some(token) != github_executor::token(&account))
            && let Some(current) = state.storage.read_settings().await
            && matches!(select_authenticated(&state.storage, &current, &updated, destination).await, Ok(next) if next == selected)
        {
            renewed = true;
            secrets.extend(selected_secrets(&updated));
            account = std::borrow::Cow::Owned(updated);
            settings = current;
            continue;
        }
        if mode == GithubRoute::Chat && status == StatusCode::BAD_REQUEST {
            let details = response.text().await.unwrap_or_default();
            if github_executor::needs_responses(model, status.as_u16(), &details) {
                state.github_routes.remember(model);
                mode = GithubRoute::Responses;
                continue;
            }
            return upstream_error_reply(status, details, &secrets);
        }
        let stall = crate::chat_output::stall_timeout("github");
        let response = match mode {
            GithubRoute::Messages => github_claude::adapt(response, model, stream, stall),
            GithubRoute::Responses => github_responses::adapt(response, model, stream, stall),
            GithubRoute::Chat => response,
        };
        return upstream_reply(response, secrets).await;
    }
}

async fn send_commandcode(
    state: &Translator,
    account: &Value,
    model: &str,
    body: &Value,
    headers: &HeaderMap,
) -> Response {
    use crate::commandcode_transport::{self as commandcode, Probe};

    if !commandcode::supports(account) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Missing CommandCode account credentials",
        );
    }
    let Some(settings) = state.storage.read_settings().await else {
        return failure(
            StatusCode::BAD_GATEWAY,
            "Could not load CommandCode settings",
        );
    };
    let Some(plan) = commandcode_plan(body, model, account, headers) else {
        return bad("Invalid CommandCode request body");
    };
    let route = match proxy_transport::select(&state.storage, &settings, account, &plan.destination)
        .await
    {
        Ok(Route::Direct) if account["providerSpecificData"]["strictProxy"] == true => {
            return failure(StatusCode::BAD_GATEWAY, "CommandCode proxy is unavailable");
        }
        Ok(Route::MissingStrict) | Err(_) => {
            return failure(StatusCode::BAD_GATEWAY, "CommandCode proxy is unavailable");
        }
        Ok(route) => route,
    };
    let private = selected_secrets(account);
    let stall = crate::chat_output::stall_timeout("commandcode");
    let mut retries = 0;
    loop {
        let Ok(upstream) = execute(&state.send, &route, &plan, "commandcode", model, account).await
        else {
            return failure(StatusCode::BAD_GATEWAY, "CommandCode request failed");
        };
        match commandcode::probe(upstream, model, account, stall).await {
            Probe::Response(response) => return upstream_reply(response, private).await,
            Probe::Rejected { status, .. }
                if retries < 2 && matches!(status.as_u16(), 502..=504) =>
            {
                retries += 1;
                tokio::time::sleep(Duration::from_secs(retries)).await;
            }
            Probe::Rejected { status, message } => {
                return upstream_error_reply(status, message, &private);
            }
        }
    }
}

async fn send_claude(state: &Translator, account: &Value, model: &str, body: &Value) -> Response {
    if !body["messages"].is_array() {
        return bad("Invalid Claude Messages request body");
    }
    let Some(settings) = state.storage.read_settings().await else {
        return failure(StatusCode::BAD_GATEWAY, "Could not load Claude settings");
    };
    let mut body = body.clone();
    if !truthy(&body["model"]) {
        body["model"] = json!(model);
    }
    let prepared = PreparedChat {
        stream: body["stream"] != false,
        body,
    };
    match crate::claude_transport::complete(
        &state.storage,
        &settings,
        account,
        model,
        &prepared,
        "",
    )
    .await
    {
        Ok((response, secrets)) => upstream_reply(response, secrets).await,
        Err(error) => failure(error.status, error.message),
    }
}

async fn send_nous(state: &Translator, account: &Value, model: &str, body: &Value) -> Response {
    use crate::provider_oauth::{
        RefreshMode, nous_inference_token, nous_refresh_due, nous_refresh_supported,
    };

    if !supports_account("nous", account) {
        return failure(StatusCode::UNAUTHORIZED, "Missing Nous account credentials");
    }
    let Some(plan) = plan("nous", model, body.clone(), account) else {
        return bad("Invalid Nous request body");
    };
    let Some(settings) = state.storage.read_settings().await else {
        return failure(StatusCode::BAD_GATEWAY, "Could not load Nous settings");
    };
    let Ok(route) =
        proxy_transport::select(&state.storage, &settings, account, &plan.destination).await
    else {
        return failure(StatusCode::BAD_GATEWAY, "Nous proxy is unavailable");
    };
    if route == Route::MissingStrict
        || route == Route::Direct && account["providerSpecificData"]["strictProxy"] == true
    {
        return failure(StatusCode::BAD_GATEWAY, "Nous proxy is unavailable");
    }
    let renewable = nous_refresh_supported(account);
    let mut private = selected_secrets(account);
    let mut current = std::borrow::Cow::Borrowed(account);
    let mut refreshed = false;
    if renewable && nous_refresh_due(account) {
        match refresh_nous(
            state,
            account,
            &route,
            &plan.destination,
            RefreshMode::Proactive,
        )
        .await
        {
            Ok(updated) => {
                private.extend(selected_secrets(&updated));
                current = std::borrow::Cow::Owned(updated);
                refreshed = true;
            }
            Err(StatusCode::BAD_GATEWAY) => {
                return failure(
                    StatusCode::BAD_GATEWAY,
                    "Nous outbound route changed during credential refresh",
                );
            }
            Err(_) => (),
        }
    }
    if nous_inference_token(&current).is_none() {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Nous provider credentials are unavailable or expired",
        );
    }
    let Ok(mut response) = execute(&state.send, &route, &plan, "nous", model, &current).await
    else {
        return failure(StatusCode::BAD_GATEWAY, "Nous request failed");
    };
    if renewable
        && !refreshed
        && matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        )
        && let Ok(updated) = refresh_nous(
            state,
            &current,
            &route,
            &plan.destination,
            RefreshMode::Rejected,
        )
        .await
        && nous_inference_token(&updated) != nous_inference_token(&current)
    {
        private.extend(selected_secrets(&updated));
        drop(response);
        response = match execute(&state.send, &route, &plan, "nous", model, &updated).await {
            Ok(response) => response,
            Err(message) => return failure(StatusCode::BAD_GATEWAY, message),
        };
    }
    upstream_reply(response, private).await
}

async fn refresh_nous(
    state: &Translator,
    account: &Value,
    route: &Route,
    destination: &str,
    mode: crate::provider_oauth::RefreshMode,
) -> Result<Value, StatusCode> {
    let updated = crate::provider_oauth::refresh_account(&state.storage, account, mode)
        .await
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    if updated["isActive"] == false
        || crate::provider_oauth::nous_inference_token(&updated).is_none()
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let settings = state
        .storage
        .read_settings()
        .await
        .ok_or(StatusCode::BAD_GATEWAY)?;
    if !matches!(proxy_transport::select(&state.storage, &settings, &updated, destination).await, Ok(next) if &next == route)
        || *route == Route::Direct && updated["providerSpecificData"]["strictProxy"] == true
    {
        return Err(StatusCode::BAD_GATEWAY);
    }
    Ok(updated)
}

async fn send_qoder(state: &Translator, account: &Value, model: &str, body: &Value) -> Response {
    if !crate::qoder_chat::supports(account) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Missing Qoder account credentials",
        );
    }
    if !body["messages"].is_array() {
        return bad("Invalid Qoder request body");
    }
    let Some(settings) = state.storage.read_settings().await else {
        return failure(StatusCode::BAD_GATEWAY, "Could not load Qoder settings");
    };
    match crate::qoder_chat::complete(&state.storage, &settings, account, model, body).await {
        Ok(response) => upstream_reply(response, selected_secrets(account)).await,
        Err(error) => failure(error.status, &error.message),
    }
}

async fn send_bedrock(
    state: &Translator,
    account: &Value,
    provider: &str,
    model: &str,
    body: &Value,
) -> Response {
    use crate::bedrock_transport::{self as bedrock, Exchange};

    if !body.is_object() {
        return bad("Invalid Bedrock request body");
    }
    if !bedrock::supports(provider, model, account) {
        return bad("Invalid Bedrock model or account configuration");
    }
    let Some(settings) = state.storage.read_settings().await else {
        return failure(StatusCode::BAD_GATEWAY, "Could not load Bedrock settings");
    };
    let stream = body["stream"] != false;
    let egress =
        match bedrock::select_egress(&state.storage, &settings, account, model, stream).await {
            Ok(egress) => egress,
            Err(message) => return failure(StatusCode::BAD_GATEWAY, message),
        };
    let prepared = PreparedChat {
        body: body.clone(),
        stream,
    };
    let exchange = match Exchange::new(
        provider,
        model,
        account,
        &prepared,
        egress,
        &state.storage,
        &settings,
    )
    .await
    {
        Ok(exchange) => exchange,
        Err(message) => return failure(StatusCode::BAD_GATEWAY, message),
    };
    match exchange.send().await {
        Ok((response, redactions)) => {
            let mut private = selected_secrets(account);
            private.extend(
                redactions
                    .secrets()
                    .map(|secret| secret.as_bytes().to_vec()),
            );
            upstream_reply(response, private).await
        }
        Err(message) => failure(StatusCode::BAD_GATEWAY, message),
    }
}

async fn send_minimax(
    state: &Translator,
    account: &Value,
    provider: &str,
    model: &str,
    body: &Value,
) -> Response {
    use crate::provider_oauth::{RefreshMode, minimax_code::lease, refresh_account};

    if !minimax_account(provider, account) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Missing MiniMax account credentials",
        );
    }
    let Some(plan) = minimax_plan(provider, model, body.clone(), account) else {
        return bad("Invalid MiniMax request body");
    };
    let Some(settings) = state.storage.read_settings().await else {
        return failure(StatusCode::BAD_GATEWAY, "Could not load MiniMax settings");
    };
    let Ok(route) =
        proxy_transport::select(&state.storage, &settings, account, &plan.destination).await
    else {
        return failure(StatusCode::BAD_GATEWAY, "MiniMax proxy is unavailable");
    };
    if matches!(route, Route::MissingStrict)
        || route == Route::Direct && account["providerSpecificData"]["strictProxy"] == true
    {
        return failure(StatusCode::BAD_GATEWAY, "MiniMax proxy is unavailable");
    }
    let code = matches!(provider, "minimax-code" | "minimax-code-global");
    let leased = account["authType"] == "mcode-lease";
    let force_refresh = code && !leased && !truthy(&account["accessToken"]);
    let mut account = if leased {
        match lease::credentials(account, None).await {
            Ok(updated) => std::borrow::Cow::Owned(updated),
            Err(message) => return failure(StatusCode::SERVICE_UNAVAILABLE, message),
        }
    } else if force_refresh {
        match refresh_account(&state.storage, account, RefreshMode::Rejected).await {
            Ok(updated) if updated["isActive"] != false && truthy(&updated["accessToken"]) => {
                std::borrow::Cow::Owned(updated)
            }
            _ => {
                return failure(
                    StatusCode::UNAUTHORIZED,
                    "MiniMax credential refresh failed",
                );
            }
        }
    } else {
        std::borrow::Cow::Borrowed(account)
    };
    if matches!(&account, std::borrow::Cow::Owned(_)) {
        let Some(current) = state.storage.read_settings().await else {
            return failure(StatusCode::BAD_GATEWAY, "Could not load MiniMax settings");
        };
        if !matches!(proxy_transport::select(&state.storage, &current, &account, &plan.destination).await, Ok(next) if next == route)
            || route == Route::Direct && account["providerSpecificData"]["strictProxy"] == true
        {
            return failure(
                StatusCode::BAD_GATEWAY,
                "MiniMax proxy changed during credential refresh",
            );
        }
    }
    let mut private = selected_secrets(&account);
    let mut renewed = force_refresh;
    loop {
        let Ok(response) = execute(&state.send, &route, &plan, provider, model, &account).await
        else {
            return failure(StatusCode::BAD_GATEWAY, "MiniMax request failed");
        };
        let status = response.status();
        if status.is_success() || !code {
            return upstream_reply(response, private).await;
        }
        let details = response.text().await.unwrap_or_default();
        if let Some(message) = crate::chat::minimax_code_quota(status, &details) {
            let details =
                json!({"type":"error","error":{"type":"rate_limit_error","message":message}})
                    .to_string();
            return upstream_error_reply(StatusCode::TOO_MANY_REQUESTS, details, &private);
        }
        if !renewed && matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            let updated = if leased {
                lease::credentials(&account, account["accessToken"].as_str())
                    .await
                    .ok()
            } else {
                refresh_account(&state.storage, &account, RefreshMode::Rejected)
                    .await
                    .ok()
            };
            if let Some(updated) = updated
                && updated["isActive"] != false
                && updated["accessToken"] != account["accessToken"]
                && truthy(&updated["accessToken"])
                && let Some(settings) = state.storage.read_settings().await
                && matches!(proxy_transport::select(&state.storage, &settings, &updated, &plan.destination).await, Ok(next) if next == route)
                && (route != Route::Direct
                    || updated["providerSpecificData"]["strictProxy"] != true)
            {
                renewed = true;
                private.extend(selected_secrets(&updated));
                account = std::borrow::Cow::Owned(updated);
                continue;
            }
        }
        return upstream_error_reply(status, details, &private);
    }
}

async fn send_zed(state: &Translator, account: &Value, model: &str, body: &Value) -> Response {
    if !crate::zed_transport::supports(account) {
        return failure(StatusCode::UNAUTHORIZED, "Missing Zed account credentials");
    }
    let Some(settings) = state.storage.read_settings().await else {
        return failure(StatusCode::BAD_GATEWAY, "Could not load Zed settings");
    };
    let route = match crate::zed_transport::egress(&state.storage, &settings, account).await {
        Ok(route) => route,
        Err(message) => return failure(StatusCode::BAD_GATEWAY, message),
    };
    let mut private = selected_secrets(account);
    match crate::zed_transport::translator_reply(&state.storage, account, model, body, route).await
    {
        Ok((response, secrets)) => {
            private.extend(secrets);
            upstream_reply(response, private).await
        }
        Err(error) => failure(error.status, &scrub_text(error.message, &private)),
    }
}

async fn send_kiro(
    state: &Translator,
    account: &Value,
    model: &str,
    source: &Value,
    headers: &HeaderMap,
) -> Response {
    use crate::chat::refresh_kiro_chat;
    use crate::kiro_chat;
    use crate::provider_oauth::RefreshMode;

    let Some(mut settings) = state.storage.read_settings().await else {
        return failure(StatusCode::BAD_GATEWAY, "Could not load Kiro settings");
    };
    let oauth_route = if account["authType"] == "oauth" {
        let Ok(binding) = kiro_chat::OAuthRoute::select(&state.storage, &settings, account).await
        else {
            return failure(StatusCode::BAD_GATEWAY, "Kiro OAuth route is unavailable");
        };
        Some(binding)
    } else {
        None
    };
    let body = if source["conversationState"].is_object() {
        source.clone()
    } else {
        let caller = crate::dashboard_session::authorization_owner(&state.storage, headers)
            .await
            .map(|owner| format!("translator/{}", URL_SAFE_NO_PAD.encode(owner)));
        let mut source = source.clone();
        kiro_chat::strip_model_media(&mut source, model);
        let Some(body) = kiro_chat::prepare(&source, model, headers, caller.as_deref()) else {
            return bad("Invalid Kiro request body");
        };
        body
    };

    let protected = oauth_route
        .as_ref()
        .is_some_and(kiro_chat::OAuthRoute::protected);
    let renewable = oauth_route
        .as_ref()
        .is_some_and(|route| route.supports_refresh(account));
    let mut account = std::borrow::Cow::Borrowed(account);
    let mut secrets = selected_secrets(&account);
    secrets.extend(
        oauth_route
            .as_ref()
            .and_then(kiro_chat::OAuthRoute::relay_secret)
            .map(|secret| secret.as_bytes().to_vec()),
    );
    let mut renewed = false;
    loop {
        if renewable
            && !renewed
            && kiro_chat::needs_refresh(&account, protected)
            && let Some(binding) = &oauth_route
        {
            let Ok(updated) =
                refresh_kiro_chat(&state.storage, &account, binding, RefreshMode::Proactive).await
            else {
                return failure(StatusCode::UNAUTHORIZED, "Kiro credential refresh failed");
            };
            secrets.extend(selected_secrets(&updated));
            account = std::borrow::Cow::Owned(updated);
            renewed = true;
            let Some(current) = state.storage.read_settings().await else {
                return failure(StatusCode::BAD_GATEWAY, "Could not load Kiro settings");
            };
            settings = current;
        }
        let response =
            match kiro_response(state, &settings, &account, &body, oauth_route.as_ref()).await {
                Ok(response) => response,
                Err((status, message)) => return failure(status, message),
            };
        if renewable
            && !renewed
            && matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            )
            && let Some(binding) = &oauth_route
            && let Ok(updated) =
                refresh_kiro_chat(&state.storage, &account, binding, RefreshMode::Rejected).await
            && updated["accessToken"] != account["accessToken"]
            && let Some(current) = state.storage.read_settings().await
        {
            secrets.extend(selected_secrets(&updated));
            account = std::borrow::Cow::Owned(updated);
            settings = current;
            renewed = true;
            continue;
        }
        if !response.status().is_success() {
            let status = response.status();
            let details =
                kiro_chat::redact_error(&account, &response.text().await.unwrap_or_default());
            return upstream_error_reply(status, details, &secrets);
        }
        let response = kiro_chat::adapt(
            response,
            model,
            &body,
            crate::chat_output::stall_timeout("kiro"),
        );
        return upstream_reply(response, secrets).await;
    }
}

async fn kiro_response(
    state: &Translator,
    settings: &Value,
    account: &Value,
    body: &Value,
    oauth_route: Option<&crate::kiro_chat::OAuthRoute>,
) -> Result<reqwest::Response, (StatusCode, &'static str)> {
    use crate::kiro_chat::{self, Connection};

    let unavailable = (
        StatusCode::BAD_GATEWAY,
        "Kiro upstream route is unavailable",
    );
    let connection = match oauth_route {
        Some(binding) => binding.connection(account),
        None => Connection::api_key(account),
    }
    .ok_or((
        StatusCode::UNAUTHORIZED,
        "Missing or expired Kiro account credentials",
    ))?;
    let endpoint = connection.upstream_url();
    let route = if let Some(binding) = oauth_route {
        if endpoint != binding.endpoint || !binding.verify(&state.storage, account).await {
            return Err(unavailable);
        }
        binding.request_route()
    } else {
        match crate::provider_http::select(&state.storage, settings, account, &endpoint)
            .await
            .map_err(|_| unavailable)?
        {
            proxy_transport::Selection::ExplicitBypass => Route::Direct,
            proxy_transport::Selection::Routed(route) => route,
        }
    };
    let outgoing = connection.body(body, account["id"].as_str());
    let timeout = connect_timeout();
    let (client, url) = match &route {
        Route::Direct => (state.send.pooled.clone(), connection.url()),
        Route::Proxy { url, .. } => kiro_chat::proxy_client(url, &endpoint).ok_or(unavailable)?,
        Route::Relay { url, .. } => (state.send.pooled.clone(), url.clone()),
        _ => return Err(unavailable),
    };
    let request = route.relay(connection.headers(client.post(&url).json(&*outgoing), false));
    let response = tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| unavailable)?
        .map_err(|_| unavailable)?;
    if matches!(route, Route::Direct)
        && let Some((endpoint, alternate)) = connection.alternate(response.status())
        && let Some(settings) = state.storage.read_settings().await
        && matches!(
            crate::provider_http::Delivery::select(&state.storage, &settings, account, &endpoint)
                .await,
            Ok(crate::provider_http::Delivery::Direct | crate::provider_http::Delivery::Bypass)
        )
    {
        let request = connection.headers(client.post(alternate).json(&*outgoing), true);
        return tokio::time::timeout(timeout, request.send())
            .await
            .map_err(|_| unavailable)?
            .map_err(|_| unavailable);
    }
    if matches!(route, Route::Proxy { .. } | Route::Relay { .. })
        && response.status().is_redirection()
    {
        return Err(unavailable);
    }
    Ok(response)
}

async fn send_grok_cli(
    state: &Translator,
    account: &Value,
    model: &str,
    body: &Value,
    headers: &HeaderMap,
) -> Response {
    use crate::grok_cli_chat::{self, Exchange};

    if !grok_cli_chat::supports(account) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Missing Grok CLI account credentials",
        );
    }
    let Some(delivery) = grok_cli_chat::bind(&state.storage, account).await else {
        return failure(StatusCode::BAD_GATEWAY, "Grok CLI proxy is unavailable");
    };
    let model = body["model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .unwrap_or(model);
    let Some(prepared) =
        crate::grok_cli_body::prepare(ChatFormat::Responses, body.clone(), model, "")
    else {
        return bad("Invalid Grok CLI request body");
    };
    let caller = crate::dashboard_session::authorization_owner(&state.storage, headers)
        .await
        .map(|owner| format!("translator/{}", URL_SAFE_NO_PAD.encode(owner)));
    let exchange = Exchange::new(
        delivery,
        account,
        &prepared.body,
        body,
        headers,
        caller.as_deref(),
    );
    match exchange
        .complete(&state.storage, account, &prepared.body)
        .await
    {
        Ok((response, renewed)) => {
            let secrets = std::iter::once(account)
                .chain(renewed.as_ref())
                .flat_map(crate::grok_cli_models::secrets)
                .chain(exchange.relay_secret())
                .map(|secret| secret.as_bytes().to_vec())
                .collect();
            upstream_reply(response, secrets).await
        }
        Err(error) => failure(error.status, error.message),
    }
}

async fn send_mimo_desktop(state: &Translator, account: &Value, source: &Value) -> Response {
    use crate::mimo_account::{self, Error, Request};

    let Some(PreparedChat { body, stream }) = prepare_mimo_desktop(source) else {
        return bad("Invalid MiMo Desktop request body");
    };
    let Some(settings) = state.storage.read_settings().await else {
        return failure(
            StatusCode::BAD_GATEWAY,
            "Could not load MiMo Desktop settings",
        );
    };
    match mimo_account::send(
        &state.storage,
        &settings,
        account,
        Request::Chat {
            body: &body,
            stream,
        },
    )
    .await
    {
        Ok(response) => {
            let mut secrets = selected_secrets(account);
            secrets.extend(
                account["providerSpecificData"]["mimoPassToken"]
                    .as_str()
                    .map(|pass| pass.as_bytes().to_vec()),
            );
            upstream_reply(response, secrets).await
        }
        Err(error) => {
            let status = match error {
                Error::Missing | Error::Rejected => StatusCode::UNAUTHORIZED,
                Error::Route | Error::Unavailable => StatusCode::BAD_GATEWAY,
            };
            failure(status, error.message())
        }
    }
}

fn prepare_mimo_desktop(source: &Value) -> Option<PreparedChat> {
    let mut body = source.clone();
    let fields = body.as_object_mut()?;
    let stream = source["stream"] != false;
    if !stream {
        fields.shift_remove("stream_options");
    }
    crate::chat::prepare_mimo_desktop(&mut body, Some(source));
    Some(PreparedChat { body, stream })
}

async fn send_antigravity(
    state: &Translator,
    account: &Value,
    model: &str,
    body: &Value,
    headers: &HeaderMap,
) -> Response {
    use crate::antigravity_transport::{complete, prepare, supports};

    if !supports(account) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Missing Antigravity OAuth account credentials",
        );
    }
    let Some(settings) = state.storage.read_settings().await else {
        return failure(
            StatusCode::BAD_GATEWAY,
            "Could not load Antigravity settings",
        );
    };
    let format = match super::source_format(body) {
        "claude" => ChatFormat::Messages,
        "openai-responses" => ChatFormat::Responses,
        _ => ChatFormat::Chat,
    };
    let Some(prepared) = prepare(format, body.clone(), model, "", |_| None) else {
        return bad("Invalid Antigravity request body");
    };
    match complete(
        &state.storage,
        &settings,
        account,
        body,
        &prepared,
        headers,
        &state.sessions,
    )
    .await
    {
        Ok((response, secrets)) => {
            let mut existing = selected_secrets(account);
            existing.extend(secrets.into_iter().map(String::into_bytes));
            upstream_reply(response, existing).await
        }
        Err(error) => failure(error.status, &error.message),
    }
}

async fn send_vertex(
    state: &Translator,
    account: &Value,
    provider: &str,
    model: &str,
    body: &Value,
) -> Response {
    let Some(settings) = state.storage.read_settings().await else {
        return failure(StatusCode::BAD_GATEWAY, "Could not load Vertex settings");
    };
    let stream = body["stream"] != false;
    let timeout = connect_timeout();
    let mut secrets = selected_secrets(account);
    if vertex_transport::requires_tunnel(&settings, account) {
        let egress = match vertex_transport::Protected::select(
            &state.storage,
            &settings,
            provider,
            model,
            stream,
            account,
        )
        .await
        {
            Ok(egress) => egress,
            Err(message) => return failure(StatusCode::BAD_GATEWAY, message),
        };
        egress.secrets(&mut secrets);
        return match egress.send(body, Default::default(), timeout).await {
            Ok(response) => upstream_reply(response, secrets).await,
            Err(message) => failure(StatusCode::BAD_GATEWAY, message),
        };
    }
    let client = &state.send.pooled;
    let exchange =
        match vertex_transport::Exchange::resolve(client, provider, model, stream, account).await {
            Ok(exchange) => exchange,
            Err(message) => return failure(StatusCode::INTERNAL_SERVER_ERROR, message),
        };
    exchange.secrets(&mut secrets);
    let mut upstream = send_vertex_direct(client, &exchange, body, timeout).await;
    if exchange.service_credential().is_some()
        && upstream.as_ref().is_ok_and(|response| {
            matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            )
        })
        && let Some(current) = vertex_retry_account(state, account).await
        && let Ok(exchange) =
            vertex_transport::Exchange::resolve(client, provider, model, stream, &current).await
        && let Some((token, expiry)) = exchange.service_credential()
        && persist_vertex_retry(state, &current, token, expiry).await
    {
        exchange.secrets(&mut secrets);
        drop(upstream);
        upstream = send_vertex_direct(client, &exchange, body, timeout).await;
    }
    match upstream {
        Ok(response) => upstream_reply(response, secrets).await,
        Err(message) => failure(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}

async fn send_vertex_direct(
    client: &reqwest::Client,
    exchange: &vertex_transport::Exchange,
    body: &Value,
    timeout: Duration,
) -> Result<reqwest::Response, &'static str> {
    let url = exchange
        .fixture_url()
        .unwrap_or_else(|| exchange.url().to_owned());
    let request = exchange.headers(client.post(url).json(body));
    tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| "fetch connect timeout")?
        .map_err(|_| "fetch failed")
}

async fn vertex_retry_account(state: &Translator, account: &Value) -> Option<Value> {
    let id = account["id"].as_str()?.to_owned();
    let current = state
        .storage
        .run(false, move |db| connections::by_id(db, &id))
        .await
        .ok()??;
    let settings = state.storage.read_settings().await?;
    can_retry_vertex(&current, account, &settings).then_some(current)
}

fn can_retry_vertex(current: &Value, selected: &Value, settings: &Value) -> bool {
    current["isActive"] != false
        && [
            "provider",
            "authType",
            "apiKey",
            "accessToken",
            "refreshToken",
            "expiresAt",
            "tokenExpiresAt",
            "providerSpecificData",
        ]
        .into_iter()
        .all(|field| current[field] == selected[field])
        && !vertex_transport::requires_tunnel(settings, current)
}

async fn persist_vertex_retry(
    state: &Translator,
    account: &Value,
    token: &str,
    expiry: i64,
) -> bool {
    let Some(id) = account["id"].as_str().map(str::to_owned) else {
        return false;
    };
    let selected = account.clone();
    let token = token.to_owned();
    state
        .storage
        .run(true, move |db| {
            let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(mut current) = connections::by_id(&transaction, &id)? else {
                return Ok(false);
            };
            let raw: Option<String> = transaction
                .query_row("SELECT data FROM settings WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .optional()?;
            let settings = match raw {
                Some(raw) => {
                    let Ok(settings) = serde_json::from_str(&raw) else {
                        return Ok(false);
                    };
                    settings
                }
                None => Value::Null,
            };
            if !can_retry_vertex(&current, &selected, &settings) {
                return Ok(false);
            }
            current["accessToken"] = json!(token);
            current["expiresAt"] = json!(expiry);
            current["updatedAt"] = json!(connections::now());
            connections::write(&transaction, &current)?;
            transaction.commit()?;
            Ok(true)
        })
        .await
        .unwrap_or(false)
}

async fn send_cline(
    state: &Translator,
    account: &Value,
    provider: &str,
    model: &str,
    body: &Value,
) -> Response {
    if !body["messages"].is_array()
        || !body["model"]
            .as_str()
            .is_some_and(|model| !model.is_empty())
    {
        return bad("Cline Send requires an OpenAI Chat request body");
    }
    let Some(settings) = state.storage.read_settings().await else {
        return failure(StatusCode::BAD_GATEWAY, "Could not load Cline settings");
    };
    let route = match crate::cline_transport::egress(&state.storage, &settings, account).await {
        Ok(route) => route,
        Err(message) => return failure(StatusCode::BAD_GATEWAY, message),
    };
    let PreparedChat { body, stream } = prepare_cline_body(body.clone(), model);
    let mut private = cline_secrets(account);
    if let Some(route) = route {
        private.extend(
            route
                .relay_secret()
                .map(|secret| secret.as_bytes().to_vec()),
        );
        return match crate::cline_transport::protected_chat(
            &state.storage,
            provider,
            &route,
            account,
            (&body, stream),
            HeaderMap::new(),
            connect_timeout(),
        )
        .await
        {
            Ok((response, renewed)) => {
                if let Some(current) = &renewed {
                    private.extend(cline_secrets(current));
                }
                upstream_reply(response, private).await
            }
            Err((status, message)) => failure(status, message),
        };
    }
    let mut upstream = cline_reply(provider, account, &body, stream).await;
    if account["authType"] == "oauth"
        && truthy(&account["refreshToken"])
        && upstream.as_ref().is_ok_and(|(response, _)| {
            matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            )
        })
        && let Ok(updated) = crate::provider_oauth::refresh_account(
            &state.storage,
            account,
            crate::provider_oauth::RefreshMode::Rejected,
        )
        .await
        && updated["isActive"] != false
        && updated["accessToken"] != account["accessToken"]
        && let Some(settings) = state.storage.read_settings().await
        && matches!(
            crate::cline_transport::egress(&state.storage, &settings, &updated).await,
            Ok(None)
        )
    {
        if let Ok((_, credential)) = upstream {
            private.push(credential);
        }
        private.extend(cline_secrets(&updated));
        upstream = cline_reply(provider, &updated, &body, stream).await;
    }
    match upstream {
        Ok((response, credential)) => {
            private.push(credential);
            upstream_reply(response, private).await
        }
        Err((status, message)) => failure(status, message),
    }
}

fn prepare_cline_body(mut body: Value, model: &str) -> PreparedChat {
    let stream = body["stream"] != false;
    if let Some(fields) = body.as_object_mut() {
        if !stream {
            fields.shift_remove("stream_options");
        }
        let lower = model.to_ascii_lowercase();
        if lower.contains("claude") {
            fields.shift_remove("temperature");
        }
        model_reasoning(&mut body, &lower);
    }
    PreparedChat { body, stream }
}

async fn cline_reply(
    provider: &str,
    account: &Value,
    body: &Value,
    stream: bool,
) -> Result<(reqwest::Response, Vec<u8>), (StatusCode, &'static str)> {
    let timeout = connect_timeout();
    let mut attempts = 0;
    loop {
        let result =
            crate::cline_transport::translator_reply(provider, account, body, stream, timeout)
                .await?;
        let (limit, delay) = retry_policy(provider, result.0.status().as_u16());
        if attempts >= limit {
            return Ok(result);
        }
        attempts += 1;
        drop(result);
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
}

fn cline_secrets(account: &Value) -> Vec<Vec<u8>> {
    let mut private = selected_secrets(account);
    if let Some(token) = account["accessToken"].as_str() {
        let trimmed = crate::provider_index::js_trim(token);
        if !trimmed.is_empty() && trimmed != token {
            private.push(trimmed.as_bytes().to_vec());
        }
    }
    private
}

async fn upstream_reply(response: reqwest::Response, mut secrets: Vec<Vec<u8>>) -> Response {
    let status = response.status();
    if status.is_success() {
        secrets.sort_unstable_by(|left, right| {
            right.len().cmp(&left.len()).then_with(|| left.cmp(right))
        });
        secrets.dedup();
        return success(response, secrets);
    }
    upstream_error_reply(status, response.text().await.unwrap_or_default(), &secrets)
}

fn upstream_error_reply(status: StatusCode, details: String, secrets: &[Vec<u8>]) -> Response {
    reply(
        status,
        json!({
            "success":false,
            "error":format!("Provider error: {}", status.as_u16()),
            "details":scrub_text(details, secrets),
        }),
    )
}

fn scrub_text(text: String, secrets: &[Vec<u8>]) -> String {
    let mut secrets: Vec<_> = secrets
        .iter()
        .filter_map(|secret| std::str::from_utf8(secret).ok())
        .filter(|secret| !secret.is_empty())
        .collect();
    secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets
        .into_iter()
        .fold(text, |text, secret| text.replace(secret, "[redacted]"))
}

fn supports_account(provider: &str, account: &Value) -> bool {
    if provider == "xai" && account["authType"] == "oauth" {
        return [&account["accessToken"], &account["refreshToken"]]
            .iter()
            .any(|value| value.as_str().is_some_and(|token| !token.is_empty()));
    }
    if crate::opencode_chat::is_provider(provider) {
        return crate::opencode_chat::supports(account);
    }
    if provider == "nous" {
        return crate::provider_oauth::nous_connection_compatible(account)
            && (crate::provider_oauth::nous_inference_token(account).is_some()
                || crate::provider_oauth::nous_refresh_supported(account));
    }
    if provider == "kimchi" {
        return crate::kimchi_chat::supports(account);
    }
    if provider == "glm" {
        return if account["authType"] == "oauth" {
            account["providerSpecificData"]["authMethod"] == "cli_poll"
                && account["accessToken"]
                    .as_str()
                    .is_some_and(|token| !token.is_empty())
        } else {
            matches!(account["authType"].as_str(), Some("apikey" | "api_key"))
                && account["apiKey"]
                    .as_str()
                    .is_some_and(|token| !token.is_empty())
        };
    }
    if provider == "kilocode" {
        return [&account["apiKey"], &account["accessToken"]]
            .iter()
            .any(|value| value.as_str().is_some_and(|token| !token.is_empty()));
    }
    if provider == "iflow" {
        return crate::iflow_transport::supports(account)
            || crate::iflow_transport::can_refresh(account);
    }
    if provider == "ollama-local" {
        return crate::ollama_local::url(account).is_some();
    }
    if provider == "ollama" {
        return crate::ollama_local::cloud_credentials(account);
    }
    if provider == "gitlab" {
        return crate::gitlab_transport::endpoint(account).is_some();
    }
    if provider == "azure" {
        return matches!(account["authType"].as_str(), Some("apikey" | "api_key"))
            && crate::azure_transport::credential(account).is_some();
    }
    if provider == "xiaomi-mimo" && crate::provider_imports::xiaomi_cloud_key(account).is_some() {
        return true;
    }
    if matches!(provider, "codex" | "gemini-cli") {
        return crate::provider_oauth::supports_chat_credentials(provider, account);
    }
    if static_chat::codebuddy::is_provider(provider) {
        return static_chat::codebuddy::supports_account(account);
    }
    if provider == "kimi" && account["authType"] == "oauth" {
        return !truthy(&account["apiKey"])
            && account["accessToken"]
                .as_str()
                .is_some_and(|token| !token.is_empty())
            && crate::provider_oauth::kimi_portal::saved_device_id(account).is_some();
    }
    matches!(account["authType"].as_str(), Some("apikey" | "api_key"))
        || provider == "muse"
            && account["authType"] == "oauth"
            && static_chat::muse_subscription(account)
        || (provider == "gemini"
            || provider.starts_with("anthropic-compatible-")
            || provider.starts_with("openai-compatible-"))
            && account["accessToken"]
                .as_str()
                .is_some_and(|token| !token.is_empty())
}

fn connect_timeout() -> Duration {
    std::env::var("FETCH_CONNECT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map_or(Duration::from_secs(60), Duration::from_millis)
}

fn codex_plan(mut body: Value, model: &str, account: &Value, session: String) -> Option<Plan> {
    let requested = body["model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .unwrap_or(model)
        .to_owned();
    let exchange = codex_executor::finish(&mut body, &requested, account, session, false)?;
    Some(Plan {
        destination: exchange.url(),
        transport: Transport::Codex(exchange),
        stream: body["stream"] != false,
        body,
        timeout: connect_timeout(),
    })
}

fn is_minimax(provider: &str) -> bool {
    matches!(
        provider,
        "minimax" | "minimax-cn" | "minimax-code" | "minimax-code-global"
    )
}

fn minimax_account(provider: &str, account: &Value) -> bool {
    if matches!(provider, "minimax-code" | "minimax-code-global") {
        return account["authType"] == "mcode-lease"
            || account["authType"] == "oauth"
                && (truthy(&account["accessToken"]) || truthy(&account["refreshToken"]));
    }
    matches!(account["authType"].as_str(), Some("apikey" | "api_key"))
        && (truthy(&account["apiKey"]) || truthy(&account["accessToken"]))
}

fn minimax_plan(provider: &str, model: &str, body: Value, account: &Value) -> Option<Plan> {
    body.as_object()?;
    let endpoint = static_chat::default_endpoint(provider)?;
    let destination = endpoint.url(account)?;
    let mut prepared = PreparedChat {
        stream: body["stream"] != false,
        body,
    };
    endpoint.prepare(provider, model, &mut prepared);
    Some(Plan {
        destination,
        transport: Transport::Static(endpoint),
        stream: prepared.stream,
        body: prepared.body,
        timeout: endpoint.timeout(connect_timeout()),
    })
}

fn commandcode_plan(
    body: &Value,
    model: &str,
    account: &Value,
    headers: &HeaderMap,
) -> Option<Plan> {
    use crate::commandcode_transport as commandcode;

    let mut body = if body["params"].is_object() {
        body.clone()
    } else {
        let prepared = commandcode::prepare_chat(body.clone(), model, false)?;
        commandcode::prepare(&prepared.body, body, headers, model, account).ok()?
    };
    body["params"]["stream"] = json!(true);
    Some(Plan {
        destination: commandcode::url().to_owned(),
        transport: Transport::CommandCode,
        stream: true,
        body,
        timeout: connect_timeout(),
    })
}

fn plan(provider: &str, model: &str, mut body: Value, account: &Value) -> Option<Plan> {
    if !body.is_object() {
        return None;
    }
    let stream = body["stream"] != false;
    let details = &account["providerSpecificData"];
    let default_timeout = connect_timeout();
    if crate::opencode_chat::is_provider(provider) {
        let endpoint = crate::opencode_chat::select(provider, model, ChatFormat::Chat)?;
        let mut prepared = PreparedChat {
            body,
            stream: stream || endpoint.force_stream(),
        };
        endpoint.prepare(&mut prepared)?;
        return Some(Plan {
            destination: endpoint.url().to_owned(),
            transport: Transport::OpenCode(endpoint),
            stream: prepared.stream,
            body: prepared.body,
            timeout: default_timeout,
        });
    }
    let specialized = match provider {
        "kilocode" => Some((
            crate::provider_imports::KILOCODE_CHAT_ENDPOINT.to_owned(),
            Transport::KiloCode,
        )),
        "azure" => Some((
            crate::azure_transport::url(account, model),
            Transport::Azure,
        )),
        "gitlab" => Some((
            crate::gitlab_transport::endpoint(account)?,
            Transport::Gitlab,
        )),
        "iflow" => {
            let mut request = PreparedChat { body, stream };
            crate::iflow_transport::prepare(&mut request);
            body = request.body;
            Some((
                crate::iflow_transport::ENDPOINT.to_owned(),
                Transport::IFlow,
            ))
        }
        "ollama-local" => Some((crate::ollama_local::url(account)?, Transport::Ollama)),
        "ollama" => Some((
            crate::ollama_local::CLOUD_CHAT.to_owned(),
            Transport::Ollama,
        )),
        _ => None,
    };
    if let Some((destination, transport)) = specialized {
        return Some(Plan {
            destination,
            transport,
            stream,
            body,
            timeout: default_timeout,
        });
    }
    if provider == "gemini-cli" {
        if !truthy(&body["request"]) || !truthy(&body["model"]) {
            let project = crate::google_code_assist::saved_project(account).or_else(|| {
                body["project"]
                    .as_str()
                    .filter(|project| !project.is_empty())
            })?;
            body = json!({"project":project,"model":model,"request":body});
        }
        return Some(Plan {
            destination: crate::gemini_cli_transport::url(stream),
            transport: Transport::GeminiCli,
            stream,
            body,
            timeout: default_timeout,
        });
    }
    let (destination, transport, timeout) = if provider.starts_with("openai-compatible-") {
        let base = crate::upstream::connection_base(account, Some(OPENAI_BASE))?;
        let responses = match details["apiType"].as_str() {
            Some("responses") => true,
            Some("chat") => false,
            _ => provider.contains("responses"),
        };
        let suffix = if responses {
            "/responses"
        } else {
            "/chat/completions"
        };
        let destination =
            crate::upstream::node_url(base, suffix, crate::upstream::OPENAI_NODE_PATHS)?
                .to_string();
        if let Some(tags) = crate::provider_oauth::nous_portal::compatible_tags(
            provider,
            &destination,
            account,
            &body,
        ) {
            body["tags"] = Value::Array(tags);
        }
        if body["response_format"]["type"] == "json_schema"
            && truthy(&body["response_format"]["json_schema"]["schema"])
            && !body["messages"].is_array()
        {
            body["messages"] = json!([]);
        }
        apply_schema_fallback(&mut body)?;
        (destination, Transport::OpenAi, default_timeout)
    } else if provider.starts_with("anthropic-compatible-") {
        let base = crate::upstream::connection_base(account, Some(DEFAULT_ANTHROPIC_BASE))?;
        let endpoint =
            crate::upstream::node_url(base, "/messages", crate::upstream::ANTHROPIC_NODE_PATHS)?;
        (
            endpoint.to_string(),
            Transport::Anthropic {
                compatible: true,
                official: crate::upstream::anthropic_official(&endpoint),
            },
            default_timeout,
        )
    } else if provider == "anthropic" {
        (
            format!("{DEFAULT_ANTHROPIC_BASE}/messages"),
            Transport::Anthropic {
                compatible: false,
                official: true,
            },
            default_timeout,
        )
    } else if provider == "gemini" {
        (
            gemini::destination(model, stream),
            Transport::Gemini,
            default_timeout,
        )
    } else {
        let endpoint = static_chat::default_endpoint(provider)?;
        let destination = endpoint.url(account)?;
        (
            destination,
            Transport::Static(endpoint),
            endpoint.timeout(default_timeout),
        )
    };

    let fields = body.as_object_mut()?;
    if !stream {
        fields.shift_remove("stream_options");
    }
    let lower = model.to_ascii_lowercase();
    if lower.contains("claude") {
        fields.shift_remove("temperature");
    }

    let mut prepared = PreparedChat { body, stream };
    if let Transport::Static(endpoint) = transport {
        let deepseek_alias = provider == "deepseek"
            && matches!(model, "deepseek-v4-pro-max" | "deepseek-v4-pro-none");
        let previous_thinking =
            deepseek_alias.then(|| prepared.body["extra_body"]["thinking"].clone());
        endpoint.prepare(provider, model, &mut prepared);
        if deepseek_alias {
            prepared.body["model"] = json!("deepseek-v4-pro");
            if let Some(Value::Object(mut previous)) = previous_thinking {
                previous.insert(
                    "type".into(),
                    prepared.body["extra_body"]["thinking"]["type"].clone(),
                );
                prepared.body["extra_body"]["thinking"] = Value::Object(previous);
            }
        }
        if !endpoint.has_reasoning_scope() {
            model_reasoning(&mut prepared.body, &lower);
        }
    } else {
        model_reasoning(&mut prepared.body, &lower);
    }
    Some(Plan {
        destination,
        transport,
        stream,
        body: prepared.body,
        timeout,
    })
}

pub(super) fn codex_preview(
    state: &Translator,
    body: &Value,
    model: &str,
    account: &Value,
    headers: &HeaderMap,
) -> Option<Value> {
    if !supports_account("codex", account) {
        return None;
    }
    let source = super::source_format(body);
    let mut request = if source == "openai-responses" {
        body.clone()
    } else {
        let chat = super::intermediate(body, "openai", model, "codex")?;
        let mut responses = crate::responses_request::from_chat(chat, model)?;
        for field in [
            "_compact",
            "_autoCodexWebSearch",
            "prompt_cache_key",
            "reasoning",
            "reasoning_effort",
        ] {
            if let Some(value) = body.get(field) {
                responses[field] = value.clone();
            }
        }
        responses
    };
    request["model"] = json!(model);
    let session = state.sessions.pick(body, account, headers, None);
    let mut plan = codex_plan(request, model, account, session)?;
    if matches!(&plan.transport, Transport::Codex(exchange) if exchange.compact) {
        plan.body["_compact"] = json!(true);
    }
    wire_preview(&state.send, plan, model, account)
}

pub(super) fn gemini_cli_preview(
    state: &Translator,
    body: &Value,
    model: &str,
    account: &Value,
    headers: &HeaderMap,
) -> Result<Value, &'static str> {
    use crate::gemini_cli_transport as cli;

    if account["authType"] != "oauth" || account["accessToken"].as_str().is_none_or(str::is_empty) {
        return Err("Gemini CLI preview requires a saved OAuth access token");
    }
    if crate::google_code_assist::saved_project(account).is_none() {
        return Err("Missing Gemini CLI project configuration");
    }
    if super::source_format(body) != "openai" {
        return Err("Expected an OpenAI request for Gemini CLI preview");
    }
    let stream = body["stream"] != false;
    let mut prepared = crate::gemini_request::prepare(
        ChatFormat::Chat,
        body.clone(),
        model,
        "",
        |_| None,
        cli::signature(),
    )
    .ok_or("Unsupported Gemini CLI preview request")?;
    let session = state.sessions.pick(body, account, headers, None);
    cli::envelope(&mut prepared.body, account, session)
        .ok_or("Could not prepare the Gemini CLI request envelope")?;
    let destination = cli::url(stream);
    let request = cli::headers(
        state.send.pooled.post(&destination),
        "redacted",
        model,
        stream,
    )
    .build()
    .map_err(|_| "Invalid Gemini CLI preview headers")?;
    let value = |name| {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .ok_or("Invalid Gemini CLI preview headers")
    };
    Ok(json!({"url":destination,"headers":{
        "Content-Type":"application/json", "Authorization":"[redacted]",
        "User-Agent":value("user-agent")?, "X-Goog-Api-Client":value("x-goog-api-client")?,
        "Accept":value("accept")?,
    },"body":prepared.body}))
}

fn is_web_chat_provider(provider: &str) -> bool {
    matches!(
        provider,
        "grok-web" | "deepseek-web" | "perplexity-web" | "gemini-web" | "chatgpt-web" | "muse-web"
    )
}

fn web_chat_preview(provider: &str, account: &Value, body: Value) -> Option<Value> {
    if body["messages"].as_array().is_none_or(Vec::is_empty) {
        return None;
    }
    let (url, auth, media) = match provider {
        "grok-web" if crate::grok_web::supports(account) => (
            crate::grok_web::CONVERSATIONS.to_owned(),
            "Cookie",
            Some("application/json"),
        ),
        "deepseek-web" if crate::deepseek_web::supports(account) => (
            crate::deepseek_web::completion_url(),
            "Authorization",
            Some("application/json"),
        ),
        "perplexity-web" => (
            crate::perplexity_web_chat::ENDPOINT.to_owned(),
            crate::perplexity_web_chat::preview_auth_header(account)?,
            Some("application/json"),
        ),
        "gemini-web" if crate::gemini_web::supports(account) => {
            (crate::gemini_web::APP.to_owned(), "Cookie", None)
        }
        "chatgpt-web" if crate::chatgpt_web::supports(account) => {
            (crate::chatgpt_web::ORIGIN.to_owned(), "Cookie", None)
        }
        "muse-web" if crate::muse_web::supports(account) => {
            (crate::muse_web::THREAD.to_owned(), "Cookie", None)
        }
        "cursor" if crate::cursor_chat::supports_account(account) => (
            crate::cursor_chat::ENDPOINT.to_owned(),
            "Authorization",
            Some(crate::cursor_chat::MEDIA),
        ),
        _ => return None,
    };
    let mut headers = json!({});
    headers[auth] = json!("[redacted]");
    if let Some(media) = media {
        headers["Content-Type"] = json!(media);
    }
    Some(json!({"url":url,"headers":headers,"body":body}))
}

pub(super) fn supports_chat_preview(provider: &str) -> bool {
    matches!(
        provider,
        "trae"
            | "zed"
            | "windsurf"
            | "azure"
            | "gitlab"
            | "iflow"
            | "ollama"
            | "ollama-local"
            | "kilocode"
            | "claude"
            | "nous"
            | "glm"
            | "kimchi"
            | "xai"
            | "cursor"
    ) || crate::cline_transport::is_provider(provider)
        || is_web_chat_provider(provider)
        || crate::qoder_models::supports(provider)
        || crate::bedrock_transport::is_provider(provider)
        || is_minimax(provider)
        || crate::opencode_chat::is_provider(provider)
}

pub(super) fn chat_preview(
    state: &Translator,
    provider: &str,
    model: &str,
    body: &Value,
    account: &Value,
) -> Option<Value> {
    let clients = &state.send;
    let mut body = super::intermediate(body, "openai", model, provider)?;
    body.as_object_mut()?.shift_remove("provider");
    body["model"] = json!(model);
    if is_web_chat_provider(provider) || provider == "cursor" {
        return web_chat_preview(provider, account, body);
    }
    if crate::opencode_chat::is_provider(provider) && supports_account(provider, account) {
        let endpoint = crate::opencode_chat::select(provider, model, ChatFormat::Chat)?;
        let request = match endpoint.wire {
            ChatFormat::Chat => body,
            ChatFormat::Messages => {
                crate::anthropic_request::prepare_for_provider(
                    ChatFormat::Chat,
                    body,
                    model,
                    "",
                    false,
                )?
                .body
            }
            ChatFormat::Responses => crate::responses_request::from_chat(body, model)?,
        };
        let plan = plan(provider, model, request, account)?;
        return wire_preview(clients, plan, model, account);
    }
    if provider == "glm" && supports_account(provider, account) {
        let endpoint = static_chat::default_endpoint(provider)?;
        let prepared = endpoint.prepare_messages(ChatFormat::Chat, body, provider, model, "")?;
        let plan = plan(provider, model, prepared.body, account)?;
        return wire_preview(clients, plan, model, account);
    }
    if provider == "claude" {
        let prepared = crate::anthropic_request::prepare_for_provider(
            ChatFormat::Chat,
            body,
            model,
            "",
            true,
        )?;
        return crate::claude_transport::preview(account, model, prepared);
    }
    if crate::qoder_models::supports(provider) {
        return crate::qoder_chat::preview(account, body);
    }
    if is_minimax(provider) {
        if !minimax_account(provider, account) {
            return None;
        }
        let endpoint = static_chat::default_endpoint(provider)?;
        let prepared = endpoint.prepare_messages(ChatFormat::Chat, body, provider, model, "")?;
        let plan = minimax_plan(provider, model, prepared.body, account)?;
        return wire_preview(clients, plan, model, account);
    }
    if let Some(format) = crate::bedrock_transport::wire(provider) {
        let stream = body["stream"] != false;
        let body = if format == ChatFormat::Messages {
            crate::anthropic_request::prepare_for_provider(
                ChatFormat::Chat,
                body,
                model,
                "",
                false,
            )?
            .body
        } else {
            body
        };
        return crate::bedrock_transport::preview(
            provider,
            model,
            account,
            PreparedChat { body, stream },
        );
    }
    if matches!(
        provider,
        "azure"
            | "gitlab"
            | "iflow"
            | "ollama"
            | "ollama-local"
            | "kilocode"
            | "nous"
            | "kimchi"
            | "xai"
    ) && supports_account(provider, account)
    {
        if matches!(provider, "ollama" | "ollama-local") {
            let stream = body["stream"] != false;
            body = crate::ollama_local::prepare(body, model, stream)?;
        }
        let mut plan = plan(provider, model, body, account)?;
        if provider == "kimchi" {
            crate::kimchi_chat::prepare(&state.storage, account, model, &mut plan.body);
        }
        return wire_preview(clients, plan, model, account);
    }
    let stream = body["stream"] != false;
    let (destination, request) = match provider {
        "trae" if crate::trae_chat::supports(account) => {
            let destination = crate::trae_chat::ENDPOINT.to_owned();
            let request = crate::trae_chat::preview_headers(
                clients.pooled.post(&destination),
                account,
                stream,
            );
            (destination, request)
        }
        "zed" if crate::zed_transport::supports(account) => {
            let destination = crate::zed_transport::completion_url();
            let request =
                crate::zed_transport::completion_headers(clients.pooled.post(&destination))
                    .header(header::AUTHORIZATION, "[redacted]")
                    .header(header::CONTENT_TYPE, "application/json");
            (destination, request)
        }
        "windsurf" if crate::windsurf_chat::supports(account) => {
            let destination = crate::windsurf_chat::destination(account)?;
            let request = crate::windsurf_chat::headers(clients.pooled.post(&destination))
                .header(header::AUTHORIZATION, "[redacted]");
            (destination, request)
        }
        _ if crate::cline_transport::typed_account(provider, account) => {
            let destination = crate::cline_transport::url();
            body = prepare_cline_body(body, model).body;
            let mut request = crate::cline_transport::oauth_headers(
                clients.pooled.post(&destination),
                Some("redacted"),
            )
            .header(header::CONTENT_TYPE, "application/json");
            if stream {
                request = request.header(header::ACCEPT, "text/event-stream");
            }
            (destination, request)
        }
        _ => return None,
    };
    let request = request.build().ok()?;
    render_wire_preview(request.headers(), &destination, body)
}

pub(super) fn grok_cli_preview(
    clients: &Clients,
    body: &Value,
    model: &str,
    account: &Value,
) -> Option<Value> {
    if !crate::grok_cli_chat::supports(account) {
        return None;
    }
    let format = if super::source_format(body) == "openai-responses" {
        ChatFormat::Responses
    } else {
        ChatFormat::Chat
    };
    let prepared = crate::grok_cli_body::prepare(format, body.clone(), model, "")?;
    let destination = crate::grok_cli_chat::ENDPOINT;
    let request = crate::grok_cli_http::identity_headers(
        clients.pooled.post(destination),
        "redacted",
        None,
        None,
    )
    .header(header::CONTENT_TYPE, "application/json")
    .header(header::ACCEPT, "text/event-stream")
    .header("x-grok-model-override", prepared.body["model"].as_str()?)
    .build()
    .ok()?;
    render_wire_preview(request.headers(), destination, prepared.body)
}

pub(super) fn commandcode_preview(
    clients: &Clients,
    body: &Value,
    model: &str,
    account: &Value,
    headers: &HeaderMap,
) -> Option<Value> {
    if !crate::commandcode_transport::supports(account) {
        return None;
    }
    let plan = commandcode_plan(body, model, account, headers)?;
    wire_preview(clients, plan, model, account)
}

pub(super) fn github_preview(
    clients: &Clients,
    body: &Value,
    model: &str,
    account: &Value,
) -> Option<Value> {
    github_executor::token(account)?;
    let mut body = super::intermediate(body, "openai", model, "github")?;
    body.as_object_mut()?.shift_remove("provider");
    let stream = body["stream"] != false;
    if !github_claude::matches_model(model) {
        github_executor::prepare(&mut body, model)?;
    }
    wire_preview(
        clients,
        Plan {
            destination: github_executor::url().to_owned(),
            transport: Transport::Github(GithubRoute::Chat),
            stream,
            body,
            timeout: connect_timeout(),
        },
        model,
        account,
    )
}

pub(super) async fn kiro_preview(
    state: &Translator,
    body: &Value,
    model: &str,
    account: &Value,
    headers: &HeaderMap,
) -> Result<Value, &'static str> {
    use crate::kiro_chat::{self, Connection};

    let settings = state
        .storage
        .read_settings()
        .await
        .ok_or("Could not load Kiro settings")?;
    let protected = if account["authType"] == "oauth" {
        kiro_chat::OAuthRoute::select(&state.storage, &settings, account)
            .await
            .map_err(|_| "Kiro OAuth route is unavailable")?
            .protected()
    } else {
        false
    };
    let connection = Connection::api_key(account)
        .or_else(|| Connection::candidate(account, protected))
        .ok_or("Missing Kiro account credentials")?;
    let mut body = body.clone();
    body.as_object_mut()
        .ok_or("Invalid Kiro preview request")?
        .shift_remove("provider");
    kiro_chat::strip_model_media(&mut body, model);
    let body =
        kiro_chat::prepare(&body, model, headers, None).ok_or("Invalid Kiro preview request")?;
    let destination = connection.upstream_url();
    let request = connection
        .headers(
            state
                .send
                .pooled
                .post(&destination)
                .header(header::CONTENT_TYPE, "application/json"),
            false,
        )
        .build()
        .map_err(|_| "Invalid Kiro preview headers")?;
    let wire = connection.body(&body, None).into_owned();
    render_wire_preview(request.headers(), &destination, wire).ok_or("Invalid Kiro preview headers")
}

pub(super) fn muse_preview(body: &Value, model: &str, account: &Value) -> Option<Value> {
    if !supports_account("muse", account) || super::source_format(body) != "openai" {
        return None;
    }
    let request = plan(
        "muse",
        model,
        super::intermediate(body, "openai", model, "muse")?,
        account,
    )?;
    let mut headers = json!({"Content-Type":"application/json", "Authorization":"[redacted]"});
    if !truthy(&account["apiKey"]) && truthy(&account["accessToken"]) {
        headers["x-api-version"] = json!("1.0.0");
    }
    if request.stream {
        headers["Accept"] = json!("text/event-stream");
    }
    Some(json!({"url":request.destination,"headers":headers,"body":request.body}))
}

pub(super) fn kimi_preview(
    clients: &Clients,
    body: &Value,
    model: &str,
    account: &Value,
) -> Option<Value> {
    static_preview(clients, body, "kimi", model, account)
}

pub(super) fn mimo_preview(
    clients: &Clients,
    body: &Value,
    model: &str,
    account: &Value,
) -> Option<Value> {
    if crate::mimo_account::has_session(account) && crate::mimo_account::account_model(model) {
        let mut request = super::intermediate(body, "openai", model, "xiaomi-mimo")?;
        request.as_object_mut()?.shift_remove("provider");
        if !truthy(&request["model"]) {
            request["model"] = json!(model);
        }
        let prepared = prepare_mimo_desktop(&request)?;
        return Some(crate::mimo_account::chat_preview(
            account,
            prepared.body,
            prepared.stream,
        ));
    }
    static_preview(clients, body, "xiaomi-mimo", model, account)
}

pub(super) fn compatible_preview(
    clients: &Clients,
    body: &Value,
    provider: &str,
    model: &str,
    account: &Value,
) -> Option<Value> {
    let responses = match account["providerSpecificData"]["apiType"].as_str() {
        Some("responses") => true,
        Some("chat") => false,
        _ => provider.contains("responses"),
    };
    let mut body = super::intermediate(body, "openai", model, provider)?;
    if responses {
        if body["response_format"]["type"] == "json_schema" && !body["messages"].is_array() {
            body["messages"] = json!([]);
        }
        apply_schema_fallback(&mut body)?;
        body = crate::responses_request::prepare(ChatFormat::Chat, body, model, "")?.body;
    }
    let plan = plan(provider, model, body, account)?;
    wire_preview(clients, plan, model, account)
}

pub(super) fn static_preview(
    clients: &Clients,
    body: &Value,
    provider: &str,
    model: &str,
    account: &Value,
) -> Option<Value> {
    if !supports_account(provider, account) {
        return None;
    }
    let endpoint = static_chat::default_endpoint(provider)?;
    let body = super::intermediate(body, "openai", model, provider)?;
    let body = match endpoint.wire {
        ChatFormat::Chat => body,
        ChatFormat::Messages => {
            endpoint
                .prepare_messages(ChatFormat::Chat, body, provider, model, "")?
                .body
        }
        ChatFormat::Responses => {
            crate::responses_request::prepare(ChatFormat::Chat, body, model, "")?.body
        }
    };
    let plan = plan(provider, model, body, account)?;
    wire_preview(clients, plan, model, account)
}

fn wire_preview(clients: &Clients, plan: Plan, model: &str, account: &Value) -> Option<Value> {
    let request = with_auth(
        clients
            .pooled
            .post(&plan.destination)
            .header(header::CONTENT_TYPE, "application/json"),
        &plan,
        model,
        account,
    )
    .build()
    .ok()?;
    render_wire_preview(request.headers(), &plan.destination, plan.body)
}

fn render_wire_preview(wire_headers: &HeaderMap, destination: &str, body: Value) -> Option<Value> {
    let mut headers = serde_json::Map::new();
    for (name, value) in wire_headers {
        let name = name.as_str();
        let (label, public) = match name {
            "content-type" => ("Content-Type", true),
            "accept" => ("Accept", true),
            "anthropic-version" => ("Anthropic-Version", true),
            "anthropic-beta" => ("Anthropic-Beta", true),
            "user-agent" => ("User-Agent", true),
            "authorization" => ("Authorization", false),
            "api-key" => ("api-key", false),
            "originator" | "version" | "x-openai-internal-codex-responses-lite" => (name, true),
            "x-xai-token-auth" | "x-grok-client-mode" | "x-grok-model-override" => (name, true),
            "x-trae-client-type"
            | "x-preferenced-language"
            | "x-user-region"
            | "referer"
            | "x-grpc-web"
            | "x-zed-client-supports-status-messages"
            | "x-zed-client-supports-stream-ended-request-completion-status"
            | "http-referer"
            | "x-title"
            | "x-platform"
            | "x-platform-version"
            | "x-client-type"
            | "x-client-version"
            | "x-core-version"
            | "x-is-multiroot" => (name, true),
            "tokentype" => ("TokenType", true),
            "copilot-integration-id"
            | "editor-version"
            | "editor-plugin-version"
            | "openai-intent"
            | "x-github-api-version"
            | "x-vscode-user-agent-library-version"
            | "x-initiator" => (name, true),
            "x-msh-platform" => ("X-Msh-Platform", true),
            "x-msh-version" => ("X-Msh-Version", true),
            "x-msh-device-name" => ("X-Msh-Device-Name", false),
            "x-msh-device-model" => ("X-Msh-Device-Model", false),
            "x-msh-device-id" => ("X-Msh-Device-Id", false),
            _ => (name, false),
        };
        headers.insert(
            label.into(),
            json!(if public {
                value.to_str().ok()?
            } else {
                "[redacted]"
            }),
        );
    }
    Some(json!({"url":super::redact_url(destination),"headers":headers,"body":body}))
}

pub(super) fn anthropic_preview(
    body: &Value,
    provider: &str,
    model: &str,
    account: &Value,
) -> Option<Value> {
    let body = super::intermediate(body, "openai", model, provider)?;
    let prepared = crate::anthropic_request::prepare_for_provider(
        ChatFormat::Chat,
        body,
        model,
        "",
        provider == "anthropic",
    )?;
    let plan = plan(provider, model, prepared.body, account)?;
    let Transport::Anthropic {
        compatible,
        official,
    } = plan.transport
    else {
        return None;
    };
    let mut headers = json!({"Content-Type":"application/json", "anthropic-version":"2023-06-01"});
    if compatible {
        if truthy(&account["apiKey"]) {
            headers["x-api-key"] = json!("[redacted]");
            if !official {
                headers["Authorization"] = json!("[redacted]");
            }
        } else if truthy(&account["accessToken"]) {
            headers["Authorization"] = json!("[redacted]");
        }
    } else {
        headers["Anthropic-Beta"] = json!(STANDARD_BETA);
        if truthy(&account["apiKey"]) || truthy(&account["accessToken"]) {
            headers["x-api-key"] = json!("[redacted]");
        }
    }
    if plan.stream {
        headers["Accept"] = json!("text/event-stream");
    }
    Some(json!({"url":super::redact_url(&plan.destination),"headers":headers,"body":plan.body}))
}

fn model_reasoning(body: &mut Value, model: &str) {
    let all = !model.starts_with("kimi-") && model.contains("deepseek");
    let tools = model.starts_with("kimi-");
    if !(all || tools) {
        return;
    }
    if let Some(messages) = body["messages"].as_array_mut() {
        for message in messages {
            if message["role"] == "assistant"
                && message["reasoning_content"]
                    .as_str()
                    .is_none_or(str::is_empty)
                && (all
                    || message["tool_calls"]
                        .as_array()
                        .is_some_and(|values| !values.is_empty()))
            {
                message["reasoning_content"] = json!(" ");
            }
        }
    }
}

fn fixture_destination(provider: &str, destination: &str) -> Option<String> {
    if let Some(fixture) = static_chat::fixture_url(provider) {
        return Some(fixture);
    }
    if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() != Ok("1") {
        return None;
    }
    let parsed = Url::parse(destination).ok()?;
    let prefix = match provider {
        "anthropic" => "anthropic",
        "gemini" => "gemini",
        _ => return None,
    };
    let query = parsed
        .query()
        .map_or_else(String::new, |query| format!("?{query}"));
    Some(format!(
        "http://127.0.0.1:52989/{prefix}{}{query}",
        parsed.path()
    ))
}

fn with_auth(
    mut request: reqwest::RequestBuilder,
    plan: &Plan,
    model: &str,
    account: &Value,
) -> reqwest::RequestBuilder {
    let key = account["apiKey"].as_str().filter(|value| !value.is_empty());
    let access = account["accessToken"]
        .as_str()
        .filter(|value| !value.is_empty());
    let credential = if account["provider"] == "nous" {
        crate::provider_oauth::nous_inference_token(account).unwrap_or("undefined")
    } else if account["provider"] == "xai" && account["authType"] == "oauth" {
        access.unwrap_or("undefined")
    } else {
        key.or(access).unwrap_or("undefined")
    };
    request = match &plan.transport {
        Transport::OpenAi => request.bearer_auth(credential),
        Transport::Static(endpoint) => endpoint.auth(request, credential, account),
        Transport::Anthropic {
            compatible,
            official,
        } => {
            let request = request.header("anthropic-version", "2023-06-01");
            if !compatible {
                request
                    .header("x-api-key", credential)
                    .header("anthropic-beta", STANDARD_BETA)
            } else {
                let mut request = if let Some(key) = key {
                    let request = request.header("x-api-key", key);
                    if *official {
                        request
                    } else {
                        request.bearer_auth(key)
                    }
                } else if let Some(token) = access {
                    request.bearer_auth(token)
                } else {
                    request
                };
                if let Some(beta) = crate::chat::anthropic_beta(model, &plan.body, "", *official) {
                    request = request.header("anthropic-beta", beta);
                }
                request
            }
        }
        Transport::Gemini => match gemini::credential(account) {
            Some(("x-goog-api-key", key)) => request.header("x-goog-api-key", key),
            Some((_, token)) => request.bearer_auth(token),
            None => request,
        },
        Transport::GeminiCli => {
            return crate::gemini_cli_transport::headers(
                request,
                access.unwrap_or_default(),
                model,
                plan.stream,
            );
        }
        Transport::Azure => crate::azure_transport::headers(request, account),
        Transport::IFlow => return crate::iflow_transport::headers(request, account, plan.stream),
        Transport::KiloCode => {
            crate::provider_imports::kilocode_chat_headers(request, credential, account)
        }
        Transport::CommandCode => {
            return crate::commandcode_transport::headers(request, account, &plan.body);
        }
        Transport::Ollama => {
            return crate::ollama_local::headers(
                request,
                account,
                &Value::Null,
                account["provider"].as_str().unwrap_or("ollama-local"),
                plan.stream,
            );
        }
        Transport::OpenCode(endpoint) => {
            endpoint.headers(request, account, &HeaderMap::new(), &plan.body)
        }
        Transport::Gitlab => {
            request.bearer_auth(crate::gitlab_transport::token(account).unwrap_or_default())
        }
        Transport::Codex(exchange) => exchange.headers(request, access.unwrap_or_default()),
        Transport::Github(mode) => {
            return match mode {
                GithubRoute::Messages => github_claude::headers(request, account, plan.stream),
                GithubRoute::Responses => github_responses::headers(request, account, plan.stream),
                GithubRoute::Chat => {
                    github_executor::headers(request, account, &Value::Null, plan.stream)
                }
            };
        }
    };
    if plan.stream {
        request = request.header(header::ACCEPT, "text/event-stream");
    }
    request
}

async fn execute(
    clients: &Clients,
    route: &Route,
    plan: &Plan,
    provider: &str,
    model: &str,
    account: &Value,
) -> Result<reqwest::Response, &'static str> {
    let fixture = match &plan.transport {
        Transport::GeminiCli => crate::gemini_cli_transport::local_url(plan.stream),
        Transport::Gitlab => Some(crate::gitlab_transport::fixture_url(&plan.destination)),
        Transport::IFlow => crate::iflow_transport::fixture_url(),
        Transport::OpenCode(endpoint) => endpoint.fixture_url(),
        Transport::CommandCode => crate::commandcode_transport::fixture_url().map(str::to_owned),
        Transport::Ollama if provider == "ollama" => {
            crate::ollama_local::cloud_fixture().map(str::to_owned)
        }
        Transport::Codex(exchange) => exchange.fixture_url(),
        Transport::Github(mode) => match mode {
            GithubRoute::Messages => github_claude::fixture_url(),
            GithubRoute::Responses => github_responses::fixture_url(),
            GithubRoute::Chat => github_executor::fixture_url(),
        },
        Transport::Static(endpoint)
            if is_minimax(provider)
                || provider == "glm"
                || matches!(
                    provider,
                    "kimi" | "muse" | "xiaomi-tokenplan" | "xiaomi-mimo"
                ) =>
        {
            endpoint.fixture_url(provider, account)
        }
        _ => fixture_destination(provider, &plan.destination),
    };
    let destination = fixture.unwrap_or_else(|| plan.destination.clone());
    let mut attempts = 0;
    let mut codex_stream_retries = 0;
    let mut fresh = false;
    loop {
        let primary = if fresh {
            &clients.fresh
        } else {
            &clients.pooled
        };
        let request = match route {
            Route::Direct => primary.post(&destination),
            Route::Proxy { url, .. } => clients
                .proxy
                .get(url, fresh, plan.timeout)
                .map_err(|_| "Invalid provider proxy")?
                .post(&plan.destination),
            Route::Relay { url, .. } => primary.post(url),
            Route::MissingStrict => return Err("Required provider proxy is unavailable"),
        };
        let request = route.relay(with_auth(request.json(&plan.body), plan, model, account));
        let mut result = tokio::time::timeout(plan.timeout, request.send()).await;
        if matches!(&plan.transport, Transport::Codex(_))
            && let Ok(Ok(response)) = result
        {
            result = match codex_executor::probe(response).await {
                codex_executor::Probe::Response(response) => Ok(Ok(response)),
                codex_executor::Probe::Transient {
                    account_capacity, ..
                } => {
                    if account_capacity || codex_stream_retries == 3 {
                        return Err("Codex upstream is temporarily unavailable");
                    }
                    codex_stream_retries += 1;
                    attempts = 0;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
        }
        let code = result
            .as_ref()
            .ok()
            .and_then(|reply| reply.as_ref().ok())
            .map_or(502, |response| response.status().as_u16());
        let (limit, delay) = retry_policy(provider, code);
        if attempts < limit {
            attempts += 1;
            fresh |= code == 502;
            drop(result);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            continue;
        }
        return match result {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err("fetch failed"),
            Err(_) => Err("fetch connect timeout"),
        };
    }
}

fn retry_policy(provider: &str, code: u16) -> (usize, u64) {
    match code {
        429 if matches!(provider, "tokenharbor" | "vercel-ai-gateway") => (2, 2000),
        502 => (3, 3000),
        503 => (3, 2000),
        504 => (2, 3000),
        _ => (0, 0),
    }
}

fn selected_secrets(account: &Value) -> Vec<Vec<u8>> {
    [
        "apiKey",
        "accessToken",
        "refreshToken",
        "idToken",
        "copilotToken",
    ]
    .into_iter()
    .flat_map(|key| [account.get(key), account["providerSpecificData"].get(key)])
    .flatten()
    .filter_map(Value::as_str)
    .filter(|value| !value.is_empty())
    .map(|value| value.as_bytes().to_owned())
    .collect()
}

fn success(response: reqwest::Response, secrets: Vec<Vec<u8>>) -> Response {
    let source = response.bytes_stream();
    let secrets: Vec<_> = secrets
        .into_iter()
        .filter(|token| token.len() >= 6)
        .collect();
    let body = if secrets.is_empty() {
        Body::from_stream(source)
    } else {
        let initial = (Box::pin(source), secrets, Vec::<u8>::new(), false);
        Body::from_stream(stream::unfold(
            initial,
            |(mut source, secrets, mut pending, finished)| async move {
                if finished {
                    return None;
                }
                loop {
                    match source.next().await {
                        Some(Ok(bytes)) => {
                            pending.extend_from_slice(&bytes);
                            let output = scrub(&mut pending, &secrets, false);
                            if !output.is_empty() {
                                return Some((Ok(output), (source, secrets, pending, false)));
                            }
                        }
                        Some(Err(error)) => {
                            return Some((Err(error), (source, secrets, pending, true)));
                        }
                        None => {
                            let output = scrub(&mut pending, &secrets, true);
                            return (!output.is_empty())
                                .then_some((Ok(output), (source, secrets, pending, true)));
                        }
                    }
                }
            },
        ))
    };
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header(header::VARY, "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding")
        .body(body)
        .expect("fixed translator stream headers")
}

fn scrub(buffer: &mut Vec<u8>, secrets: &[Vec<u8>], done: bool) -> Vec<u8> {
    let mut read = 0;
    let mut output = Vec::with_capacity(buffer.len());
    while read < buffer.len() {
        let rest = &buffer[read..];
        if !done
            && secrets
                .iter()
                .any(|secret| secret.len() > rest.len() && secret.starts_with(rest))
        {
            break;
        }
        if let Some(secret) = secrets.iter().find(|secret| rest.starts_with(secret)) {
            output.extend_from_slice(b"[redacted]");
            read += secret.len();
        } else {
            output.push(buffer[read]);
            read += 1;
        }
    }
    buffer.drain(..read);
    output
}
