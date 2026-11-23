use std::collections::HashMap;
use std::io::Read;
use std::sync::LazyLock;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::read::GzDecoder;
use percent_encoding::percent_decode_str;
use sha2::{Digest, Sha256};

use super::{protected, redirect};
use crate::{key_identity, storage::Storage};

const CLIENT_ID: &str = "__RUST_DASHBOARD_CLIENT_ID__";
const VARY: &str =
    "RSC, Next-Router-State-Tree, Next-Router-Prefetch, Next-Router-Segment-Prefetch, Next-Url";
const BUNDLE: &[u8] = include_bytes!("dashboard_original.tar.gz");

static FILES: LazyLock<HashMap<String, Bytes>> =
    LazyLock::new(|| read_bundle().expect("embedded dashboard browser assets"));

fn read_bundle() -> std::io::Result<HashMap<String, Bytes>> {
    let mut archive = tar::Archive::new(GzDecoder::new(BUNDLE));
    let mut files = HashMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() {
            let name = entry.path()?.to_string_lossy().into_owned();
            let mut body = Vec::new();
            entry.read_to_end(&mut body)?;
            files.insert(name, Bytes::from(body));
        }
    }
    Ok(files)
}

pub(super) fn assets() -> Router {
    Router::new()
        .route("/_next/static/{*asset}", get(serve_asset))
        .route("/providers/{*asset}", get(serve_asset))
        .route("/i18n/literals/{*asset}", get(serve_asset))
        .route("/icons/{*asset}", get(serve_asset))
        .route("/favicon.svg", get(serve_asset))
        .route("/favicon.ico", get(serve_asset))
        .route("/manifest.webmanifest", get(serve_asset))
}

async fn serve_asset(request: Request) -> Response {
    let Ok(path) = percent_decode_str(request.uri().path()).decode_utf8() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(body) = FILES.get(&format!("asset{path}")) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = mime_guess::from_path(path.as_ref()).first_or_octet_stream();
    let content_type = if path == "/manifest.webmanifest" {
        "application/manifest+json"
    } else {
        mime.essence_str()
    };
    let cache = if path.starts_with("/_next/static/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, cache),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body.clone(),
    )
        .into_response()
}

pub(super) async fn render(
    State(storage): State<Storage>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if request.method() != Method::GET || !(path == "/dashboard" || path.starts_with("/dashboard/"))
    {
        return next.run(request).await;
    }

    let headers = request.headers();
    let flight = headers.get("rsc").is_some_and(|value| value == "1");
    let Some((raw, substitution)) = template(path, if flight { "rsc" } else { "html" }) else {
        return next.run(request).await;
    };
    if let Err(path) = protected(&storage, headers).await {
        return redirect(path);
    }
    if flight && headers.contains_key("next-router-segment-prefetch") {
        return (StatusCode::NO_CONTENT, [(header::VARY, VARY)]).into_response();
    }

    let mut document = raw.to_owned();
    if let Some((marker, value)) = substitution {
        document = document.replace(marker, value);
    }
    if document.contains(CLIENT_ID) {
        let machine = key_identity::machine_id(storage.root()).unwrap_or_default();
        document = document.replace(CLIENT_ID, &machine);
    }

    let csp = (!flight).then(|| policy(&document));
    let mut response = (
        [
            (
                header::CONTENT_TYPE,
                if flight {
                    "text/x-component"
                } else {
                    "text/html; charset=utf-8"
                },
            ),
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, VARY),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        document,
    )
        .into_response();
    if let Some(csp) = csp.and_then(|value| HeaderValue::from_str(&value).ok()) {
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    response
}

fn page(path: &str, extension: &str) -> Option<&'static str> {
    std::str::from_utf8(FILES.get(&format!("page{path}.{extension}"))?).ok()
}

fn template<'a>(
    path: &'a str,
    extension: &str,
) -> Option<(&'static str, Option<(&'static str, &'a str)>)> {
    if let Some(text) = page(path, extension) {
        return Some((text, None));
    }
    let (prefix, marker, template_path) = [
        (
            "/dashboard/providers/",
            "rustdashproviderplaceholder",
            "/dashboard/providers/rustdashproviderplaceholder",
        ),
        (
            "/dashboard/media-providers/combo/",
            "rustdashcomboplaceholder",
            "/dashboard/media-providers/combo/rustdashcomboplaceholder",
        ),
        (
            "/dashboard/media-providers/embedding/custom-embedding-",
            "rustdashcustomplaceholder",
            "/dashboard/media-providers/embedding/custom-embedding-rustdashcustomplaceholder",
        ),
    ]
    .into_iter()
    .find(|(prefix, _, _)| path.starts_with(prefix))?;
    let segment = path.strip_prefix(prefix)?;
    if segment.is_empty()
        || segment.len() > 160
        || !segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    Some((page(template_path, extension)?, Some((marker, segment))))
}

fn policy(document: &str) -> String {
    let hashes = document
        .split("<script")
        .skip(1)
        .filter_map(|tag| {
            let (attributes, text) = tag.split_once('>')?;
            if attributes.contains("src=") {
                return None;
            }
            let (source, _) = text.split_once("</script>")?;
            Some(format!(
                " 'sha256-{}'",
                STANDARD.encode(Sha256::digest(source))
            ))
        })
        .collect::<String>();
    format!(
        "default-src 'none'; script-src 'self'{hashes}; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self' blob:; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
    )
}
