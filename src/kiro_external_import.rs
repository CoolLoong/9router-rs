use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde_json::{Map, Value, json};

use super::{body, error, reply};
use crate::provider_connections::{object, truthy};
use crate::provider_index::js_trim;
use crate::provider_oauth::{kiro_token_endpoint, save_imported, token_claims};
use crate::storage::Storage;
use crate::thinking_intent::numeric;

pub(super) async fn import(State(storage): State<Storage>, request: Request) -> Response {
    let input = match body(&storage, request).await {
        Ok(input) => input,
        Err((status, message)) => return error(status, message),
    };
    let wrapped = ["cliProxyAuth", "auth", "json"]
        .iter()
        .find_map(|field| input.get(*field).filter(|value| !value.is_null()))
        .unwrap_or(&input);
    let record = match fields(wrapped) {
        Ok(record) => record,
        Err(reason) => return error(StatusCode::BAD_REQUEST, reason),
    };
    match save_imported(&storage, "kiro", record).await {
        Ok(saved) => reply(
            StatusCode::OK,
            json!({
                "success":true,"connection":{"id":saved["id"],"provider":"kiro","email":saved["email"]},
            }),
            true,
        ),
        Err(_) => error(StatusCode::BAD_REQUEST, "CLIProxyAPI import failed"),
    }
}

fn fields(raw: &Value) -> Result<Map<String, Value>, &'static str> {
    let decoded;
    let input = if let Some(text) = raw.as_str() {
        decoded =
            serde_json::from_str::<Value>(text).map_err(|_| "CLIProxyAPI auth JSON is invalid")?;
        &decoded
    } else {
        raw
    };
    if !input.is_object() && !input.is_array() {
        return Err("CLIProxyAPI auth JSON is required");
    }
    let pick = |names: &[&str]| {
        names
            .iter()
            .find_map(|field| input.get(*field).filter(|value| truthy(value)))
            .unwrap_or(&Value::Null)
    };
    let method = trimmed(pick(&["auth_method", "authMethod"]));
    if !method.is_empty() && method != "external_idp" {
        return Err("Only external_idp Kiro auth is supported by this importer");
    }
    let endpoint = kiro_token_endpoint(trimmed(pick(&["token_endpoint", "tokenEndpoint"])))?;
    let access = require(
        trimmed(pick(&["access_token", "accessToken"])),
        "access_token is required",
    )?;
    let refresh = require(
        trimmed(pick(&["refresh_token", "refreshToken"])),
        "refresh_token is required",
    )?;
    let id = require(
        trimmed(pick(&["client_id", "clientId"])),
        "client_id is required",
    )?;
    let scope = pick(&["scopes", "scope"]);
    let scope = if let Some(entries) = scope.as_array() {
        entries
            .iter()
            .map(trimmed)
            .filter(|scope| !scope.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        trimmed(scope).to_owned()
    };
    require(&scope, "scopes is required")?;
    let profile = require(
        trimmed(pick(&["profile_arn", "profileArn"])),
        "profile_arn is required",
    )?;
    let region = trimmed(&input["region"]);
    let region = if region.is_empty() {
        "us-east-1"
    } else {
        region
    };
    let claims = token_claims(access);
    let email = input
        .get("email")
        .filter(|value| truthy(value))
        .or_else(|| {
            ["email", "preferred_username", "upn", "sub"]
                .iter()
                .find_map(|field| claims.get(*field).filter(|value| truthy(value)))
        });
    let explicit = pick(&["expired", "expires_at", "expiresAt"]);
    let duration = pick(&["expires_in", "expiresIn"]);
    let expiry = expires(explicit, duration, &claims["exp"])?;
    Ok(object(json!({
        "accessToken":access,"refreshToken":refresh,"expiresAt":expiry,"email":email,
        "providerSpecificData":{
            "profileArn":profile,"region":region,"authMethod":"external_idp",
            "provider":"CLIProxyAPI","clientId":id,"tokenEndpoint":endpoint.as_str(),"scope":scope,
        },
    })))
}

fn trimmed(value: &Value) -> &str {
    value.as_str().map(js_trim).unwrap_or_default()
}

fn require<'a>(value: &'a str, reason: &'static str) -> Result<&'a str, &'static str> {
    (!value.is_empty()).then_some(value).ok_or(reason)
}

fn expires(explicit: &Value, duration: &Value, claim: &Value) -> Result<String, &'static str> {
    let now = Utc::now().timestamp_millis() as f64;
    let date = explicit
        .as_str()
        .and_then(|value| {
            DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|date| date.to_utc())
                .or_else(|| {
                    NaiveDate::parse_from_str(value, "%Y-%m-%d")
                        .ok()?
                        .and_hms_opt(0, 0, 0)
                        .map(|date| date.and_utc())
                })
        })
        .or_else(|| explicit.as_i64().and_then(DateTime::from_timestamp_millis));
    let fallback = || {
        let milliseconds = if let Some(seconds) = numeric(duration).filter(|seconds| *seconds > 0.0)
        {
            now + seconds * 1000.0
        } else if truthy(claim) {
            numeric(claim)? * 1000.0
        } else {
            now + 3_600_000.0
        };
        DateTime::from_timestamp_millis(milliseconds.trunc() as i64)
    };
    date.or_else(fallback)
        .map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true))
        .ok_or("Invalid time value")
}
