use std::sync::OnceLock;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use serde_json::{Value, json};

use super::management_ui::{render_provider_account, supports_provider_account};
use crate::dashboard_ui::{asset, escaped, protected, redirect, render, render_shell};
use crate::provider_index;
use crate::storage::Storage;

const MEDIA_LIST: &str = include_str!("dashboard_assets/media-list.html");
const MEDIA_DETAIL: &str = include_str!("dashboard_assets/media-detail.html");
const MEDIA_TTS: &str = include_str!("dashboard_assets/media-tts.html");
const MEDIA_CREDENTIALS: &str = include_str!("dashboard_assets/media-credentials.html");
const MEDIA_COMBO: &str = include_str!("dashboard_assets/media-combo.html");
const CLI_LIST: &str = include_str!("dashboard_assets/cli-list.html");
const CLI_DETAIL: &str = include_str!("dashboard_assets/cli-detail.html");
const CLI_GUIDE: &str = include_str!("dashboard_assets/cli-guide.html");
const MITM_PAGE: &str = include_str!("dashboard_assets/mitm.html");
const SCRIPT: &str = include_str!("dashboard_assets/integrations.js");
const SCRIPT_PATH: &str = "/__9router-ui/integrations.js";
const HERMES_SCRIPT_PATH: &str = "/__9router-ui/hermes.js";
const MITM_SCRIPT_PATH: &str = "/__9router-ui/mitm.js";
const CLI_PATH: &str = "/dashboard/cli-tools";
const MITM_PATH: &str = "/dashboard/mitm";
const MEDIA_PATH: &str = "/dashboard/media-providers";
const MEDIA_WEB_PATH: &str = "/dashboard/media-providers/web";
const NODE_COMPONENT: &percent_encoding::AsciiSet =
    &percent_encoding::NON_ALPHANUMERIC.remove(b'-');
static TOOLS: OnceLock<Vec<Value>> = OnceLock::new();
static TTS_CATALOG: OnceLock<Value> = OnceLock::new();
static MEDIA_INFO: OnceLock<Value> = OnceLock::new();
static MITM_TOOLS: OnceLock<Vec<Value>> = OnceLock::new();

#[derive(Clone)]
struct IntegrationsUi {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    Router::new()
        .route("/dashboard/media-providers/{kind}", get(media_list))
        .route("/dashboard/media-providers/combo/{id}", get(media_combo))
        .route("/dashboard/media-providers/{kind}/{id}", get(media_detail))
        .route(CLI_PATH, get(cli_list))
        .route(MITM_PATH, get(mitm_page))
        .route("/dashboard/cli-tools/{tool}", get(cli_detail))
        .route("/__9router-ui/cli-status", get(cli_status))
        .route(SCRIPT_PATH, get(script))
        .route(HERMES_SCRIPT_PATH, get(hermes_script))
        .route(MITM_SCRIPT_PATH, get(mitm_script))
        .with_state(IntegrationsUi { storage })
}

async fn script() -> Response {
    asset("text/javascript; charset=utf-8", SCRIPT)
}

async fn hermes_script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("dashboard_assets/hermes.js"),
    )
}

async fn mitm_script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("dashboard_assets/mitm.js"),
    )
}

#[derive(Clone, Copy)]
struct Media {
    kind: &'static str,
    label: &'static str,
    preview: &'static str,
}

fn media(kind: &str) -> Option<Media> {
    let (kind, label, preview) = match kind {
        "web" => ("web", "Web Fetch & Search", ""),
        "webSearch" => ("webSearch", "Web Search", "/v1/search"),
        "webFetch" => ("webFetch", "Web Fetch", "/v1/web/fetch"),
        "embedding" => ("embedding", "Embeddings", "/v1/embeddings"),
        "tts" => ("tts", "Text to Speech", "/v1/audio/speech"),
        "stt" => ("stt", "Speech to Text", "/v1/audio/transcriptions"),
        "image" => ("image", "Images", "/v1/images/generations"),
        "imageToText" => ("imageToText", "Image to Text", ""),
        "video" => ("video", "Video", "/v1/videos/generations"),
        "music" => ("music", "Music", ""),
        "systemone" => ("systemone", "System One", "/v1/systemone"),
        _ => return None,
    };
    Some(Media {
        kind,
        label,
        preview,
    })
}

