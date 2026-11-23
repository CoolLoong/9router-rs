use std::fmt::Write as _;
use std::io::Write as _;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{FromRequest, Multipart, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{SecondsFormat, Utc};
use flate2::Compression;
use flate2::write::DeflateEncoder;
use roxmltree::Document;
use saml_rs::raw::metadata::{Endpoint, IdpMetadataConfig, SpMetadataConfig};
use saml_rs::raw::{Binding, EntitySetting, HttpRequest, IdentityProvider, ServiceProvider};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use url::Url;

use crate::chat_input::truthy;
use crate::chat_output::js_text;
use crate::dashboard_session::{
    SessionState, caller, has_session, issued_cookie, saml_configured, secure_cookie,
};
use crate::oidc::{checked_origin, public_origin, redirect, redirect_error};
use crate::provider_index::js_trim;
use crate::settings::{effective, reply};
use crate::storage::Storage;

const LIMIT: usize = 2 * 1024 * 1024;
const ASSERTION: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
const PROTOCOL: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
const EMAIL: &str = "urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress";
const POST: &str = "urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST";
const ACS: &str = "/api/auth/saml/acs";
const METADATA: &str = "/api/auth/saml/metadata";
const ENTITY: &str = "urn:9router:sp";
const AGE: i64 = 600;

pub fn routes() -> Router<SessionState> {
    Router::new()
        .route(ACS, post(consume))
        .route(METADATA, get(metadata))
        .route("/api/auth/saml/start", get(start))
        .route("/api/auth/saml/test", post(test))
}

fn origin(request: &Request, settings: &Value) -> Result<String, String> {
    let configured = text(settings, "baseUrl");
    if configured.is_empty() {
        public_origin(request)
    } else {
        checked_origin(&configured)
    }
}

fn own_origin(request: &Request) -> Result<String, String> {
    let authority = request
        .uri()
        .authority()
        .map(|value| value.as_str())
        .or_else(|| {
            request
                .headers()
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
        })
        .unwrap_or_default();
    checked_origin(&format!(
        "{}://{authority}",
        request.uri().scheme_str().unwrap_or("http")
    ))
}

fn text(value: &Value, field: &str) -> String {
    value
        .get(field)
        .filter(|value| truthy(value))
        .map(js_text)
        .map_or_else(String::new, |text| js_trim(&text).into())
}

fn entity(settings: &Value) -> String {
    let configured = text(settings, "samlIssuer");
    if configured.is_empty() {
        ENTITY.into()
    } else {
        configured
    }
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\r', "&#xD;")
        .replace('\n', "&#xA;")
}

fn generate_id() -> Result<String, String> {
    let mut random = [0; 20];
    getrandom::fill(&mut random).map_err(|_| "Could not initialize SAML sign in".to_owned())?;
    let mut id = String::from("_");
    for byte in random {
        write!(&mut id, "{byte:02x}")
            .map_err(|_| "Could not initialize SAML sign in".to_owned())?;
    }
    Ok(id)
}

fn state_cookie(id: &str, headers: &HeaderMap) -> Option<HeaderValue> {
    let expires = (Utc::now() + chrono::Duration::seconds(AGE)).format("%a, %d %b %Y %H:%M:%S GMT");
    let secure = if secure_cookie(headers) {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "saml_state={id}; Path=/; Expires={expires}; Max-Age={AGE}{secure}; HttpOnly; SameSite=lax"
    ))
    .ok()
}

fn clear(response: &mut Response) {
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static("saml_state=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT"),
    );
}

fn saml_error(origin: &str, reason: &str) -> Response {
    redirect_error(origin, reason, false)
}

