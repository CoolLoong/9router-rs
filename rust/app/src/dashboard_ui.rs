use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::Value;

use crate::dashboard_session::{has_session, is_tunnel, oidc_configured, saml_configured};
use crate::provider_connections::truthy;
use crate::provider_index::js_trim;
use crate::settings::effective;
use crate::storage::Storage;

#[path = "dashboard_catalog_ui.rs"]
mod catalog_ui;
#[path = "dashboard_chat_ui.rs"]
mod chat_ui;
#[path = "dashboard_integrations_ui.rs"]
mod integrations_ui;
#[path = "dashboard_management_ui.rs"]
mod management_ui;
#[path = "dashboard_optimization_ui.rs"]
mod optimization_ui;
#[path = "translator_ui.rs"]
mod translator_ui;
#[path = "dashboard_usage_ui.rs"]
mod usage_ui;

const LOGIN_HTML: &str = include_str!("dashboard_assets/login.html");
const SHELL_HTML: &str = include_str!("dashboard_assets/shell.html");
const ENDPOINT_HTML: &str = include_str!("dashboard_assets/endpoint.html");
const STYLE: &str = include_str!("dashboard_assets/dashboard.css");
const SCRIPT: &str = include_str!("dashboard_assets/dashboard.js");
pub(super) const QUOTA_AUTO_PING_DIALOG: &str =
    include_str!("dashboard_assets/quota-auto-ping.html");
const NATIVE_UPDATE_UI: bool = cfg!(all(
    feature = "native-package",
    any(target_os = "linux", windows, target_os = "macos")
));
const NATIVE_INSTALL_UI: bool = cfg!(all(
    feature = "native-package",
    any(target_os = "linux", windows)
));
const LICENSE: &str = include_str!("../../../LICENSE");
const POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

#[derive(Clone)]
struct DashboardUi {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    let catalog = catalog_ui::router(storage.clone());
    let chat = chat_ui::router(storage.clone());
    let integrations = integrations_ui::router(storage.clone());
    let management = management_ui::router(storage.clone());
    let optimization = optimization_ui::router(storage.clone());
    let usage = usage_ui::router(storage.clone());
    let translator = translator_ui::router(storage.clone());
    Router::new()
        .route("/", get(index))
        .route("/landing", get(landing))
        .route("/__9router-ui/landing.css", get(landing_style))
        .route("/__9router-ui/landing.js", get(landing_script))
        .route("/login", get(login))
        .route("/dashboard", get(endpoint))
        .route("/dashboard/endpoint", get(endpoint))
        .route("/dashboard/settings/pricing", get(pricing))
        .route("/dashboard/skills", get(skills))
        .route("/dashboard/console-log", get(console))
        .route("/__9router-ui/pricing.js", get(pricing_script))
        .route("/__9router-ui/skills.js", get(skills_script))
        .route("/__9router-ui/console.js", get(console_script))
        .route("/__9router-ui/tunnel.js", get(tunnel_script))
        .route("/__9router-ui/dashboard.css", get(style))
        .route("/__9router-ui/dashboard.js", get(script))
        .route("/__9router-ui/updates.js", get(updates_script))
        .route(
            "/__9router-ui/quota-auto-ping.js",
            get(quota_auto_ping_script),
        )
        .route("/__9router-ui/license.txt", get(license))
        .with_state(DashboardUi { storage })
        .merge(catalog)
        .merge(chat)
        .merge(integrations)
        .merge(management)
        .merge(optimization)
        .merge(usage)
        .merge(translator)
}

async fn quota_auto_ping_script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("dashboard_assets/quota-auto-ping.js"),
    )
}

pub(super) fn redirect(path: &'static str) -> Response {
    (
        StatusCode::TEMPORARY_REDIRECT,
        [(header::LOCATION, path)],
        Body::from(path),
    )
        .into_response()
}

async fn index() -> Response {
    redirect("/dashboard")
}

fn page(html: impl Into<Body>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, POLICY),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        html.into(),
    )
        .into_response()
}

pub(crate) fn asset(content_type: &'static str, text: &'static str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Body::from(text),
    )
        .into_response()
}

async fn landing() -> Response {
    page(include_str!("dashboard_assets/landing.html"))
}

async fn landing_style() -> Response {
    asset(
        "text/css; charset=utf-8",
        include_str!("dashboard_assets/landing.css"),
    )
}