fn kind_path(kind: &str) -> String {
    let kind = if matches!(kind, "webSearch" | "webFetch") {
        "web"
    } else {
        kind
    };
    format!("{MEDIA_PATH}/{kind}")
}

fn supports(provider: &Value, kind: &str) -> bool {
    provider["hidden"] != true
        && has_service(provider, kind)
        && !provider["hiddenKinds"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|entry| entry == kind))
}

fn has_service(provider: &Value, kind: &str) -> bool {
    provider["serviceKinds"]
        .as_array()
        .is_some_and(|kinds| kinds.iter().any(|entry| entry == kind))
}

fn integration_error(status: StatusCode, heading: &str, back: &str, session: bool) -> Response {
    let content = format!(
        r#"<div class="page-heading"><a href="{}">Back</a><h1>{}</h1></div>"#,
        escaped(back),
        escaped(heading),
    );
    let mut response = render_shell(heading, back, "integration-error", &content, session, None);
    *response.status_mut() = status;
    response
}

fn media_credentials(provider: &str, kind: &str) -> Option<String> {
    let (scheme, title, url_label, placeholder) = match (provider, kind) {
        ("vertex", "video" | "imageToText") => ("vertex", "Vertex credentials", "", ""),
        ("cloudflare-ai", "systemone" | "image") => ("cloudflare", "Cloudflare Workers AI", "", ""),
        ("aws-polly", "tts") => ("polly", "AWS Polly credentials", "", ""),
        ("selfhosted-stt", "stt") => (
            "selfhosted",
            "Self-hosted speech to text",
            "Transcription endpoint",
            "http://localhost:8080/v1/audio/transcriptions",
        ),
        ("selfhosted-tts", "tts") => (
            "selfhosted",
            "Self-hosted text to speech",
            "Server base URL",
            "http://localhost:8880",
        ),
        ("selfhosted-embedding", "embedding") => (
            "selfhosted",
            "Self-hosted embeddings",
            "OpenAI-compatible base URL",
            "http://localhost:8080/v1",
        ),
        _ => return None,
    };
    Some(render(
        MEDIA_CREDENTIALS,
        &[
            ("scheme", scheme),
            ("title", title),
            ("url-label", url_label),
            ("url-placeholder", placeholder),
            ("key-hidden", if scheme == "vertex" { "hidden" } else { "" }),
            (
                "key-label",
                if scheme == "polly" {
                    "AWS secret access key"
                } else {
                    "API key"
                },
            ),
            (
                "polly-hidden",
                if scheme == "polly" { "" } else { "hidden" },
            ),
            (
                "url-hidden",
                if scheme == "selfhosted" { "" } else { "hidden" },
            ),
            (
                "account-hidden",
                if scheme == "cloudflare" { "" } else { "hidden" },
            ),
            (
                "vertex-hidden",
                if scheme == "vertex" { "" } else { "hidden" },
            ),
        ],
    ))
}

fn tts_preview(provider: &str) -> String {
    let catalog = TTS_CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("dashboard-tts-catalog.json"))
            .expect("embedded speech preview catalog")
    });
    let config = &catalog[provider];
    render(
        MEDIA_TTS,
        &[
            ("catalog", &escaped(&config.to_string())),
            (
                "model-hidden",
                if config["hasModel"] == false {
                    "hidden"
                } else {
                    ""
                },
            ),
            (
                "browse-hidden",
                if config["endpoint"].is_string() {
                    ""
                } else {
                    "hidden"
                },
            ),
        ],
    )
}

