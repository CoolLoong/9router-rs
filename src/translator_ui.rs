use axum::Router;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::get;

use crate::dashboard_ui::{asset, protected, redirect, render, render_shell};
use crate::storage::Storage;

#[path = "translator_api.rs"]
mod api;

const PAGE: &str = include_str!("dashboard_assets/translator.html");
const SCRIPT: &str = include_str!("dashboard_assets/translator.js");
const SCRIPT_PATH: &str = "/__9router-ui/translator.js";
const PATH: &str = "/dashboard/translator";
const STEPS: &[(&str, &str, &str)] = &[
    (
        "Client request",
        "1_req_client.json",
        r#"<button type="button" class="button" data-operation="detect">Detect format</button><button type="button" class="button button-primary" data-operation="intermediate">Convert to OpenAI</button>"#,
    ),
    ("Source body", "2_req_source.json", ""),
    (
        "OpenAI intermediate",
        "3_req_openai.json",
        r#"<button type="button" class="button button-primary" data-operation="build">Build target request</button>"#,
    ),
    (
        "Target request",
        "4_req_target.json",
        r#"<button type="button" class="button button-primary" data-operation="send">Send to provider</button><button type="button" class="button" id="translator-stop" hidden>Stop</button>"#,
    ),
    ("Provider response", "5_res_provider.txt", ""),
    ("OpenAI response", "6_res_openai.txt", ""),
    ("Client response", "7_res_client.txt", ""),
];

#[derive(Clone)]
struct TranslatorUi {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    let api = api::router(storage.clone());
    Router::new()
        .route(PATH, get(page))
        .route(SCRIPT_PATH, get(script))
        .with_state(TranslatorUi { storage })
        .merge(api)
}

async fn script() -> Response {
    asset("text/javascript; charset=utf-8", SCRIPT)
}

async fn page(State(state): State<TranslatorUi>, request: Request) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let panels: String = STEPS.iter().enumerate().map(|(index, (title, file, actions))| {
        let stage = index + 1;
        let expanded = if index == 0 { "open" } else { "" };
        let format = if file.ends_with(".json") {
            r#"<button type="button" class="button" data-operation="format">Format JSON</button>"#
        } else {
            ""
        };
        format!(r#"<details id="translator-stage-{stage}" class="card translator-stage" data-file="{file}" {expanded}>
  <summary>{title} <code>{file}</code></summary>
  <div class="settings-form">
    <textarea aria-label="{title}" rows="9" spellcheck="false"></textarea>
    <div class="key-actions">
      <button type="button" class="button" data-operation="load">Load</button>
      <button type="button" class="button" data-operation="save">Save</button>
      {format}
      <button type="button" class="button" data-operation="copy">Copy</button>
      {actions}
    </div>
  </div>
</details>"#)
    }).collect();
    let content = render(PAGE, &[("panels", &panels)]);
    render_shell(
        "Translator Debug",
        PATH,
        "translator",
        &content,
        session,
        Some(SCRIPT_PATH),
    )
}