async fn landing_script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("dashboard_assets/landing.js"),
    )
}

async fn style() -> Response {
    asset("text/css; charset=utf-8", STYLE)
}

async fn script() -> Response {
    asset("text/javascript; charset=utf-8", SCRIPT)
}

async fn updates_script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("dashboard_assets/updates.js"),
    )
}

async fn license() -> Response {
    asset("text/plain; charset=utf-8", LICENSE)
}

pub(crate) fn render(template: &str, slots: &[(&str, &str)]) -> String {
    let mut result = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some((text, marker)) = remaining.split_once("{{") {
        let Some((name, tail)) = marker.split_once("}}") else {
            break;
        };
        result.push_str(text);
        if let Some((_, value)) = slots.iter().find(|(key, _)| *key == name) {
            result.push_str(value);
        }
        remaining = tail;
    }
    result.push_str(remaining);
    result
}

pub(crate) fn escaped(value: &str) -> String {
    let mut safe = String::with_capacity(value.len());
    for char in value.chars() {
        match char {
            '&' => safe.push_str("&amp;"),
            '<' => safe.push_str("&lt;"),
            '>' => safe.push_str("&gt;"),
            '"' => safe.push_str("&quot;"),
            '\'' => safe.push_str("&#39;"),
            _ => safe.push(char),
        }
    }
    safe
}

fn tunneled_access_denied(headers: &HeaderMap, settings: &Value) -> bool {
    settings["tunnelDashboardAccess"] != true && is_tunnel(headers, settings).unwrap_or(false)
}

fn labeled(settings: &Value, field: &str, fallback: &str) -> String {
    escaped(
        settings[field]
            .as_str()
            .map(js_trim)
            .filter(|text| !text.is_empty())
            .unwrap_or(fallback),
    )
}

fn mode(settings: &Value) -> (&str, &str) {
    let auth = settings["authMode"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or("password");
    let sso = settings["ssoType"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or(if auth == "saml" { "saml" } else { "oidc" });
    (auth, sso)
}

async fn login(State(state): State<DashboardUi>, request: Request) -> Response {
    let settings = effective(state.storage.read_settings().await.unwrap_or_default());
    let closed_to_tunnel = tunneled_access_denied(request.headers(), &settings);
    let explicit_sign_in = request.uri().query().is_some_and(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .any(|(key, value)| key == "sign-in" && value == "1")
    });
    let ready = !closed_to_tunnel
        && (settings["requireLogin"] == false && !explicit_sign_in
            || has_session(&state.storage, request.headers()));
    let (auth, sso) = mode(&settings);
    let has_sso = matches!(auth, "sso" | "oidc" | "saml" | "both");
    let saml = has_sso && sso == "saml" && saml_configured(&settings);
    let oidc = has_sso && sso == "oidc" && oidc_configured(&settings) == Some(true);
    let has_password = !saml && !oidc || matches!(auth, "password" | "both");

    let label = if saml {
        labeled(&settings, "samlLoginLabel", "Sign in with SAML SSO")
    } else {
        labeled(&settings, "oidcLoginLabel", "Sign in with OIDC")
    };
    let sso_link = if saml || oidc {
        let route = if saml {
            "/api/auth/saml/start"
        } else {
            "/api/auth/oidc/start"
        };
        format!(r#"<a class="button button-primary" href="{route}">{label}</a>"#)
    } else {
        String::new()
    };
    let status = if closed_to_tunnel {
        "Dashboard access via this tunnel is disabled."
    } else if ready {
        "You’re signed in."
    } else if saml {
        "Sign in with SAML single sign-on."
    } else if oidc {
        "Sign in with your OIDC provider."
    } else {
        "Enter your password to access the dashboard."
    };
    let defaults_hint = if has_password
        && !truthy(&settings["password"])
        && std::env::var("INITIAL_PASSWORD")
            .ok()
            .is_none_or(|value| value.is_empty())
    {
        r#"<p class="muted login-hint">Default password: <code>123456</code>. Set a password before enabling remote dashboard access.</p>"#
    } else {
        ""
    };
    let recovery = if has_sso && !saml && !oidc {
        r#"<p class="notice notice-warning">Single sign-on is incomplete. Password login is available for recovery.</p>"#
    } else {
        ""
    };
    let error = request
        .uri()
        .query()
        .and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes()).find_map(|(name, value)| {
                (name == "error").then(|| escaped(&value.chars().take(256).collect::<String>()))
            })
        })
        .unwrap_or_default();

    page(render(
        LOGIN_HTML,
        &[
            ("description", status),
            ("ready", if ready { "true" } else { "false" }),
            ("continue-hidden", if ready { "" } else { "hidden" }),
            (
                "controls-hidden",
                if ready || closed_to_tunnel {
                    "hidden"
                } else {
                    ""
                },
            ),
            ("password-hidden", if has_password { "" } else { "hidden" }),
            ("sso", &sso_link),
            (
                "divider",
                if (saml || oidc) && has_password {
                    r#"<div class="login-divider">or</div>"#
                } else {
                    ""
                },
            ),
            ("recovery", recovery),
            ("defaults", defaults_hint),
            ("error-hidden", if error.is_empty() { "hidden" } else { "" }),
            ("error", &error),
        ],
    ))
}

