use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;

use crate::dashboard_ui::{
    QUOTA_AUTO_PING_DIALOG, asset, escaped, protected, redirect, render, render_shell,
};
use crate::provider_index;
use crate::storage::Storage;

const PROVIDERS: &str = include_str!("dashboard_assets/providers.html");
const CONNECTION: &str = include_str!("dashboard_assets/connection.html");
const PROVIDER_DETAIL: &str = include_str!("dashboard_assets/provider-detail.html");
const NODE_EDITOR: &str = include_str!("dashboard_assets/provider-node.html");
const NODE_SCRIPT: &str = include_str!("dashboard_assets/provider-node.js");
const NODE_SCRIPT_PATH: &str = "/__9router-ui/provider-node.js";
const PROFILE: &str = include_str!("dashboard_assets/profile.html");
const PROFILE_SCRIPT: &str = include_str!("dashboard_assets/profile.js");
const PROFILE_SCRIPT_PATH: &str = "/__9router-ui/profile.js";
const SCRIPT: &str = include_str!("dashboard_assets/management.js");
const SCRIPT_PATH: &str = "/__9router-ui/management.js";
const PROVIDER_DETAIL_SCRIPT: &str = include_str!("dashboard_assets/provider-detail.js");
const PROVIDER_DETAIL_SCRIPT_PATH: &str = "/__9router-ui/provider-detail.js";
const PROVIDER_ACCOUNT_SCRIPT: &str = include_str!("dashboard_assets/provider-account.js");
const PROVIDER_ACCOUNT_SCRIPT_PATH: &str = "/__9router-ui/provider-account.js";
const PROVIDER_PATH: &str = "/dashboard/providers";
const PROFILE_PATH: &str = "/dashboard/profile";
const ROUTE_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.');

#[derive(Clone)]
struct ManagementUi {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route(PROVIDER_PATH, get(providers))
        .route("/dashboard/providers/new", get(new_connection))
        .route(
            "/dashboard/providers/connections/{id}",
            get(edit_connection),
        )
        .route("/dashboard/providers/nodes/new", get(new_node_editor))
        .route("/dashboard/providers/nodes/{id}", get(edit_node_editor))
        .route("/dashboard/providers/{id}", get(provider_detail))
        .route(PROFILE_PATH, get(profile))
        .route(PROFILE_SCRIPT_PATH, get(profile_script))
        .route(NODE_SCRIPT_PATH, get(node_script))
        .route(SCRIPT_PATH, get(script))
        .route(PROVIDER_DETAIL_SCRIPT_PATH, get(provider_detail_script))
        .route(PROVIDER_ACCOUNT_SCRIPT_PATH, get(provider_account_script))
        .with_state(ManagementUi { storage })
}

async fn script() -> Response {
    asset("text/javascript; charset=utf-8", SCRIPT)
}

async fn profile_script() -> Response {
    asset("text/javascript; charset=utf-8", PROFILE_SCRIPT)
}

async fn node_script() -> Response {
    asset("text/javascript; charset=utf-8", NODE_SCRIPT)
}

async fn provider_detail_script() -> Response {
    asset("text/javascript; charset=utf-8", PROVIDER_DETAIL_SCRIPT)
}

async fn provider_account_script() -> Response {
    asset("text/javascript; charset=utf-8", PROVIDER_ACCOUNT_SCRIPT)
}

async fn providers(State(state): State<ManagementUi>, request: Request) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let oauth_links: String = provider_index::entries()
        .iter()
        .filter(|provider| {
            provider["hidden"] != true
                && (provider["authType"] == "oauth"
                    || provider["canCreateWithKey"] != true
                        && provider["noAuthFree"] != true
                        && provider["webCookie"] != true)
        })
        .filter_map(|provider| Some((provider["id"].as_str()?, provider["name"].as_str()?)))
        .map(|(id, name)| {
            format!(
                r#"<li><a href="/dashboard/providers/{}">{}</a></li>"#,
                escaped(id),
                escaped(name)
            )
        })
        .collect();
    let directory = provider_options(false);
    let content = render(
        PROVIDERS,
        &[("oauth-links", &oauth_links), ("directory", &directory)],
    );
    render_shell(
        "Providers",
        PROVIDER_PATH,
        "providers",
        &content,
        session,
        Some(SCRIPT_PATH),
    )
}

