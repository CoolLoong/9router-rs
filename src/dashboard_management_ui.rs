use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde_json::Value;

use crate::dashboard_ui::{asset, escaped, protected, redirect, render, render_shell};
use crate::provider_index;
use crate::storage::Storage;

const CONNECTION: &str = include_str!("../assets/native-web/connection.html");
const NODE_EDITOR: &str = include_str!("../assets/native-web/provider-node.html");
const NODE_SCRIPT: &str = include_str!("../assets/native-web/provider-node.js");
const NODE_SCRIPT_PATH: &str = "/__9router-ui/provider-node.js";
const SCRIPT: &str = include_str!("../assets/native-web/management.js");
const SCRIPT_PATH: &str = "/__9router-ui/management.js";
const PROVIDER_PATH: &str = "/dashboard/providers";

#[derive(Clone)]
struct ManagementUi {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route(
            "/dashboard/providers/connections/{id}",
            get(edit_connection),
        )
        .route("/dashboard/providers/nodes/new", get(new_node_editor))
        .route("/dashboard/providers/nodes/{id}", get(edit_node_editor))
        .route(NODE_SCRIPT_PATH, get(node_script))
        .route(SCRIPT_PATH, get(script))
        .with_state(ManagementUi { storage })
}

async fn script() -> Response {
    asset("text/javascript; charset=utf-8", SCRIPT)
}

async fn node_script() -> Response {
    asset("text/javascript; charset=utf-8", NODE_SCRIPT)
}

async fn new_node_editor(State(state): State<ManagementUi>, request: Request) -> Response {
    match protected(&state.storage, request.headers()).await {
        Ok(session) => node_editor("", session),
        Err(path) => redirect(path),
    }
}

async fn edit_node_editor(
    State(state): State<ManagementUi>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let key = id.clone();
    match state
        .storage
        .run(false, move |db| crate::provider_nodes::by_id(db, &key))
        .await
    {
        Ok(Some(node))
            if node
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(node_editor_type) =>
        {
            node_editor(&id, session)
        }
        Ok(_) => node_editor_error(
            StatusCode::NOT_FOUND,
            "Compatible endpoint not found",
            session,
        ),
        Err(_) => node_editor_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not load compatible endpoint",
            session,
        ),
    }
}

fn node_editor_type(kind: &str) -> bool {
    matches!(
        kind,
        "openai-compatible" | "anthropic-compatible" | "custom-embedding"
    )
}

fn node_editor(id: &str, session: bool) -> Response {
    let creating = id.is_empty();
    let title = if creating {
        "Add compatible endpoint"
    } else {
        "Edit compatible endpoint"
    };
    let content = render(
        NODE_EDITOR,
        &[
            ("title", title),
            ("id", &escaped(id)),
            (
                "save",
                if creating {
                    "Create endpoint"
                } else {
                    "Save endpoint"
                },
            ),
            ("edit-hidden", if creating { "hidden" } else { "" }),
        ],
    );
    render_shell(
        title,
        PROVIDER_PATH,
        "provider-node",
        &content,
        session,
        Some(NODE_SCRIPT_PATH),
    )
}

fn node_editor_error(status: StatusCode, message: &'static str, session: bool) -> Response {
    let content = format!(
        r#"<div class="page-heading"><a href="{PROVIDER_PATH}">Back to providers</a><h1>{message}</h1></div>"#,
    );
    let mut page = render_shell(
        message,
        PROVIDER_PATH,
        "provider-detail",
        &content,
        session,
        None,
    );
    *page.status_mut() = status;
    page
}

async fn edit_connection(
    State(state): State<ManagementUi>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    match protected(&state.storage, request.headers()).await {
        Ok(session) => connection(&id, session),
        Err(path) => redirect(path),
    }
}

fn basic_key_form(provider: &Value) -> bool {
    provider["canCreateWithKey"] == true
        && provider["webCookie"] != true
        && provider["hidden"] != true
        && provider["credentialForm"].is_null()
        && provider["apiKeyOptionalWith"].is_null()
}

fn provider_options(only_supported: bool) -> String {
    let mut options: Vec<_> = provider_index::entries()
        .iter()
        .filter(|provider| !only_supported || basic_key_form(provider))
        .filter_map(|provider| {
            Some((
                provider["id"].as_str()?,
                provider["name"].as_str()?,
                provider["alias"].as_str()?,
                &provider["modelsFetcher"],
            ))
        })
        .collect();
    options.sort_unstable_by_key(|(_, name, _, _)| name.to_ascii_lowercase());
    options
        .into_iter()
        .map(|(provider, name, alias, catalog)| {
            let catalog = match (catalog["url"].as_str(), catalog["type"].as_str()) {
                (Some(url), Some(kind)) if kind != "openai" => format!(
                    r#" data-catalog-url="{}" data-catalog-type="{}""#,
                    escaped(url),
                    escaped(kind),
                ),
                _ => String::new(),
            };
            format!(
                r#"<option value="{}" data-alias="{}"{catalog}>{}</option>"#,
                escaped(provider),
                escaped(alias),
                escaped(name)
            )
        })
        .collect()
}

fn connection(id: &str, session: bool) -> Response {
    let creating = id.is_empty();
    let title = if creating {
        "Add API-key connection"
    } else {
        "Edit connection"
    };
    let options = provider_options(true);
    let directory = if creating {
        String::new()
    } else {
        provider_options(false)
    };
    let content = render(
        CONNECTION,
        &[
            ("title", title),
            ("id", &escaped(id)),
            ("providers", &options),
            ("directory", &directory),
            ("create-hidden", if creating { "" } else { "hidden" }),
            ("edit-hidden", if creating { "hidden" } else { "" }),
        ],
    );
    render_shell(
        title,
        PROVIDER_PATH,
        "connection",
        &content,
        session,
        Some(SCRIPT_PATH),
    )
}
