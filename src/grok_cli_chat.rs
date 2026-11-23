use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use reqwest::{Client, Method, Response, StatusCode, header};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::chat_accounts;
use crate::grok_cli_http::{account_access, identity_headers, refresh_mode};
use crate::grok_cli_models::{due, expired};
use crate::provider_http::{self, Bound, http_client, requires_protection};
use crate::provider_index::js_trim;
use crate::provider_oauth::refresh_account;
use crate::storage::Storage;

pub(super) const ENDPOINT: &str = "https://cli-chat-proxy.grok.com/v1/responses";
const FIXTURE: &str = "http://127.0.0.1:52989/grok-cli/v1/responses";
const MAX_SESSIONS: usize = 5000;
const SESSION_TTL: Duration = Duration::from_secs(7200);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const STREAM_TIMEOUT: Duration = Duration::from_secs(600);
static HTTP: LazyLock<Option<Client>> = LazyLock::new(|| http_client(STREAM_TIMEOUT).build().ok());
static SESSIONS: Mutex<VecDeque<([u8; 32], Session)>> = Mutex::new(VecDeque::new());

pub(super) fn supports(account: &Value) -> bool {
    account["authType"] == "oauth" && account_access(account).is_some()
}

pub(super) async fn protects_chat(storage: &Storage, settings: &Value) -> bool {
    requires_protection(settings, &Value::Null)
        || chat_accounts::active_connections(storage, "grok-cli")
            .await
            .is_ok_and(|accounts| {
                accounts
                    .iter()
                    .any(|account| requires_protection(settings, account))
            })
}

pub(super) async fn bind(storage: &Storage, account: &Value) -> Option<Bound> {
    provider_http::bind(
        storage,
        account,
        ENDPOINT,
        FIXTURE,
        HTTP.as_ref()?,
        STREAM_TIMEOUT,
        None,
    )
    .await
}

struct Session {
    id: Uuid,
    turn: usize,
    last: Instant,
}

pub(super) struct Exchange {
    delivery: Bound,
    session: Uuid,
    request: Uuid,
    turn: usize,
}

pub(super) struct Failure {
    pub status: StatusCode,
    pub message: &'static str,
}

impl Exchange {
    pub(super) fn new(
        delivery: Bound,
        account: &Value,
        body: &Value,
        source: &Value,
        headers: &header::HeaderMap,
        owner: Option<&str>,
    ) -> Self {
        let candidate = [
            "x-session-id",
            "session-id",
            "session_id",
            "x-amp-thread-id",
            "x-claude-code-session-id",
        ]
        .into_iter()
        .filter_map(|name| headers.get(name).and_then(|value| value.to_str().ok()))
        .chain(
            ["prompt_cache_key", "session_id", "conversation_id"]
                .into_iter()
                .filter_map(|field| source[field].as_str()),
        )
        .chain(source["metadata"]["user_id"].as_str())
        .chain(account["providerSpecificData"]["workspaceId"].as_str())
        .map(js_trim)
        .find(|value| !value.is_empty() && value.encode_utf16().count() <= 256)
        .unwrap_or_default();
        let mut key = Sha256::new();
        for part in [
            account["id"].as_str().unwrap_or_default(),
            owner.unwrap_or_default(),
            candidate,
        ] {
            key.update(part.len().to_le_bytes());
            key.update(part.as_bytes());
        }
        let key: [u8; 32] = key.finalize().into();
        let from_input = body["input"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| {
                item["role"] == "user"
                    && item["type"]
                        .as_str()
                        .is_none_or(|kind| matches!(kind, "" | "message"))
            })
            .count()
            .max(1);
        let now = Instant::now();
        let (session, turn) = {
            let mut sessions = SESSIONS.lock().unwrap_or_else(|error| error.into_inner());
            let previous = sessions
                .iter()
                .position(|(stored, _)| *stored == key)
                .and_then(|index| sessions.remove(index))
                .map(|(_, session)| session)
                .filter(|session| now.duration_since(session.last) <= SESSION_TTL);
            let (session, turn) = previous.map_or_else(
                || (Uuid::new_v4(), from_input),
                |entry| (entry.id, from_input.max(entry.turn.saturating_add(1))),
            );
            sessions.push_front((
                key,
                Session {
                    id: session,
                    turn,
                    last: now,
                },
            ));
            sessions.truncate(MAX_SESSIONS);
            (session, turn)
        };
        Self {
            delivery,
            session,
            request: Uuid::new_v4(),
            turn,
        }
    }