fn session(storage: &Storage, headers: &HeaderMap, settings: &Value) -> Result<bool, &'static str> {
    let present = has_session(storage, headers);
    if tunneled_access_denied(headers, settings) || settings["requireLogin"] != false && !present {
        return Err("/login");
    }
    Ok(present)
}

pub(crate) async fn protected(
    storage: &Storage,
    headers: &HeaderMap,
) -> Result<bool, &'static str> {
    let settings = effective(storage.read_settings().await.unwrap_or_default());
    session(storage, headers, &settings)
}

pub(crate) fn render_shell(
    title: &str,
    active_path: &str,
    body_page: &str,
    content: &str,
    session: bool,
    script: Option<&str>,
) -> Response {
    let title = escaped(title);
    let body_page = escaped(body_page);
    let script = script
        .map(|path| format!(r#"<script defer src="{}"></script>"#, escaped(path)))
        .unwrap_or_default();
    let current = |path| {
        if active_path == path {
            r#"aria-current="page""#
        } else {
            ""
        }
    };
    let translator = if active_path == "/dashboard/translator"
        || std::env::var("ENABLE_TRANSLATOR").as_deref() == Ok("true")
    {
        format!(
            r#"<a href="/dashboard/translator" {}>Translator</a>"#,
            current("/dashboard/translator")
        )
    } else {
        String::new()
    };
    page(render(
        SHELL_HTML,
        &[
            ("title", &title),
            ("page", &body_page),
            ("script", &script),
            ("session-hidden", if session { "" } else { "hidden" }),
            ("endpoint-current", current("/dashboard/endpoint")),
            ("providers-current", current("/dashboard/providers")),
            ("models-current", current("/dashboard/models")),
            ("combos-current", current("/dashboard/combos")),
            ("chat-current", current("/dashboard/basic-chat")),
            ("cli-current", current("/dashboard/cli-tools")),
            (
                "media-web-current",
                current("/dashboard/media-providers/web"),
            ),
            (
                "media-embedding-current",
                current("/dashboard/media-providers/embedding"),
            ),
            (
                "media-tts-current",
                current("/dashboard/media-providers/tts"),
            ),
            (
                "media-stt-current",
                current("/dashboard/media-providers/stt"),
            ),
            (
                "media-image-current",
                current("/dashboard/media-providers/image"),
            ),
            (
                "media-video-current",
                current("/dashboard/media-providers/video"),
            ),
            (
                "media-systemone-current",
                current("/dashboard/media-providers/systemone"),
            ),
            ("usage-current", current("/dashboard/usage")),
            ("console-current", current("/dashboard/console-log")),
            ("quota-current", current("/dashboard/quota")),
            ("pricing-current", current("/dashboard/settings/pricing")),
            ("skills-current", current("/dashboard/skills")),
            ("proxy-pools-current", current("/dashboard/proxy-pools")),
            ("token-saver-current", current("/dashboard/token-saver")),
            ("pxpipe-current", current("/dashboard/pxpipe")),
            ("translator-link", &translator),
            (
                "update-link",
                if NATIVE_UPDATE_UI {
                    r#"<a href="/dashboard/endpoint#native-update">Updates</a>"#
                } else {
                    ""
                },
            ),
            ("profile-current", current("/dashboard/profile")),
            ("content", content),
        ],
    ))
}

async fn endpoint(State(state): State<DashboardUi>, request: Request) -> Response {
    let settings = effective(state.storage.read_settings().await.unwrap_or_default());
    let session = match session(&state.storage, request.headers(), &settings) {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let tunnel_panel = request.uri().path() == "/dashboard/endpoint"
        && request.uri().query().is_some_and(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .any(|(key, value)| key == "panel" && value == "tunnel")
        });
    if tunnel_panel {
        return render_shell(
            "Tunnel & Tailscale",
            "/dashboard/endpoint",
            "tunnel",
            include_str!("dashboard_assets/tunnel.html"),
            session,
            Some("/__9router-ui/tunnel.js"),
        );
    }
    let updates = if NATIVE_UPDATE_UI {
        render(
            include_str!("dashboard_assets/updates.html"),
            &[
                (
                    "can-install",
                    if NATIVE_INSTALL_UI { "true" } else { "false" },
                ),
                ("signed-in", if session { "true" } else { "false" }),
            ],
        )
    } else {
        String::new()
    };
    let content = render(
        ENDPOINT_HTML,
        &[
            (
                "require-key",
                if truthy(&settings["requireApiKey"]) {
                    "checked"
                } else {
                    ""
                },
            ),
            ("native-updates", &updates),
        ],
    );
    render_shell(
        "Endpoint & Key",
        "/dashboard/endpoint",
        "endpoint",
        &content,
        session,
        NATIVE_UPDATE_UI.then_some("/__9router-ui/updates.js"),
    )
}

async fn pricing(State(state): State<DashboardUi>, request: Request) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    render_shell(
        "Pricing Settings",
        "/dashboard/settings/pricing",
        "pricing",
        include_str!("dashboard_assets/pricing.html"),
        session,
        Some("/__9router-ui/pricing.js"),
    )
}

async fn pricing_script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("dashboard_assets/pricing.js"),
    )
}

