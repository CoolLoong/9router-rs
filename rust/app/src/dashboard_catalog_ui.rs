use axum::Router;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::get;

use crate::dashboard_ui::{asset, protected, redirect, render_shell};
use crate::storage::Storage;

const COMBOS: &str = include_str!("dashboard_assets/combos.html");
const MODELS: &str = include_str!("dashboard_assets/models.html");
const SCRIPT: &str = include_str!("dashboard_assets/catalog.js");
const SCRIPT_PATH: &str = "/__9router-ui/catalog.js";

#[derive(Clone)]
struct CatalogUi {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route("/dashboard/combos", get(combos))
        .route("/dashboard/models", get(models))
        .route(SCRIPT_PATH, get(script))
        .with_state(CatalogUi { storage })
}

async fn script() -> Response {
    asset("text/javascript; charset=utf-8", SCRIPT)
}

async fn combos(State(state): State<CatalogUi>, request: Request) -> Response {
    match protected(&state.storage, request.headers()).await {
        Ok(session) => render_shell(
            "Combos & Vision Adapter",
            "/dashboard/combos",
            "combos",
            COMBOS,
            session,
            Some(SCRIPT_PATH),
        ),
        Err(path) => redirect(path),
    }
}

async fn models(State(state): State<CatalogUi>, request: Request) -> Response {
    match protected(&state.storage, request.headers()).await {
        Ok(session) => render_shell(
            "Models",
            "/dashboard/models",
            "models",
            MODELS,
            session,
            Some(SCRIPT_PATH),
        ),
        Err(path) => redirect(path),
    }
}