    pub(super) async fn complete(
        &self,
        storage: &Storage,
        account: &Value,
        body: &Value,
    ) -> Result<(Response, Option<Value>), Failure> {
        let renewable = account["refreshToken"]
            .as_str()
            .is_some_and(|value| !js_trim(value).is_empty());
        let updated = if renewable && due(account, 300_000) {
            Some(self.refresh(storage, account).await?)
        } else {
            None
        };
        let current = updated.as_ref().unwrap_or(account);
        if expired(current) {
            return Err(Failure {
                status: StatusCode::UNAUTHORIZED,
                message: "Grok CLI access token has expired",
            });
        }
        let response = self.send(current, body).await?;
        if updated.is_none()
            && renewable
            && matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            )
        {
            drop(response);
            let current = self.refresh(storage, account).await?;
            return Ok((self.send(&current, body).await?, Some(current)));
        }
        Ok((response, updated))
    }

    async fn refresh(&self, storage: &Storage, account: &Value) -> Result<Value, Failure> {
        let current = refresh_account(storage, account, refresh_mode(&self.delivery))
            .await
            .map_err(|_| Failure {
                status: StatusCode::UNAUTHORIZED,
                message: "Grok CLI credential refresh failed",
            })?;
        if expired(&current) {
            return Err(Failure {
                status: StatusCode::UNAUTHORIZED,
                message: "Grok CLI access token has expired",
            });
        }
        let changed = || Failure {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Grok CLI proxy routing changed during credential refresh",
        };
        if !bind(storage, &current)
            .await
            .is_some_and(|route| self.delivery.same_route(&route))
        {
            return Err(changed());
        }
        Ok(current)
    }

    async fn send(&self, account: &Value, body: &Value) -> Result<Response, Failure> {
        let token = account_access(account).ok_or(Failure {
            status: StatusCode::UNAUTHORIZED,
            message: "Invalid Grok CLI access token",
        })?;
        let details = &account["providerSpecificData"];
        let email = details["email"]
            .as_str()
            .or_else(|| account["email"].as_str());
        let user = [
            &details["userId"],
            &account["userId"],
            &account["providerUserId"],
            &details["principalId"],
        ]
        .into_iter()
        .find_map(Value::as_str);
        let session = self.session.to_string();
        let mut attempt = 0;
        loop {
            let mut request =
                identity_headers(self.delivery.request(Method::POST), token, email, user)
                    .header(header::ACCEPT, "text/event-stream")
                    .header("x-grok-session-id", &session)
                    .header("x-grok-conv-id", &session)
                    .header("x-grok-req-id", self.request.to_string())
                    .header("x-grok-turn-idx", self.turn.to_string())
                    .header(
                        "x-grok-model-override",
                        body["model"].as_str().unwrap_or_default(),
                    )
                    .json(body);
            if let Some(device) = details["deviceId"]
                .as_str()
                .or_else(|| details["agentId"].as_str())
                .filter(|value| !value.is_empty() && value.len() <= 1024)
                .and_then(|value| header::HeaderValue::from_str(value).ok())
            {
                request = request.header("x-grok-agent-id", device);
            }
            let response = tokio::time::timeout(CONNECT_TIMEOUT, request.send())
                .await
                .ok()
                .and_then(Result::ok)
                .ok_or(Failure {
                    status: StatusCode::BAD_GATEWAY,
                    message: "Grok CLI fetch failed",
                })?;
            if self.delivery.protected && response.status().is_redirection() {
                return Err(Failure {
                    status: StatusCode::BAD_GATEWAY,
                    message: "Grok CLI proxy redirect refused",
                });
            }
            let delay = match response.status().as_u16() {
                429 => 2000,
                502 | 503 => 1500,
                _ => return Ok(response),
            };
            if attempt == 2 {
                return Ok(response);
            }
            drop(response);
            attempt += 1;
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    }

    pub(super) fn relay_secret(&self) -> Option<&str> {
        self.delivery.relay_secret()
    }
}