async fn media_list(
    State(state): State<IntegrationsUi>,
    Path(kind): Path<String>,
    request: Request,
) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let Some(kind) = media(&kind) else {
        return integration_error(
            StatusCode::NOT_FOUND,
            "Media category not found",
            MEDIA_WEB_PATH,
            session,
        );
    };
    if matches!(kind.kind, "webSearch" | "webFetch") {
        return redirect(MEDIA_WEB_PATH);
    }
    let web = kind.kind == "web";
    let entries: String = provider_index::entries()
        .iter()
        .filter_map(|provider| {
            let id = provider["id"].as_str()?;
            let name = provider["name"].as_str()?;
            let mut links = Vec::new();
            if web {
                for (key, label) in [("webFetch", "Web Fetch"), ("webSearch", "Web Search")] {
                    if supports(provider, key) {
                        links.push((key, label));
                    }
                }
            } else if supports(provider, kind.kind) {
                links.push((kind.kind, kind.label));
            }
            if links.is_empty() {
                return None;
            }
            let links: String = links.iter().map(|(kind, label)| format!(
                r#"<a href="{MEDIA_PATH}/{kind}/{}">{}</a>"#, escaped(id), escaped(label)
            )).collect::<Vec<_>>().join(" · ");
            Some(format!(
                r#"<article class="key-row" data-media-id="{}"><div class="key-meta"><h3>{}</h3><span class="muted" data-connection-count></span></div><div class="key-actions">{links}</div></article>"#,
                escaped(id), escaped(name)
            ))
        })
        .collect();
    let title = if web {
        "Web Fetch & Search"
    } else {
        kind.label
    };
    let content = render(
        MEDIA_LIST,
        &[
            ("title", &escaped(title)),
            ("kind", kind.kind),
            ("providers", &entries),
            ("web-hidden", if web { "" } else { "hidden" }),
            (
                "embedding-hidden",
                if kind.kind == "embedding" {
                    ""
                } else {
                    "hidden"
                },
            ),
        ],
    );
    render_shell(
        title,
        &kind_path(kind.kind),
        "media-list",
        &content,
        session,
        Some(SCRIPT_PATH),
    )
}

