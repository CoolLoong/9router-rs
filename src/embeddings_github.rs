use std::borrow::Cow;

use reqwest::{Method, Response, StatusCode, header};
use serde_json::Value;

use super::{EmbeddingState, UpstreamError, read_response};
use crate::github_executor::{catalog_metadata, token};
use crate::provider_http::{self, Bound};
use crate::provider_oauth::{RefreshError, RefreshMode, refresh_account};

const ENDPOINT: &str = "https://api.githubcopilot.com/embeddings";
const FIXTURE: &str = "http://127.0.0.1:52989/github/embeddings";

pub(super) async fn fetch(
    state: &EmbeddingState,
    settings: &Value,
    observed: &Value,
    body: &Value,
) -> Result<Value, UpstreamError> {
    let delivery = select(state, settings, observed).await?;
    let protected = delivery.protected;
    let refreshable = observed["authType"] == "oauth";
    let current = if refreshable {
        refresh(state, observed, &delivery, token(observed).is_none())
            .await?
            .map_or(Cow::Borrowed(observed), Cow::Owned)
    } else {
        Cow::Borrowed(observed)
    };
    let initial_token = token(&current).ok_or_else(|| UpstreamError {
        status: StatusCode::UNAUTHORIZED,
        message: "Copilot credentials are unavailable".into(),
        terminal: protected,
    })?;
    let mut response = send(&delivery, initial_token, body).await?;
    let mut updated = None;
    if refreshable
        && matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        )
        && let Some(refreshed) = refresh(state, &current, &delivery, true).await?
        && let Some(new_token) = token(&refreshed)
        && let Ok(retry) = send(&delivery, new_token, body).await
    {
        response = retry;
        updated = Some(refreshed);
    }
    let secrets = [observed, current.as_ref()]
        .into_iter()
        .chain(updated.as_ref())
        .flat_map(|account| {
            [
                token(account),
                account["accessToken"].as_str(),
                account["refreshToken"].as_str(),
            ]
            .into_iter()
            .flatten()
        })
        .chain(delivery.relay_secret());
    read_response(
        response,
        "github",
        protected,
        "Copilot embedding endpoint returned an unexpected redirect",
        secrets,
        true,
    )
    .await
}

async fn select(
    state: &EmbeddingState,
    settings: &Value,
    account: &Value,
) -> Result<Bound, UpstreamError> {
    let selection =
        provider_http::select_authenticated(&state.storage, settings, account, ENDPOINT)
            .await
            .map_err(|_| proxy_unavailable())?;
    provider_http::bind_selection(
        selection,
        ENDPOINT,
        FIXTURE,
        &state.protected,
        state.timeout,
        None,
    )
    .ok_or_else(proxy_unavailable)
}

async fn refresh(
    state: &EmbeddingState,
    account: &Value,
    expected: &Bound,
    rejected: bool,
) -> Result<Option<Value>, UpstreamError> {
    let mode = RefreshMode::github(rejected, expected.route.relay_identity());
    let result = refresh_account(&state.storage, account, mode).await;
    let updated = match result {
        Ok(updated) => Some(updated),
        Err(RefreshError::AccountUnavailable) => {
            return Err(UpstreamError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "The provider account is unavailable".into(),
                terminal: expected.protected,
            });
        }
        Err(RefreshError::Storage) => {
            return Err(
                UpstreamError::bad_gateway("Could not read provider credentials").protected(true),
            );
        }
        Err(_) => None,
    };
    let settings = state
        .storage
        .read_settings()
        .await
        .ok_or_else(proxy_unavailable)?;
    let current = updated.as_ref().unwrap_or(account);
    if !select(state, &settings, current)
        .await?
        .same_route(expected)
    {
        return Err(proxy_unavailable());
    }
    Ok(updated)
}

async fn send(binding: &Bound, credential: &str, body: &Value) -> Result<Response, UpstreamError> {
    let (vscode, extension, agent, version) = catalog_metadata();
    binding
        .request(Method::POST)
        .bearer_auth(credential)
        .header("copilot-integration-id", "vscode-chat")
        .header("editor-version", format!("vscode/{vscode}"))
        .header("editor-plugin-version", format!("copilot-chat/{extension}"))
        .header(header::USER_AGENT, agent)
        .header("x-github-api-version", version)
        .json(body)
        .send()
        .await
        .map_err(|error| UpstreamError::fetch(error, binding.protected))
}

fn proxy_unavailable() -> UpstreamError {
    UpstreamError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "Copilot embedding connection proxy is unavailable".into(),
        terminal: true,
    }
}
