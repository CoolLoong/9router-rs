use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};

use crate::chat_format::ChatFormat;
use crate::{gemini_request, openai_thinking, vertex_transport};

use super::{bad, failure, intermediate, success};

pub(super) fn preview(body: &Value, provider: &str, model: &str, account: &Value) -> Response {
    let stream = body["stream"] != false;
    let (url, authorization) =
        match vertex_transport::preview_target(provider, model, stream, account) {
            Ok(target) => target,
            Err(message) => return failure(StatusCode::INTERNAL_SERVER_ERROR, message),
        };
    let translated = match provider {
        "vertex" => gemini(body.clone(), model),
        _ => partner(body.clone(), model),
    };
    let Some(body) = translated else {
        return bad("Unsupported Vertex preview request body");
    };
    let mut headers = json!({"Content-Type":"application/json"});
    if authorization {
        headers["Authorization"] = json!("[redacted]");
    }
    if stream {
        headers["Accept"] = json!("text/event-stream");
    }
    success(json!({"url":url.as_str(),"headers":headers,"body":body}))
}

fn gemini(mut body: Value, model: &str) -> Option<Value> {
    if body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .is_none_or(|message| !matches!(message["role"].as_str(), Some("user" | "tool")))
    {
        body.as_object_mut()?.shift_remove("thinking");
    }
    let mut prepared = gemini_request::prepare(
        ChatFormat::Chat,
        body,
        model,
        "",
        |_| None,
        gemini_request::DEFAULT_SIGNATURE,
    )?;
    vertex_transport::prepare_body(&mut prepared.body);
    Some(prepared.body)
}

fn partner(mut body: Value, model: &str) -> Option<Value> {
    let (_, controls) = openai_thinking::capture(&mut body, "vertex-partner", model, None, false);
    let mut result = intermediate(&body, "openai", model, "vertex-partner")?;
    if let Some(controls) = controls {
        controls.write(&mut result);
    }
    Some(result)
}