async fn start(State(state): State<SessionState>, request: Request) -> Response {
    let settings = state
        .storage
        .read_settings()
        .await
        .map(effective)
        .unwrap_or_default();
    let origin = origin(&request, &settings).unwrap_or_else(|_| "http://localhost:20128".into());
    if !saml_configured(&settings) {
        return saml_error(&origin, "saml_not_configured");
    }
    match authorization(&state.storage, &settings, &origin).await {
        Ok((url, request_id)) => {
            let mut response = redirect(url.as_str());
            if let Some(cookie) = state_cookie(&request_id, request.headers()) {
                response.headers_mut().append(header::SET_COOKIE, cookie);
            }
            response
        }
        Err(error) => saml_error(&origin, &error),
    }
}

async fn authorization(
    storage: &Storage,
    settings: &Value,
    origin: &str,
) -> Result<(Url, String), String> {
    let destination = text(settings, "samlEntryPoint");
    let mut url = Url::parse(&destination)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .ok_or_else(|| "Invalid URL".to_owned())?;
    let issue = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let id = generate_id()?;
    let sp = xml(&entity(settings));
    let destination = xml(&destination);
    let reply = xml(&format!("{origin}{ACS}"));
    let message = format!(
        "<?xml version=\"1.0\"?><samlp:AuthnRequest xmlns:samlp=\"{PROTOCOL}\" ID=\"{id}\" Version=\"2.0\" IssueInstant=\"{issue}\" ProtocolBinding=\"{POST}\" Destination=\"{destination}\" AssertionConsumerServiceURL=\"{reply}\"><saml:Issuer xmlns:saml=\"{ASSERTION}\">{sp}</saml:Issuer><samlp:NameIDPolicy xmlns:samlp=\"{PROTOCOL}\" AllowCreate=\"true\" Format=\"{EMAIL}\"/><samlp:RequestedAuthnContext xmlns:samlp=\"{PROTOCOL}\" Comparison=\"exact\"><saml:AuthnContextClassRef xmlns:saml=\"{ASSERTION}\">urn:oasis:names:tc:SAML:2.0:ac:classes:PasswordProtectedTransport</saml:AuthnContextClassRef></samlp:RequestedAuthnContext></samlp:AuthnRequest>",
    );
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(message.as_bytes())
        .map_err(|error| error.to_string())?;
    let bytes = encoder.finish().map_err(|error| error.to_string())?;
    let encoded = STANDARD.encode(bytes);
    let query: Vec<_> = url
        .query_pairs()
        .filter(|(name, _)| name != "SAMLRequest")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(query)
        .append_pair("SAMLRequest", &encoded);
    let request_id = id.clone();
    storage.run(true, move |db| {
        let now = Utc::now().timestamp();
        db.execute("DELETE FROM kv WHERE scope IN ('saml-pending', 'saml-consumed') AND CAST(value AS INTEGER) < ?1", [now])?;
        db.execute("INSERT INTO kv(scope,key,value) VALUES('saml-pending',?1,?2)", (request_id, (now + AGE).to_string()))?;
        Ok(())
    }).await.map_err(|_| "Could not store SAML request state".to_owned())?;
    Ok((url, id))
}

async fn metadata(State(state): State<SessionState>, request: Request) -> Response {
    let settings = state
        .storage
        .read_settings()
        .await
        .map(effective)
        .unwrap_or_default();
    let host = own_origin(&request).unwrap_or_else(|_| "http://localhost:20128".into());
    let metadata = generate_id().map(|id| format!(
        "<?xml version=\"1.0\"?>\n<EntityDescriptor xmlns=\"urn:oasis:names:tc:SAML:2.0:metadata\" xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\" entityID=\"{entity}\" ID=\"{id}\">\n  <SPSSODescriptor protocolSupportEnumeration=\"{PROTOCOL}\" AuthnRequestsSigned=\"false\" WantAssertionsSigned=\"true\">\n    <NameIDFormat>{EMAIL}</NameIDFormat>\n    <AssertionConsumerService index=\"1\" isDefault=\"true\" Binding=\"{POST}\" Location=\"{host}{ACS}\"/>\n  </SPSSODescriptor>\n</EntityDescriptor>",
        entity=xml(&entity(&settings)), host=xml(&host)
    ));
    let (status, body) = match metadata {
        Ok(body) => (StatusCode::OK, body),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("<?xml version=\"1.0\"?><Error>{}</Error>", xml(&error)),
        ),
    };
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml"),
    );
    response.headers_mut().insert(header::VARY, HeaderValue::from_static("rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding"));
    if status.is_success() {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    response
}

