use std::collections::BTreeMap;
use std::fmt::Write;

use axum::Router;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::get;

use crate::dashboard_ui::{asset, escaped, protected, redirect, render_shell};
use crate::storage::Storage;
use crate::{provider_connections, provider_index, provider_nodes};

const PAGE: &str = include_str!("dashboard_assets/usage.html");
const SCRIPT: &str = include_str!("dashboard_assets/usage.js");
const SCRIPT_PATH: &str = "/__9router-ui/usage.js";
const USAGE_PATH: &str = "/dashboard/usage";

#[derive(Clone)]
struct UsageUi {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route(USAGE_PATH, get(usage))
        .route(SCRIPT_PATH, get(script))
        .with_state(UsageUi { storage })
}

async fn script() -> Response {
    asset("text/javascript; charset=utf-8", SCRIPT)
}

async fn usage(State(state): State<UsageUi>, request: Request) -> Response {
    match protected(&state.storage, request.headers()).await {
        Ok(session) => {
            let providers = activity_providers(&state.storage).await;
            render_shell(
                "Usage",
                USAGE_PATH,
                "usage",
                &PAGE.replace("{{usage-activity-providers}}", &providers),
                session,
                Some(SCRIPT_PATH),
            )
        }
        Err(path) => redirect(path),
    }
}

async fn activity_providers(storage: &Storage) -> String {
    storage
        .run(false, |db| {
            let mut roster = BTreeMap::new();
            for entry in provider_index::entries()
                .iter()
                .filter(|entry| entry["noAuthFree"] == true && usage_provider(entry))
            {
                if let Some(id) = entry["id"].as_str() {
                    roster.insert(
                        id.to_owned(),
                        entry["name"].as_str().unwrap_or(id).to_owned(),
                    );
                }
            }
            for account in provider_connections::read_all(db, None)?
                .into_iter()
                .filter(|row| row["isActive"] != false)
            {
                let Some(id) = account["provider"].as_str() else {
                    continue;
                };
                if roster.contains_key(id) {
                    continue;
                }
                let label = if let Some(entry) = provider_index::find(id) {
                    if !usage_provider(entry) {
                        continue;
                    }
                    entry["name"].as_str().unwrap_or(id).to_owned()
                } else if id.starts_with("openai-compatible-")
                    || id.starts_with("anthropic-compatible-")
                {
                    provider_nodes::by_id(db, id)?
                        .and_then(|node| {
                            node.get("name")
                                .and_then(|value| value.as_str())
                                .map(str::to_owned)
                        })
                        .unwrap_or_else(|| id.to_owned())
                } else {
                    continue;
                };
                roster.insert(id.to_owned(), label);
            }
            let mut html = String::new();
            for (id, label) in roster {
                let _ = write!(
                    html,
                    r#"<li data-provider="{}">{}</li>"#,
                    escaped(&id),
                    escaped(&label)
                );
            }
            Ok(html)
        })
        .await
        .unwrap_or_default()
}

fn usage_provider(entry: &serde_json::Value) -> bool {
    entry["hidden"] != true
        && entry["serviceKinds"]
            .as_array()
            .is_none_or(|kinds| kinds.is_empty() || kinds.iter().any(|kind| kind == "llm"))
}