async fn media_detail(
    State(state): State<IntegrationsUi>,
    Path((kind, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let Some(kind) = media(&kind).filter(|kind| kind.kind != "web") else {
        return integration_error(
            StatusCode::NOT_FOUND,
            "Media category not found",
            MEDIA_WEB_PATH,
            session,
        );
    };
    let provider = provider_index::find(&id);
    let custom = kind.kind == "embedding" && id.starts_with("custom-embedding-");
    let custom_node = if custom {
        let key = id.clone();
        match state
            .storage
            .run(false, move |db| crate::provider_nodes::by_id(db, &key))
            .await
        {
            Ok(Some(node))
                if node.get("type").and_then(Value::as_str) == Some("custom-embedding") =>
            {
                Some(Value::Object(node))
            }
            Ok(_) => {
                return integration_error(
                    StatusCode::NOT_FOUND,
                    "Embedding endpoint not found",
                    &kind_path(kind.kind),
                    session,
                );
            }
            Err(_) => {
                return integration_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not load embedding endpoint",
                    &kind_path(kind.kind),
                    session,
                );
            }
        }
    } else {
        None
    };
    let Some(info) = custom_node
        .as_ref()
        .or(provider.filter(|entry| has_service(entry, kind.kind)))
    else {
        return integration_error(
            StatusCode::NOT_FOUND,
            "Media provider not found",
            &kind_path(kind.kind),
            session,
        );
    };
    let name = info["name"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or("Custom embedding");
    let alias = info[if custom { "prefix" } else { "alias" }]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or(&id);
    let credentials = media_credentials(&id, kind.kind);
    let no_auth = provider.is_some_and(|entry| entry["noAuth"] == true);
    let oauth = if supports_provider_account(&id) {
        render_provider_account(
            &id,
            session
                && (id != "zed" || cfg!(target_os = "linux"))
                && crate::catalog_guard::is_direct_loopback(&request),
        )
    } else {
        String::new()
    };
    let can_create = custom
        || !no_auth
            && (credentials.is_none() || (id == "vertex" && kind.kind == "imageToText"))
            && provider.is_some_and(|entry| {
                entry["canCreateWithKey"] == true
                    && entry["webCookie"] != true
                    && entry["credentialForm"].is_null()
                    && entry["apiKeyOptionalWith"].is_null()
            });
    let title = format!("{name} · {}", kind.label);
    let provider_path = format!(
        "/dashboard/providers/{}",
        percent_encoding::utf8_percent_encode(&id, NODE_COMPONENT)
    );
    let service_info = media_service_info(&id, kind);
    let tts = if kind.kind == "tts" {
        tts_preview(&id)
    } else {
        String::new()
    };
    let content = render(
        MEDIA_DETAIL,
        &[
            ("title", &escaped(&title)),
            ("kind", kind.kind),
            ("provider", &escaped(&id)),
            ("alias", &escaped(alias)),
            ("preview", kind.preview),
            ("tts", &tts),
            ("provider-settings", &escaped(&provider_path)),
            ("service-info", &service_info),
            ("back", &escaped(&kind_path(kind.kind))),
            ("credentials", credentials.as_deref().unwrap_or("")),
            ("oauth", &oauth),
            ("no-auth-hidden", if no_auth { "" } else { "hidden" }),
            (
                "public-proxy-hidden",
                if no_auth && crate::proxy_transport::supports_public_proxy_pool(&id) {
                    ""
                } else {
                    "hidden"
                },
            ),
            (
                "create-label",
                if id == "ollama-local" {
                    "Add Ollama server"
                } else {
                    "Add API-key connection"
                },
            ),
            ("create-hidden", if can_create { "" } else { "hidden" }),
            ("custom-hidden", if custom { "" } else { "hidden" }),
            (
                "embedding-editor",
                &format!(
                    "/dashboard/providers/nodes/{}",
                    percent_encoding::utf8_percent_encode(&id, NODE_COMPONENT)
                ),
            ),
            (
                "sample-hidden",
                if kind.preview.is_empty() {
                    "hidden"
                } else {
                    ""
                },
            ),
            (
                "audio-hidden",
                if kind.kind == "stt" { "" } else { "hidden" },
            ),
            (
                "image-hidden",
                if kind.kind == "image" { "" } else { "hidden" },
            ),
        ],
    );
    media_preview_policy(render_shell(
        &title,
        &kind_path(kind.kind),
        "media-detail",
        &content,
        session,
        Some(SCRIPT_PATH),
    ))
}

fn media_service_info(provider: &str, kind: Media) -> String {
    let info = &MEDIA_INFO.get_or_init(|| {
        serde_json::from_str(include_str!("dashboard-media-info.json"))
            .expect("embedded media service information")
    })[provider];
    let service = &info["services"][kind.kind];
    let mut rows = String::new();
    for (field, label) in [
        ("mode", "Mode"),
        ("defaultModel", "Model"),
        ("baseUrl", "Default upstream endpoint"),
        ("costPerQuery", "Cost per call"),
        ("pricingUrl", "Pricing"),
        ("freeTier", "Free tier"),
        ("freeMonthlyQuota", "Free quota"),
        ("searchTypes", "Types"),
        ("formats", "Formats"),
        ("maxMaxResults", "Max results"),
        ("maxCharacters", "Max characters"),
    ] {
        let value = &service[field];
        let value = match (field, value) {
            ("pricingUrl", Value::String(url)) => format!(
                r#"<a href="{}" target="_blank" rel="noopener noreferrer">View pricing</a>"#,
                escaped(url)
            ),
            ("baseUrl" | "defaultModel", Value::String(text)) if !text.is_empty() => {
                format!("<code>{}</code>", escaped(text))
            }
            (_, Value::String(text)) if !text.is_empty() => escaped(text),
            ("costPerQuery", Value::Number(amount)) => match amount.as_f64() {
                Some(0.0) => "Free".to_owned(),
                Some(value) => format!("${value:.4}"),
                None => continue,
            },
            ("freeMonthlyQuota", Value::Number(amount)) => match amount.as_u64() {
                Some(0) => "—".to_owned(),
                Some(value) if value >= 999_999 => "Unlimited".to_owned(),
                Some(value) => format!("{value} / mo"),
                None => continue,
            },
            (_, Value::Number(amount)) => amount.to_string(),
            (_, Value::Array(items)) if !items.is_empty() => escaped(
                &items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            _ => continue,
        };
        rows.push_str(&format!("<dt>{label}</dt><dd>{value}</dd>"));
    }
    let notes = [&info["notice"], &info["kindNotice"][kind.kind]]
        .into_iter()
        .filter_map(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| format!(r#"<p class="muted">{}</p>"#, escaped(text)))
        .collect::<String>();
    if rows.is_empty() && notes.is_empty() {
        return String::new();
    }
    let site = info["apiKeyUrl"]
        .as_str()
        .map(|url| {
            format!(
                r#"<a href="{}" target="_blank" rel="noopener noreferrer">Provider site</a>"#,
                escaped(url)
            )
        })
        .unwrap_or_default();
    let details = if rows.is_empty() {
        String::new()
    } else {
        format!("<dl>{rows}</dl>")
    };
    format!(
        r#"<section class="card" aria-labelledby="media-service-info-title">
  <div class="card-heading"><h2 id="media-service-info-title">Service configuration</h2>{site}</div>
  {notes}{details}
</section>"#
    )
}

async fn media_combo(
    State(state): State<IntegrationsUi>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let lookup = id.clone();
    match state
        .storage
        .run(false, move |db| {
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM combos WHERE id = ?1)",
                [lookup],
                |row| row.get::<_, bool>(0),
            )
        })
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return integration_error(
                StatusCode::NOT_FOUND,
                "Media combo not found",
                MEDIA_WEB_PATH,
                session,
            );
        }
        Err(_) => {
            return integration_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load media combo",
                MEDIA_WEB_PATH,
                session,
            );
        }
    }
    let providers: Vec<_> = provider_index::entries()
        .iter()
        .filter_map(|entry| {
            let kinds: Vec<_> = ["webSearch", "webFetch"]
                .into_iter()
                .filter(|kind| supports(entry, kind))
                .collect();
            let id = entry["id"].as_str()?;
            (!kinds.is_empty()).then(|| {
                json!({"id":id,"alias":entry["alias"].as_str().unwrap_or(id),
            "name":entry["name"],"noAuth":entry["noAuth"] == true,"kinds":kinds})
            })
        })
        .collect();
    let content = render(
        MEDIA_COMBO,
        &[
            ("id", &escaped(&id)),
            ("providers", &escaped(&json!(providers).to_string())),
        ],
    );
    media_preview_policy(render_shell(
        "Media combo",
        &kind_path("web"),
        "media-combo",
        &content,
        session,
        Some(SCRIPT_PATH),
    ))
}