fn cert(raw: &str) -> String {
    raw.replace("-----BEGIN CERTIFICATE-----", "")
        .replace("-----END CERTIFICATE-----", "")
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '='))
        .collect()
}

async fn test(State(state): State<SessionState>, request: Request) -> Response {
    let settings = state
        .storage
        .read_settings()
        .await
        .map(effective)
        .unwrap_or_default();
    if settings["requireLogin"] != false && !has_session(&state.storage, request.headers()) {
        return reply(
            StatusCode::UNAUTHORIZED,
            json!({"error":"Unauthorized"}),
            false,
        );
    }
    let host = own_origin(&request).unwrap_or_else(|_| "http://localhost:20128".into());
    let bytes = to_bytes(request.into_body(), LIMIT)
        .await
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let option = |key: &str| {
        if truthy(&body[key]) {
            text(&body, key)
        } else {
            text(&settings, key)
        }
    };
    let endpoint = option("samlEntryPoint");
    let issuer = option("samlIssuer");
    let certificate = if let Some(value) = body.get("samlCert") {
        js_trim(&js_text(value)).to_owned()
    } else {
        text(&settings, "samlCert")
    };
    let failure = if endpoint.is_empty() {
        Some("Single Sign-On Service URL (samlEntryPoint) is required")
    } else if Url::parse(&endpoint).is_err() {
        Some("Single Sign-On Service URL must be a valid URL")
    } else if issuer.is_empty() {
        Some("SP Entity ID / Issuer (samlIssuer) is required")
    } else if certificate.is_empty() {
        Some("IdP X.509 Certificate (samlCert) is required")
    } else if cert(&certificate).is_empty() {
        Some("Invalid IdP X.509 Certificate format")
    } else {
        None
    };
    if let Some(message) = failure {
        return reply(StatusCode::BAD_REQUEST, json!({"error":message}), false);
    }
    reply(
        StatusCode::OK,
        json!({
            "ok":true, "samlEntryPoint":endpoint, "samlIssuer":issuer, "certValid":true,
            "acsUrl":format!("{host}{ACS}"), "metadataUrl":format!("{host}{METADATA}"),
            "message":"SAML 2.0 configuration verified successfully.",
        }),
        false,
    )
}

fn request_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';').map(js_trim))
        .find_map(|part| part.strip_prefix("saml_state="))
        .filter(|text| !text.is_empty())
        .and_then(|text| {
            percent_encoding::percent_decode_str(text)
                .decode_utf8()
                .ok()
        })
        .map(|text| text.into_owned())
}

async fn assertion(request: Request) -> Result<Option<String>, String> {
    let mime = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|header| header.to_str().ok())
        .unwrap_or("");
    if mime.starts_with("multipart/form-data") {
        let mut form = Multipart::from_request(request, &())
            .await
            .map_err(|_| "Invalid SAML form data".to_owned())?;
        while let Some(field) = form
            .next_field()
            .await
            .map_err(|_| "Invalid SAML form data".to_owned())?
        {
            if field.name() == Some("SAMLResponse") {
                return field
                    .text()
                    .await
                    .map(Some)
                    .map_err(|_| "Invalid SAML form data".to_owned());
            }
        }
        return Ok(None);
    }
    if !mime.starts_with("application/x-www-form-urlencoded") {
        return Err("Content-Type was not one of \"multipart/form-data\" or \"application/x-www-form-urlencoded\".".into());
    }
    let bytes = to_bytes(request.into_body(), LIMIT)
        .await
        .map_err(|_| "Invalid SAML form data".to_owned())?;
    Ok(url::form_urlencoded::parse(&bytes)
        .find_map(|(name, value)| (name == "SAMLResponse").then(|| value.into_owned())))
}

