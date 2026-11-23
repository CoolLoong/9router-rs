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
#[path = "dashboard_frontend.rs"]
mod frontend;
#[path = "dashboard_management_ui.rs"]
mod management_ui;
#[path = "translator_api.rs"]
mod translator;

const LOGIN_HTML: &str = include_str!("../assets/native-web/login.html");
const SHELL_HTML: &str = include_str!("../assets/native-web/shell.html");
const STYLE: &str = include_str!("../assets/native-web/dashboard.css");
const SCRIPT: &str = include_str!("../assets/native-web/dashboard.js");
const NATIVE_UPDATE_UI: bool = cfg!(all(
    feature = "native-package",
    any(target_os = "linux", windows, target_os = "macos")
));
const NATIVE_INSTALL_UI: bool = cfg!(all(
    feature = "native-package",
    any(target_os = "linux", windows)
));
const LICENSE: &str = include_str!("../LICENSE");
const POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

#[derive(Clone)]
struct DashboardUi {
    storage: Storage,
}

pub fn router(storage: Storage) -> Router {
    let mut native = Router::new()
        .route("/", get(index))
        .route("/landing", get(landing))
        .route("/__9router-ui/landing.css", get(landing_style))
        .route("/__9router-ui/landing.js", get(landing_script))
        .route("/login", get(login))
        .route("/__9router-ui/dashboard.css", get(style))
        .route("/__9router-ui/dashboard.js", get(script))
        .route("/__9router-ui/updates.js", get(updates_script))
        .route("/__9router-ui/license.txt", get(license));
    if NATIVE_UPDATE_UI {
        native = native.route("/dashboard/updates", get(updates));
    }
    native
        .with_state(DashboardUi {
            storage: storage.clone(),
        })
        .merge(catalog_ui::router(storage.clone()))
        .merge(management_ui::router(storage.clone()))
        .merge(translator::router(storage.clone()))
        .merge(frontend::router(storage))
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
    page(include_str!("../assets/native-web/landing.html"))
}

async fn landing_style() -> Response {
    asset(
        "text/css; charset=utf-8",
        include_str!("../assets/native-web/landing.css"),
    )
}

async fn landing_script() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("../assets/native-web/landing.js"),
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
        include_str!("../assets/native-web/updates.js"),
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
                    r#"<a href="/dashboard/updates">Updates</a>"#
                } else {
                    ""
                },
            ),
            ("profile-current", current("/dashboard/profile")),
            ("content", content),
        ],
    ))
}

async fn updates(State(state): State<DashboardUi>, request: Request) -> Response {
    let session = match protected(&state.storage, request.headers()).await {
        Ok(session) => session,
        Err(path) => return redirect(path),
    };
    let content = render(
        include_str!("../assets/native-web/updates.html"),
        &[
            (
                "can-install",
                if NATIVE_INSTALL_UI { "true" } else { "false" },
            ),
            ("signed-in", if session { "true" } else { "false" }),
        ],
    );
    render_shell(
        "Updates",
        "/dashboard/updates",
        "updates",
        &content,
        session,
        Some("/__9router-ui/updates.js"),
    )
}