fn media_preview_policy(mut response: Response) -> Response {
    if let Some(policy) = response
        .headers_mut()
        .get_mut(header::CONTENT_SECURITY_POLICY)
        && let Ok(text) = policy.to_str()
        && let Ok(media) = HeaderValue::from_str(&format!(
            "{text}; img-src blob: data:; media-src blob: data:"
        ))
    {
        *policy = media;
    }
    response
}

fn tools() -> &'static [Value] {
    TOOLS.get_or_init(|| {
        serde_json::from_str(include_str!("dashboard-tools.json"))
            .expect("embedded CLI tool catalog")
    })
}

async fn cli_list(State(state): State<IntegrationsUi>, request: Request) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let entries: String = tools().iter().filter_map(|tool| {
        let id = tool["id"].as_str()?;
        let name = tool["name"].as_str()?;
        let desc = tool["description"].as_str().unwrap_or_default();
        Some(format!(r#"<article class="key-row" data-cli-id="{}">
<div class="key-meta"><h3>{}</h3><span class="muted">{}</span><span class="key-state" data-install-state></span></div>
<a class="button" href="{CLI_PATH}/{}">Configure</a></article>"#,
            escaped(id), escaped(name), escaped(desc), escaped(id)
        ))
    }).collect();
    let content = render(CLI_LIST, &[("tools", &entries)]);
    render_shell(
        "CLI Tools",
        CLI_PATH,
        "cli-list",
        &content,
        session,
        Some(SCRIPT_PATH),
    )
}

