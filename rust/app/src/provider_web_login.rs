use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use rusqlite::TransactionBehavior;
use serde_json::{Value, json};
use tokio::sync::watch;
use uuid::Uuid;

use super::{ProviderState, error, respond};
use crate::chat_output::js_text;
use crate::provider_connections::{self as connections, truthy};
use crate::provider_index::js_trim;
use crate::storage::Storage;

#[path = "provider_web_browser.rs"]
mod browser;
pub(crate) use browser::{ManagedPage, MusePage};
#[path = "provider_web_identity.rs"]
pub(crate) mod identity;

const PROVIDERS: &[&str] = &[
    "grok-web",
    "chatgpt-web",
    "gemini-web",
    "muse-web",
    "deepseek-web",
    "uc-web",
];
const MAX_PARAMS: usize = 32 * 1024;

pub(crate) fn supports(provider: &str) -> bool {
    PROVIDERS.contains(&provider)
}

#[derive(Clone, Default)]
pub(super) struct Sessions(Arc<Mutex<HashMap<String, Session>>>);

struct Session {
    profile: String,
    state: Login,
    cancel: watch::Sender<bool>,
}

enum Login {
    Running,
    Cancelled,
    TimedOut,
    Failed(String),
    Saved { connection: String, email: String },
}

impl Session {
    fn status(&self) -> &'static str {
        match self.state {
            Login::Running => "running",
            Login::Cancelled => "cancelled",
            Login::TimedOut => "timeout",
            Login::Failed(_) => "error",
            Login::Saved { .. } => "success",
        }
    }
}

pub(super) async fn status(State(state): State<ProviderState>, request: Request) -> Response {
    if let Some(provider) = provider(&request) {
        let sessions = state
            .web_login
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(session) = sessions.get(provider.as_ref()) else {
            return error(StatusCode::NOT_FOUND, "No login session for this provider");
        };
        let (failure, connection, email) = match &session.state {
            Login::Failed(failure) => (Some(failure.as_str()), None, None),
            Login::TimedOut => (
                Some("Login window timed out after 5 minutes — try again"),
                None,
                None,
            ),
            Login::Saved { connection, email } => {
                (None, Some(connection), (!email.is_empty()).then_some(email))
            }
            _ => (None, None, None),
        };
        return respond(
            StatusCode::OK,
            json!({
                "provider":provider,"status":session.status(),"profile":session.profile,
                "error":failure,"connectionId":connection,"email":email,"credentialPreview":null,
            }),
        );
    }
    let root = state.storage.root().join("browser-profiles");
    let mut profiles = Vec::new();
    if tokio::fs::symlink_metadata(&root)
        .await
        .is_ok_and(|meta| meta.is_dir())
        && let Ok(mut files) = tokio::fs::read_dir(root).await
    {
        while let Ok(Some(file)) = files.next_entry().await {
            if file.file_type().await.is_ok_and(|kind| kind.is_dir())
                && let Ok(name) = file.file_name().into_string()
                && name != "web-login"
            {
                profiles.push(name);
            }
        }
    }
    profiles.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    respond(
        StatusCode::OK,
        json!({"providers":PROVIDERS,"profiles":profiles}),
    )
}

pub(super) async fn start(State(state): State<ProviderState>, request: Request) -> Response {
    let Ok(bytes) = to_bytes(request.into_body(), MAX_PARAMS).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to start login");
    };
    let Ok(input) = serde_json::from_slice::<Value>(&bytes) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to start login");
    };
    let name = input
        .get("provider")
        .filter(|value| truthy(value))
        .map(js_text)
        .unwrap_or_default();
    let name = js_trim(&name);
    let Some(config) = browser::config(name) else {
        return respond(
            StatusCode::BAD_REQUEST,
            json!({"error":format!("Web login not supported for \"{name}\"")}),
        );
    };
    let profile_value = input
        .get("profile")
        .filter(|value| truthy(value))
        .map(js_text)
        .unwrap_or_default();
    let mut profile: String = js_trim(&profile_value)
        .encode_utf16()
        .take(40)
        .map(|character| {
            u8::try_from(character)
                .ok()
                .filter(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, b'_' | b'-')
                })
                .map_or('-', char::from)
        })
        .collect();
    if profile.is_empty() {
        profile = "default".into();
    }
    let name = name.to_owned();
    let mut sessions = state
        .web_login
        .0
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(session) = sessions.get(&name)
        && matches!(session.state, Login::Running)
    {
        return accepted(&name, &session.profile);
    }
    if let Some((provider, _)) = sessions
        .iter()
        .find(|(_, entry)| matches!(entry.state, Login::Running) && entry.profile == profile)
    {
        return respond(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "error":format!("Profile \"{profile}\" is already open for {provider} — finish that login first or pick another profile"),
            }),
        );
    }
    let path = state.storage.root().join("browser-profiles").join(&profile);
    let (send, receive) = watch::channel(false);
    sessions.insert(
        name.clone(),
        Session {
            profile: profile.clone(),
            state: Login::Running,
            cancel: send,
        },
    );
    drop(sessions);
    tokio::spawn(run(
        state.storage,
        state.web_login,
        name.clone(),
        path,
        config,
        receive,
    ));
    accepted(&name, &profile)
}