async fn skills(State(state): State<DashboardUi>, request: Request) -> Response {
    use std::fmt::Write;

    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let data: Value = serde_json::from_str(include_str!("dashboard_assets/skills.json"))
        .expect("pinned skill catalog");
    let root = escaped(data["raw"].as_str().unwrap_or_default());
    let blob = escaped(data["blob"].as_str().unwrap_or_default());
    let repository = escaped(data["repository"].as_str().unwrap_or_default());
    let mut content = format!(
        r#"<div class="page-heading"><h1>Skills</h1></div>
<section class="card" aria-label="Setup"><code>Read this skill and use it: {root}/9router/SKILL.md</code></section>
<p id="skill-status" class="notice" role="status" hidden></p>
<section class="usage-view" aria-label="Available skills">"#
    );
    for skill in data["skills"].as_array().into_iter().flatten() {
        let id = escaped(skill["id"].as_str().unwrap_or_default());
        let name = escaped(skill["name"].as_str().unwrap_or_default());
        let text = escaped(skill["description"].as_str().unwrap_or_default());
        let endpoint = skill["endpoint"].as_str().map_or_else(String::new, |path| {
            format!("<p><code>{}</code></p>", escaped(path))
        });
        let _ = write!(
            content,
            r#"<article class="card"><div class="card-heading"><h2>{name}</h2><button class="button" type="button" data-skill-link="{root}/{id}/SKILL.md">Copy link</button></div><p>{text}</p>{endpoint}<a href="{blob}/{id}/SKILL.md">{root}/{id}/SKILL.md</a></article>"#
        );
    }
    let _ = write!(
        content,
        r#"</section><p><a href="{repository}/tree/master/skills">Browse skills on GitHub</a></p>"#
    );
    render_shell(
        "Skills",
        "/dashboard/skills",
        "skills",
        &content,
        session,
        Some("/__9router-ui/skills.js"),
    )
}

async fn skills_script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("dashboard_assets/skills.js"),
    )
}

async fn console(State(state): State<DashboardUi>, request: Request) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    render_shell(
        "Console log",
        "/dashboard/console-log",
        "console",
        include_str!("dashboard_assets/console.html"),
        session,
        Some("/__9router-ui/console.js"),
    )
}

async fn console_script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("dashboard_assets/console.js"),
    )
}

async fn tunnel_script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("dashboard_assets/tunnel.js"),
    )
}