async fn mitm_page(State(state): State<IntegrationsUi>, request: Request) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let tools = MITM_TOOLS.get_or_init(|| {
        serde_json::from_str(include_str!("dashboard-mitm-tools.json"))
            .expect("embedded MITM tool catalog")
    });
    let entries: String = tools
        .iter()
        .filter_map(|tool| {
            let id = tool["id"].as_str()?;
            let name = tool["name"].as_str()?;
            Some(format!(
                r#"<option value="{}" data-models="{}">{}</option>"#,
                escaped(id),
                escaped(&tool["models"].to_string()),
                escaped(name)
            ))
        })
        .collect();
    let content = render(MITM_PAGE, &[("tools", &entries)]);
    render_shell(
        "MITM settings",
        CLI_PATH,
        "mitm",
        &content,
        session,
        Some(MITM_SCRIPT_PATH),
    )
}

async fn cli_detail(
    State(state): State<IntegrationsUi>,
    Path(tool): Path<String>,
    request: Request,
) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let Some(data) = tools().iter().find(|entry| entry["id"] == tool) else {
        return integration_error(
            StatusCode::NOT_FOUND,
            "CLI tool not found",
            CLI_PATH,
            session,
        );
    };
    let title = data["name"].as_str().unwrap_or("CLI tool");
    let desc = data["description"].as_str().unwrap_or_default();
    if data["guide"]["steps"].is_array() {
        return cli_guide(&tool, title, desc, &data["guide"], session);
    }
    if tool == "hermes" {
        let content = render(
            include_str!("dashboard_assets/hermes-detail.html"),
            &[("title", &escaped(title)), ("desc", &escaped(desc))],
        );
        return render_shell(
            title,
            CLI_PATH,
            "hermes-detail",
            &content,
            session,
            Some(HERMES_SCRIPT_PATH),
        );
    }
    let support = matches!(
        tool.as_str(),
        "omp"
            | "crush"
            | "forge"
            | "smelt"
            | "codewhale"
            | "codex"
            | "droid"
            | "openclaw"
            | "pi"
            | "claude"
            | "opencode"
            | "cline"
            | "kilo"
            | "copilot"
            | "cowork"
            | "deepseek-tui"
            | "jcode"
            | "grok-build"
    );
    let content = render(
        CLI_DETAIL,
        &[
            ("title", &escaped(title)),
            ("desc", &escaped(desc)),
            ("id", &escaped(&tool)),
            ("settings-hidden", if support { "" } else { "hidden" }),
            ("model-hidden", if tool == "omp" { "hidden" } else { "" }),
            (
                "extra-models",
                if tool == "opencode" {
                    r#"<label for="cli-extra-models">Additional models, one per line</label><textarea id="cli-extra-models" rows="3"></textarea>
<label for="cli-opencode-default">Model on launch</label><select id="cli-opencode-default"><option value="">Use OpenCode default</option></select>
<label for="cli-opencode-explorer">Explorer model ID</label><input id="cli-opencode-explorer" list="cli-model-options" placeholder="Use first configured model" autocomplete="off">"#
                } else if matches!(tool.as_str(), "droid" | "copilot" | "cowork") {
                    r#"<label for="cli-extra-models">Additional models, one per line</label><textarea id="cli-extra-models" rows="3"></textarea>"#
                } else if tool == "claude" {
                    include_str!("dashboard_assets/claude-env.html")
                } else if tool == "grok-build" {
                    include_str!("dashboard_assets/grok-build-options.html")
                } else if tool == "openclaw" {
                    r#"<fieldset id="cli-openclaw-agents" class="settings-form" hidden><legend>Agent models</legend></fieldset>"#
                } else if tool == "codex" {
                    r#"<label for="cli-codex-subagent">Subagent model ID</label><input id="cli-codex-subagent" list="cli-model-options" placeholder="Use main model" autocomplete="off">"#
                } else {
                    ""
                },
            ),
            (
                "tool-advanced",
                match tool.as_str() {
                    "codex" => include_str!("dashboard_assets/codex-profiles.html"),
                    "opencode" => include_str!("dashboard_assets/opencode-models.html"),
                    "cowork" => include_str!("dashboard_assets/cowork-connectors.html"),
                    _ => "",
                },
            ),
        ],
    );
    render_shell(
        title,
        CLI_PATH,
        "cli-detail",
        &content,
        session,
        Some(SCRIPT_PATH),
    )
}