async fn new_connection(State(state): State<ManagementUi>, request: Request) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    connection("", session)
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
        Ok(_) => provider_detail_message(
            StatusCode::NOT_FOUND,
            "Compatible endpoint not found",
            session,
        ),
        Err(_) => provider_detail_message(
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

async fn provider_detail(
    State(state): State<ManagementUi>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let node = if provider_index::compatible(&id).is_some() {
        let id = id.clone();
        match state
            .storage
            .run(false, move |db| crate::provider_nodes::by_id(db, &id))
            .await
        {
            Ok(node) => node.map(Value::Object),
            Err(_) => {
                return provider_detail_message(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not load provider",
                    session,
                );
            }
        }
    } else {
        None
    };
    let builtin = provider_index::find(&id);
    let Some(info) = builtin.or(node.as_ref()) else {
        return provider_detail_message(StatusCode::NOT_FOUND, "Provider not found", session);
    };
    let alias = info["alias"].as_str().unwrap_or(&id);
    let name = info["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .unwrap_or(&id);
    let can_create = node.is_some() || basic_key_form(info);
    let model_kind = if info["type"] == "custom-embedding" {
        "embedding"
    } else {
        info["serviceKinds"]
            .as_array()
            .filter(|kinds| kinds.len() == 1)
            .and_then(|kinds| kinds[0].as_str())
            .unwrap_or("llm")
    };
    let auth = if node.is_some() {
        match info["type"].as_str() {
            Some("anthropic-compatible") => "Anthropic Compatible",
            Some("custom-embedding") => "Custom Embedding",
            _ if info["apiType"] == "responses" => "OpenAI Responses Compatible",
            _ => "OpenAI Compatible",
        }
    } else if info["noAuth"] == true {
        "No authentication"
    } else if info["authGroup"] == "oauth" || info["authGroup"] == "free" {
        if info["canCreateWithKey"] == true {
            "OAuth or API key"
        } else {
            "OAuth"
        }
    } else if info["webCookie"] == true {
        "Session cookie"
    } else {
        "API key"
    };
    let id_encoded = utf8_percent_encode(&id, ROUTE_COMPONENT);
    let alias_encoded = utf8_percent_encode(alias, ROUTE_COMPONENT);
    let account_setup = if supports_provider_account(&id) {
        let local = session
            && (id != "zed" || cfg!(target_os = "linux"))
            && crate::catalog_guard::is_direct_loopback(&request);
        render_provider_account(&id, local)
    } else {
        String::new()
    };
    let advanced = if node.is_some() && info["type"].as_str().is_some_and(node_editor_type) {
        format!(r#"<a href="{PROVIDER_PATH}/nodes/{id_encoded}" class="button">Edit endpoint</a>"#)
    } else if id == "aws-polly" {
        r#"<a href="/dashboard/media-providers/tts/aws-polly#media-credentials-title" class="button">Configure AWS credentials</a>"#.to_owned()
    } else if !account_setup.is_empty() {
        r##"<a href="#provider-account" class="button">Connect an account</a>"##.to_owned()
    } else {
        String::new()
    };
    let mut content = render(
        PROVIDER_DETAIL,
        &[
            ("id", &escaped(&id)),
            ("alias", &escaped(alias)),
            ("kind", &escaped(model_kind)),
            ("title", &escaped(name)),
            ("auth", auth),
            (
                "create",
                &format!("{PROVIDER_PATH}/new?provider={id_encoded}"),
            ),
            ("create-hidden", if can_create { "" } else { "hidden" }),
            (
                "models",
                &format!("/dashboard/models?provider={alias_encoded}"),
            ),
            (
                "new-model",
                &format!(
                    "/dashboard/models?provider={alias_encoded}&amp;type={}&amp;create=1",
                    crate::cli_video::component(model_kind)
                ),
            ),
            ("advanced", &advanced),
            ("account-setup", &account_setup),
        ],
    );
    if matches!(id.as_str(), "claude" | "codex") {
        content.push_str(QUOTA_AUTO_PING_DIALOG);
    }
    render_shell(
        name,
        PROVIDER_PATH,
        "provider-detail",
        &content,
        session,
        Some(PROVIDER_DETAIL_SCRIPT_PATH),
    )
}

fn provider_detail_message(status: StatusCode, message: &'static str, session: bool) -> Response {
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

pub(super) fn supports_provider_account(id: &str) -> bool {
    matches!(
        id,
        "github"
            | "gitlab"
            | "grok-cli"
            | "xiaomi-mimo"
            | "zed"
            | "commandcode"
            | "kiro"
            | "kimi"
            | "glm"
            | "cursor"
            | "iflow"
            | "codex"
            | "xai"
            | "claude"
            | "gemini-cli"
            | "antigravity"
            | "cline"
            | "clinepass"
            | "kilocode"
            | "nous"
            | "qoder"
            | "qoder-cn"
            | "minimax-code"
            | "minimax-code-global"
            | "kimchi"
            | "muse"
            | "codebuddy-intl"
            | "codebuddy-cn"
            | "windsurf"
            | "trae"
            | "grok-web"
            | "chatgpt-web"
            | "gemini-web"
            | "muse-web"
            | "deepseek-web"
            | "uc-web"
    )
}

pub(super) fn render_provider_account(id: &str, local: bool) -> String {
    format!(
        r#"<section class="card" id="provider-account" data-provider="{}" data-local="{local}" aria-labelledby="provider-account-title">
  <h2 id="provider-account-title">Connect an account</h2>
  <p id="provider-account-status" class="notice" role="status" hidden></p>
  <div id="provider-account-forms"></div>
  <script defer src="{PROVIDER_ACCOUNT_SCRIPT_PATH}"></script>
</section>"#,
        escaped(id),
    )
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

async fn profile(State(state): State<ManagementUi>, request: Request) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    render_shell(
        "Profile & Settings",
        PROFILE_PATH,
        "profile",
        PROFILE,
        session,
        Some(PROFILE_SCRIPT_PATH),
    )
}
