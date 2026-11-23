use std::sync::OnceLock;

use axum::Router;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::get;

use crate::dashboard_ui::{QUOTA_AUTO_PING_DIALOG, asset, protected, redirect, render_shell};
use crate::storage::Storage;

const PROXIES: &str = include_str!("dashboard_assets/proxy-pools.html");
const SAVER: &str = include_str!("dashboard_assets/token-saver.html");
const QUOTA: &str = include_str!("dashboard_assets/quota.html");
const PXPIPE: &str = include_str!("dashboard_assets/pxpipe.html");
const SCRIPT: &str = include_str!("dashboard_assets/optimization.js");
const SCRIPT_PATH: &str = "/__9router-ui/optimization.js";
const PROXY_PATH: &str = "/dashboard/proxy-pools";
const SAVER_PATH: &str = "/dashboard/token-saver";
const QUOTA_PATH: &str = "/dashboard/quota";
const QUOTA_SCRIPT: &str = "/__9router-ui/quota.js";
const PXPIPE_PATH: &str = "/dashboard/pxpipe";
const PXPIPE_SCRIPT: &str = "/__9router-ui/pxpipe.js";
static QUOTA_FORMAT: OnceLock<String> = OnceLock::new();

#[derive(Clone)]
struct OptimizationUi {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route(PROXY_PATH, get(proxy_pools))
        .route(SAVER_PATH, get(token_saver))
        .route(QUOTA_PATH, get(quota))
        .route(PXPIPE_PATH, get(pxpipe))
        .route(SCRIPT_PATH, get(script))
        .route(QUOTA_SCRIPT, get(quota_script))
        .route(PXPIPE_SCRIPT, get(pxpipe_script))
        .route("/__9router-ui/quota-format.js", get(quota_format))
        .with_state(OptimizationUi { storage })
}

async fn script() -> Response {
    asset("text/javascript; charset=utf-8", SCRIPT)
}

async fn proxy_pools(State(state): State<OptimizationUi>, request: Request) -> Response {
    page(
        state,
        request,
        "Proxy Pools",
        PROXY_PATH,
        "proxy-pools",
        PROXIES,
    )
    .await
}

async fn token_saver(State(state): State<OptimizationUi>, request: Request) -> Response {
    page(
        state,
        request,
        "Token Saver",
        SAVER_PATH,
        "token-saver",
        SAVER,
    )
    .await
}

async fn quota(State(state): State<OptimizationUi>, request: Request) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    render_shell(
        "Quota Tracker",
        QUOTA_PATH,
        "quota",
        &format!("{QUOTA}{QUOTA_AUTO_PING_DIALOG}"),
        session,
        Some(QUOTA_SCRIPT),
    )
}

async fn quota_script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("dashboard_assets/quota.js"),
    )
}

async fn pxpipe(State(state): State<OptimizationUi>, request: Request) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    render_shell(
        "PXPIPE",
        PXPIPE_PATH,
        "pxpipe",
        PXPIPE,
        session,
        Some(PXPIPE_SCRIPT),
    )
}

async fn pxpipe_script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("dashboard_assets/pxpipe.js"),
    )
}

async fn quota_format() -> Response {
    let content = QUOTA_FORMAT.get_or_init(|| {
        let catalog = crate::openai_models::data();
        let order = catalog["transportAlias"].as_object().into_iter().flatten()
            .filter_map(|(provider, alias)| {
                let names = catalog["models"][alias.as_str()?].as_array()?.iter()
                    .filter_map(|model| model["id"].as_str().map(|id| serde_json::json!({"id":id})))
                    .collect::<Vec<_>>();
                Some((provider.clone(), serde_json::json!(names)))
            }).collect::<serde_json::Map<_, _>>();
        let source = include_str!("dashboard_assets/quota-format.js");
        format!("const modelOrder={};\nconst getModelsByProviderId = (provider) => Object.hasOwn(modelOrder, provider) ? modelOrder[provider] : [];\n{source}",
            serde_json::Value::Object(order))
    });
    asset("text/javascript; charset=utf-8", content)
}

async fn page(
    state: OptimizationUi,
    request: Request,
    title: &str,
    path: &str,
    name: &str,
    content: &str,
) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    render_shell(title, path, name, content, session, Some(SCRIPT_PATH))
}