fn cli_guide(tool: &str, title: &str, desc: &str, guide: &Value, session: bool) -> Response {
    let notes = guide["notes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|note| note["text"].as_str())
        .map(|text| format!(r#"<p class="muted">{}</p>"#, escaped(text)))
        .collect::<String>();
    let steps = guide["steps"].as_array().into_iter().flatten().filter_map(|step| {
        let title = escaped(step["title"].as_str()?);
        let description = step["desc"].as_str().unwrap_or_default();
        let value = step["value"].as_str().map(|value| {
            let copy = if step["copyable"] == true {
                r#"<button type="button" class="button" data-guide-copy>Copy</button>"#
            } else { "" };
            format!(r#"<div class="key-actions"><code data-guide-template>{}</code>{copy}</div>"#, escaped(value))
        }).unwrap_or_default();
        Some(format!(r#"<li><strong>{title}</strong><p class="muted">{}</p>{value}</li>"#, escaped(description)))
    }).collect::<String>();
    let steps = format!("<ol>{steps}</ol>");
    let code = guide["code"]["code"].as_str().map(|code| {
        let language = escaped(guide["code"]["language"].as_str().unwrap_or_default());
        format!(r#"<div><pre class="usage-log" data-guide-template data-guide-language="{language}">{}</pre><button type="button" class="button" data-guide-copy>Copy configuration</button></div>"#, escaped(code))
    }).unwrap_or_default();
    let content = render(
        CLI_GUIDE,
        &[
            ("title", &escaped(title)),
            ("desc", &escaped(desc)),
            ("id", &escaped(tool)),
            (
                "external",
                if guide["requiresExternalUrl"] == true {
                    "true"
                } else {
                    "false"
                },
            ),
            ("status-hidden", if tool == "devin" { "" } else { "hidden" }),
            (
                "values-hidden",
                if matches!(tool, "devin" | "opendesign") {
                    "hidden"
                } else {
                    ""
                },
            ),
            ("notes", &notes),
            ("steps", &steps),
            ("code", &code),
        ],
    );
    render_shell(
        title,
        CLI_PATH,
        "cli-guide",
        &content,
        session,
        Some(SCRIPT_PATH),
    )
}

async fn cli_status(State(state): State<IntegrationsUi>, request: Request) -> Response {
    if let Err(path) = protected(&state.storage, request.headers()).await {
        return redirect(path);
    }
    let requested = request.uri().query().and_then(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(name, value)| (name == "tool").then(|| value.into_owned()))
    });
    if let Some(id) = requested {
        let status = crate::cli_tools::tool_status(&id).await;
        return status_response(json!({ (id):status }));
    }
    let statuses =
        futures_util::future::join_all(tools().iter().filter_map(|tool| tool["id"].as_str()).map(
            |id| async move {
                let value = crate::cli_tools::tool_status(id).await.unwrap_or_default();
                (id.to_owned(), value)
            },
        ))
        .await;
    status_response(Value::Object(statuses.into_iter().collect()))
}

fn status_response(value: Value) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(value.to_string()))
        .expect("CLI status response")
}
