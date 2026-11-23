use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};

use super::{ConnectionTest, ProviderTools, error, reply, test_connection};
use crate::provider_connections::{self as accounts, truthy};
use crate::provider_index::{self, Compatible, compatible};

const BATCH_LIMIT: usize = 1024 * 1024;
const INVALID_MODE: &str = "Invalid mode. Use: provider, oauth, free, apikey, compatible, all";

pub(super) async fn run(State(state): State<ProviderTools>, request: Request) -> Response {
    let request = to_bytes(request.into_body(), BATCH_LIMIT)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .filter(|value| !value.is_null());
    let Some(request) = request else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Batch test failed");
    };
    let mode = &request["mode"];
    if !truthy(mode) {
        return error(StatusCode::BAD_REQUEST, "mode is required");
    }
    let provider_id = &request["providerId"];
    let Some(mode) = mode.as_str().filter(|mode| match *mode {
        "provider" => truthy(provider_id),
        "oauth" | "free" | "apikey" | "compatible" | "all" => true,
        _ => false,
    }) else {
        return error(StatusCode::BAD_REQUEST, INVALID_MODE);
    };

    let Ok(connections) = state
        .storage
        .run(false, |db| accounts::read_all(db, None))
        .await
    else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Batch test failed");
    };
    let mut results = Vec::new();
    let mut passed = 0;
    for connection in connections.iter().filter(|connection| {
        connection["isActive"] == true && selected(mode, provider_id, connection)
    }) {
        let Some(id) = connection["id"].as_str() else {
            continue;
        };
        let observation = match test_connection(&state, id.to_owned()).await {
            ConnectionTest::Complete(test) => {
                row(test.valid, json!(test.error), test.latency_ms, Value::Null)
            }
            ConnectionTest::Missing => row(false, json!("Connection not found"), 0, Value::Null),
            ConnectionTest::Failed => failure(json!("Test failed")),
        };
        let connection_name = ["name", "email", "provider"]
            .iter()
            .map(|field| &connection[*field])
            .find(|value| truthy(value));
        let mut result = json!({
            "provider": connection["provider"],
            "connectionId": id,
            "connectionName": connection_name,
            "authType": connection["authType"].as_str().filter(|value| !value.is_empty())
                .unwrap_or_else(|| auth_group(connection)),
        });
        if let Some(fields) = result.as_object_mut() {
            fields.extend(accounts::object(observation));
        }
        passed += usize::from(result["valid"] == true);
        results.push(result);
    }
    let total = results.len();
    reply(
        StatusCode::OK,
        json!({
            "mode": mode, "providerId": truthy(provider_id).then_some(provider_id),
            "results": results, "testedAt": accounts::now(),
            "summary": {"total":total,"passed":passed,"failed":total-passed},
        }),
    )
}

fn selected(mode: &str, provider_id: &Value, connection: &Value) -> bool {
    match mode {
        "all" => true,
        "provider" => connection["provider"] == *provider_id,
        "compatible" => is_compatible(connection["provider"].as_str().unwrap_or_default()),
        kind => auth_group(connection) == kind,
    }
}

fn is_compatible(provider: &str) -> bool {
    matches!(
        compatible(provider),
        Some(Compatible::OpenAi | Compatible::Anthropic)
    )
}

fn auth_group(connection: &Value) -> &str {
    let provider = connection["provider"].as_str().unwrap_or_default();
    let group = provider_index::find(provider).and_then(|entry| entry["authGroup"].as_str());
    match connection["authType"]
        .as_str()
        .filter(|kind| !kind.is_empty())
    {
        Some("oauth") if group == Some("free") => "free",
        Some(kind) => kind,
        None => group.unwrap_or_else(|| {
            if is_compatible(provider) {
                "compatible"
            } else {
                "apikey"
            }
        }),
    }
}

fn failure(error: Value) -> Value {
    row(
        false,
        error.clone(),
        0,
        json!({
            "type":"network_error","source":"local","code":null,"message":error,
        }),
    )
}

fn row(valid: bool, error: Value, latency_ms: u128, diagnosis: Value) -> Value {
    json!({
        "valid":valid,"latencyMs":latency_ms,"error":truthy(&error).then_some(error),
        "diagnosis":diagnosis,"statusCode":null,"testedAt":accounts::now(),
    })
}
