use std::collections::BTreeMap;
use std::fmt::Write;

use axum::Router;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::get;

use crate::dashboard_ui::{asset, escaped, protected, redirect, render_shell};
use crate::openai_models::catalog_owner;
use crate::storage::Storage;
use crate::{provider_connections, provider_index};

const CHAT: &str = include_str!("dashboard_assets/basic-chat.html");
const SCRIPT: &str = include_str!("dashboard_assets/basic-chat.js");
const STREAM: &str = include_str!("dashboard_assets/chat-stream.mjs");
const SCRIPT_PATH: &str = "/__9router-ui/basic-chat.js";
const CHAT_PATH: &str = "/dashboard/basic-chat";

#[derive(Clone)]
struct ChatUi {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route(CHAT_PATH, get(chat))
        .route(SCRIPT_PATH, get(script))
        .route("/__9router-ui/chat-stream.mjs", get(stream))
        .with_state(ChatUi { storage })
}

async fn script() -> Response {
    asset("text/javascript; charset=utf-8", SCRIPT)
}

async fn stream() -> Response {
    asset("text/javascript; charset=utf-8", STREAM)
}

async fn chat(State(state): State<ChatUi>, request: Request) -> Response {
    match protected(&state.storage, request.headers()).await {
        Ok(session) => {
            let choices = provider_choices(&state.storage).await;
            render_shell(
                "Basic Chat",
                CHAT_PATH,
                "basic-chat",
                &CHAT.replace("{{providers}}", &choices),
                session,
                Some(SCRIPT_PATH),
            )
        }
        Err(path) => redirect(path),
    }
}

async fn provider_choices(storage: &Storage) -> String {
    let connections = storage
        .run(false, |db| provider_connections::read_all(db, None))
        .await
        .unwrap_or_default();
    let mut choices = BTreeMap::new();
    for connection in connections
        .iter()
        .filter(|entry| entry["isActive"] != false)
    {
        let Some(id) = connection["provider"].as_str() else {
            continue;
        };
        let label = connection["name"]
            .as_str()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| provider_index::find(id).and_then(|entry| entry["name"].as_str()))
            .unwrap_or(id);
        choices
            .entry(catalog_owner(connection, id))
            .or_insert((id, label));
    }
    for entry in provider_index::entries()
        .iter()
        .filter(|entry| entry["noAuthFree"] == true)
    {
        let Some(id) = entry["id"].as_str() else {
            continue;
        };
        let label = entry["name"].as_str().unwrap_or(id);
        choices
            .entry(catalog_owner(entry, id))
            .or_insert((id, label));
    }
    choices.entry("combo").or_insert(("combo", "Combos"));
    let mut html = String::new();
    for (owner, (id, label)) in choices {
        let _ = write!(
            html,
            r#"<option value="{}" data-provider="{}">{}</option>"#,
            escaped(owner),
            escaped(id),
            escaped(label),
        );
    }
    html
}