async fn consume(State(state): State<SessionState>, request: Request) -> Response {
    let settings = state
        .storage
        .read_settings()
        .await
        .map(effective)
        .unwrap_or_default();
    let origin = origin(&request, &settings).unwrap_or_else(|_| "http://localhost:20128".into());
    let (ip, _) = caller(&request);
    if let Some(seconds) = state.check_lock(&ip) {
        return saml_error(
            &origin,
            &format!("Too many failed attempts. Try again in {seconds}s."),
        );
    }
    let headers = request.headers().clone();
    let expected = request_cookie(&headers);
    let result = async {
        let raw = assertion(request)
            .await?
            .filter(|data| !data.is_empty())
            .ok_or_else(|| "saml_missing_response".to_owned())?;
        if !saml_configured(&settings) {
            return Err("saml_not_configured".into());
        }
        let result = validate(&state.storage, settings, &origin, raw, expected).await?;
        issued_cookie(&state.storage, &headers, result)
    }
    .await;
    let mut response = match result {
        Ok(cookie) => {
            state.succeed(&ip);
            let mut response = redirect(&format!("{origin}/dashboard"));
            clear(&mut response);
            response.headers_mut().append(header::SET_COOKIE, cookie);
            return response;
        }
        Err(reason) => {
            state.fail(ip);
            saml_error(&origin, &reason)
        }
    };
    clear(&mut response);
    response
}

struct Hint {
    issuer: String,
    request_id: Option<String>,
}

fn parse_hint(encoded: &str, destination: &str) -> Result<Hint, String> {
    let bytes = STANDARD
        .decode(encoded.as_bytes())
        .map_err(|_| "Invalid SAML response".to_owned())?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "Invalid SAML response".to_owned())?;
    let doc = Document::parse(text).map_err(|_| "Invalid SAML response".to_owned())?;
    let root = doc.root_element();
    if !root.has_tag_name((PROTOCOL, "Response")) {
        return Err("Invalid SAML response".into());
    }
    if root
        .attribute("Destination")
        .is_some_and(|value| value != destination)
    {
        return Err("SAML destination does not match".into());
    }
    let assertion = root
        .children()
        .find(|node| node.has_tag_name((ASSERTION, "Assertion")))
        .ok_or_else(|| "No SAML assertion".to_owned())?;
    let issuer = assertion
        .children()
        .find(|node| node.has_tag_name((ASSERTION, "Issuer")))
        .and_then(|node| node.text())
        .filter(|issuer| !issuer.is_empty())
        .ok_or_else(|| "No SAML issuer".to_owned())?;
    Ok(Hint {
        issuer: issuer.to_owned(),
        request_id: root
            .attribute("InResponseTo")
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
    })
}

async fn validate(
    storage: &Storage,
    settings: Value,
    origin: &str,
    encoded: String,
    expected: Option<String>,
) -> Result<Value, String> {
    let destination = format!("{origin}{ACS}");
    let hint = parse_hint(&encoded, &destination)?;
    if let Some(id) = expected.as_ref()
        && hint
            .request_id
            .as_ref()
            .is_none_or(|other| !bool::from(id.as_bytes().ct_eq(other.as_bytes())))
    {
        return Err("SAML request state does not match".into());
    }
    let claims = tokio::task::spawn_blocking(move || {
        verified_claims(
            settings,
            hint.issuer,
            destination,
            encoded,
            hint.request_id.clone(),
        )
    })
    .await
    .map_err(|_| "Could not verify SAML response".to_owned())??;
    let Claims {
        assertion,
        expiry,
        request_id,
        payload,
    } = claims;
    let require_pending = expected.is_none();
    storage.run(true, move |db| {
        let transaction = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = Utc::now().timestamp();
        transaction.execute("DELETE FROM kv WHERE scope IN ('saml-pending', 'saml-consumed') AND CAST(value AS INTEGER) < ?1", [now])?;
        if let Some(id) = request_id {
            let count = transaction.execute("DELETE FROM kv WHERE scope='saml-pending' AND key=?1", [id])?;
            if count == 0 && require_pending { return Ok(false) }
        }
        let added = transaction.execute(
            "INSERT INTO kv(scope,key,value) VALUES ('saml-consumed',?1,?2) ON CONFLICT(scope,key) DO NOTHING",
            (&assertion, expiry.to_string()),
        )?;
        if added == 0 { return Ok(false) }
        transaction.commit()?;
        Ok(true)
    }).await.map_err(|_| "Could not record SAML sign in".to_owned())?
        .then_some(payload).ok_or_else(|| "SAML response was already used or expired".to_owned())
}