pub(super) async fn cancel(State(state): State<ProviderState>, request: Request) -> Response {
    let Some(provider) = provider(&request) else {
        return error(StatusCode::BAD_REQUEST, "provider is required");
    };
    let sessions = state
        .web_login
        .0
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let cancelled = sessions.get(provider.as_ref()).is_some_and(|session| {
        matches!(session.state, Login::Running) && session.cancel.send(true).is_ok()
    });
    respond(StatusCode::OK, json!({"cancelled":cancelled}))
}

fn accepted(provider: &str, profile: &str) -> Response {
    respond(
        StatusCode::ACCEPTED,
        json!({"provider":provider,"profile":profile,"status":"running"}),
    )
}

async fn run(
    storage: Storage,
    sessions: Sessions,
    provider: String,
    path: PathBuf,
    config: browser::Config,
    cancel: watch::Receiver<bool>,
) {
    let result = browser::authenticate(&path, &config, cancel.clone()).await;
    let state = match result {
        browser::Outcome::Captured(credential) if !*cancel.borrow() => {
            match identity::inspect(&storage, &Value::Null, &provider, &credential).await {
                identity::Identity::Invalid(message) => Login::Failed(message.into()),
                identity => {
                    let email = match identity {
                        identity::Identity::Valid(email) => email,
                        _ => String::new(),
                    };
                    if *cancel.borrow() {
                        Login::Cancelled
                    } else {
                        match save(
                            &storage,
                            &provider,
                            email.clone(),
                            credential,
                            config.local_storage,
                        )
                        .await
                        {
                            Ok(connection) => Login::Saved { connection, email },
                            Err(error) => Login::Failed(error),
                        }
                    }
                }
            }
        }
        browser::Outcome::Captured(_) | browser::Outcome::Cancelled => Login::Cancelled,
        browser::Outcome::TimedOut => Login::TimedOut,
        browser::Outcome::Failed(message) => Login::Failed(message.into()),
    };
    let mut entries = sessions.0.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(session) = entries.get_mut(&provider) {
        session.state = state;
    }
}

async fn save(
    storage: &Storage,
    provider: &str,
    email: String,
    credential: String,
    local_storage: bool,
) -> Result<String, String> {
    let provider_name = crate::provider_index::find(provider)
        .and_then(|provider| provider["name"].as_str())
        .unwrap_or(provider);
    let name = if email.is_empty() {
        provider_name.to_owned()
    } else {
        email.clone()
    };
    let provider = provider.to_owned();
    storage
        .run(true, move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let all = connections::read_all(&tx, Some(&provider))?;
            let mut web: Vec<_> = all
                .iter()
                .filter(|row| row["providerSpecificData"]["authMethod"] == "web-login")
                .collect();
            web.sort_by(|left, right| right["updatedAt"].as_str().cmp(&left["updatedAt"].as_str()));
            let selected = if email.is_empty() {
                (web.len() == 1).then(|| web[0])
            } else {
                web.iter().copied().find(|row| row["email"] == email)
            };
            if selected.is_none() && all.iter().any(|row| row["name"] == name) {
                return Ok(Err(format!(
                    "A connection named \"{name}\" already exists — edit or delete it first"
                )));
            }
            let now = connections::now();
            let mut connection = selected.cloned().unwrap_or_else(|| {
                json!({
                    "id":Uuid::new_v4().to_string(),"provider":provider,"authType":"cookie",
                    "createdAt":now,"priority":1,
                })
            });
            let id = connection["id"].as_str().unwrap_or_default().to_owned();
            let mut details = connections::object(connection["providerSpecificData"].clone());
            details.insert("authMethod".into(), json!("web-login"));
            if local_storage {
                details.insert("tokenSource".into(), json!("localStorage"));
            }
            let saved_email = if email.is_empty() {
                connection["email"].clone()
            } else {
                json!(email)
            };
            let patch = connections::object(json!({
                "apiKey":credential,"name":name,"email":saved_email,"providerSpecificData":details,
                "testStatus":"active","lastError":null,"lastErrorAt":null,"isActive":true,
            }));
            connection = connections::patch(connection, patch, &now);
            connections::write(&tx, &connection)?;
            if !email.is_empty() {
                for duplicate in web {
                    if duplicate["id"] != id && duplicate["email"] == email {
                        tx.execute(
                            "DELETE FROM providerConnections WHERE id = ?1",
                            [duplicate["id"].as_str()],
                        )?;
                    }
                }
            }
            connections::reorder(&tx, &provider)?;
            tx.commit()?;
            Ok(Ok(id))
        })
        .await
        .map_err(|_| "Failed to save connection".to_owned())?
}

fn provider(request: &Request) -> Option<std::borrow::Cow<'_, str>> {
    url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
        .find_map(|(name, value)| (name == "provider").then_some(value))
        .filter(|value| !value.is_empty())
}
