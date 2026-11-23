use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{body, error, metadata_bytes, metadata_client, nonempty, reply};
use crate::chat_output::js_text;
use crate::provider_connections::{object, truthy};
use crate::provider_index::js_trim;
use crate::provider_oauth::save_imported;
use crate::proxy_transport::{self, Route};
use crate::storage::Storage;

const URL: &str = "https://cloud.zed.dev/client/users/me";

pub(super) async fn import(State(storage): State<Storage>, request: Request) -> Response {
    let input = match body(&storage, request).await {
        Ok(input) => input,
        Err((status, message)) => return error(status, message),
    };
    let Some(token) = input["accessToken"]
        .as_str()
        .map(js_trim)
        .filter(|token| !token.is_empty())
    else {
        return error(StatusCode::BAD_REQUEST, "Access token is required");
    };
    let user_id = input
        .get("userId")
        .filter(|id| !id.is_null())
        .map(js_text)
        .unwrap_or_default();
    let user_id = js_trim(&user_id);
    if user_id.is_empty() {
        return error(StatusCode::BAD_REQUEST, "User id is required");
    }
    let system_id = input["systemId"]
        .as_str()
        .map(js_trim)
        .filter(|id| !id.is_empty())
        .map_or_else(|| Uuid::new_v4().to_string(), str::to_owned);
    let user = match user_info(&storage, user_id, token, &system_id).await {
        Ok(user) => user,
        Err(reason) => return error(StatusCode::UNAUTHORIZED, reason),
    };
    let email = nonempty(&user["email"]);
    let display = ["name", "display_name", "username"]
        .iter()
        .find_map(|field| nonempty(&user[*field]))
        .map_or_else(|| format!("Zed {user_id}"), str::to_owned);
    let update = object(json!({
        "accessToken":token,"refreshToken":null,"expiresAt":null,
        "email":email,"displayName":display,"providerSpecificData":{
            "authMethod":"imported","userId":user_id,"systemId":system_id,"organizationId":organization(&user),
        },
    }));
    match save_imported(&storage, "zed", update).await {
        Ok(record) => reply(
            StatusCode::OK,
            json!({
                "success":true,"connection":{"id":record["id"],"provider":"zed","email":record["email"],
                    "displayName":record["displayName"]},
            }),
            true,
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save the provider connection",
        ),
    }
}

pub(crate) async fn user_info(
    storage: &Storage,
    user: &str,
    token: &str,
    system: &str,
) -> Result<Value, String> {
    let failed = || "Zed token validation failed".to_owned();
    let settings = storage.read_settings().await.ok_or_else(failed)?;
    let route = proxy_transport::select(storage, &settings, &Value::Null, URL)
        .await
        .map_err(|_| failed())?;
    let client = metadata_client(&route, Duration::from_secs(10)).ok_or_else(failed)?;
    let url = match &route {
        Route::Relay { url, .. } => url,
        Route::Direct if std::env::var("NINEROUTER_MIGRATION_FIXTURE").as_deref() == Ok("1") => {
            "http://127.0.0.1:52989/zed-fixture/client/users/me"
        }
        _ => URL,
    };
    let mut authorization =
        HeaderValue::from_str(&format!("{user} {token}")).map_err(|_| failed())?;
    authorization.set_sensitive(true);
    let mut response = route
        .relay(
            client
                .get(url)
                .header(reqwest::header::ACCEPT, "application/json")
                .header(reqwest::header::AUTHORIZATION, authorization)
                .header("x-zed-system-id", system),
        )
        .send()
        .await
        .map_err(|_| failed())?;
    let status = response.status();
    let raw = metadata_bytes(&mut response).await.ok_or_else(failed)?;
    let data: Value = serde_json::from_slice(&raw).unwrap_or_default();
    if !status.is_success() {
        let text = String::from_utf8_lossy(&raw);
        let reason = [&data["message"], &data["error"]["message"], &data["error"]]
            .into_iter()
            .find_map(nonempty)
            .unwrap_or(&text);
        let reason = reason.replace(token, "[redacted]");
        let reason: String = reason
            .chars()
            .filter(|ch| !ch.is_control())
            .take(256)
            .collect();
        return Err(if reason.is_empty() {
            format!("HTTP {}", status.as_u16())
        } else {
            reason
        });
    }
    Ok(data)
}

pub(crate) fn organization(user: &Value) -> String {
    let direct = [
        &user["default_organization_id"],
        &user["defaultOrganizationId"],
    ]
    .into_iter()
    .find(|value| truthy(value));
    let organization = direct.or_else(|| {
        let groups = user["organizations"].as_array()?;
        let group = groups
            .iter()
            .find(|group| truthy(&group["is_personal"]))
            .or_else(|| groups.first())?;
        group.get("id")
    });
    let Some(value) = organization.filter(|value| truthy(value)) else {
        return String::new();
    };
    value
        .as_str()
        .or_else(|| value.get(0).and_then(Value::as_str))
        .or_else(|| value["id"].as_str())
        .map_or_else(|| js_text(value), str::to_owned)
}