struct Claims {
    assertion: String,
    expiry: i64,
    request_id: Option<String>,
    payload: Value,
}

fn verified_claims(
    settings: Value,
    issuer: String,
    destination: String,
    encoded: String,
    request_id: Option<String>,
) -> Result<Claims, String> {
    let mut policy = EntitySetting::default();
    policy.clock_drifts = (-60_000, 60_000);
    policy.want_message_signed = true;
    let sp = ServiceProvider::from_config(
        &SpMetadataConfig {
            entity_id: entity(&settings),
            want_assertions_signed: true,
            assertion_consumer_service: vec![Endpoint::new(Binding::Post, destination)],
            ..Default::default()
        },
        policy,
    )
    .map_err(|error| error.to_string())?;
    let idp = IdentityProvider::from_config(
        &IdpMetadataConfig {
            entity_id: issuer,
            signing_certs: vec![cert(&text(&settings, "samlCert"))],
            single_sign_on_service: vec![Endpoint::new(
                Binding::Redirect,
                text(&settings, "samlEntryPoint"),
            )],
            ..Default::default()
        },
        EntitySetting::default(),
    )
    .map_err(|error| error.to_string())?;
    let request = HttpRequest::post(vec![("SAMLResponse".into(), encoded)]);
    let profile = if let Some(id) = request_id.as_ref() {
        sp.parse_login_response_with_request_id(&idp, Binding::Post, &request, id)
    } else {
        sp.parse_unsolicited_login_response(&idp, Binding::Post, &request)
    }
    .map_err(|error| error.to_string())?
    .extract;
    let assertion = profile
        .get_str("assertion.id")
        .ok_or_else(|| "Missing signed SAML assertion ID".to_owned())?
        .to_owned();
    let claim = |name: &str| {
        let value = profile
            .get_key("attributes")
            .and_then(|attrs| attrs.get_key(name))
            .or_else(|| profile.get_key(name))?;
        value
            .as_str()
            .or_else(|| value.as_array()?.first()?.as_str())
            .filter(|claim| !claim.is_empty())
    };
    let email_field = text(&settings, "samlAttributeEmail");
    let email = [
        email_field.as_str(),
        "email",
        "emailAddress",
        "mail",
        "nameID",
        "nameId",
        "upn",
        "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress",
        "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/nameidentifier",
        "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/upn",
    ]
    .into_iter()
    .find_map(claim);
    let name_field = text(&settings, "samlAttributeName");
    let name = [
        name_field.as_str(),
        "displayName",
        "name",
        "cn",
        "commonName",
        "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/name",
        "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/givenname",
    ]
    .into_iter()
    .find_map(claim)
    .map(str::to_owned)
    .or_else(|| {
        let name = format!(
            "{} {}",
            claim("givenName").unwrap_or(""),
            claim("sn").or_else(|| claim("surname")).unwrap_or("")
        );
        let name = js_trim(&name);
        (!name.is_empty()).then(|| name.to_owned())
    })
    .or_else(|| email.map(str::to_owned))
    .unwrap_or_else(|| "SAML user".into());
    let expiry = profile
        .get_str("conditions.notOnOrAfter")
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map_or_else(
            || Utc::now().timestamp() + AGE,
            |value| value.timestamp() + 60,
        );
    Ok(Claims {
        request_id,
        assertion,
        expiry,
        payload: json!({"saml":true, "samlEmail":email, "samlName":name}),
    })
}
