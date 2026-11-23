use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

pub fn into_http(result: Result<Value, String>) -> Response {
    match result.ok().and_then(build_response) {
        Some(response) => response,
        None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn build_response(response: Value) -> Option<Response> {
    let status = u16::try_from(response["status"].as_u64()?).ok()?;
    let headers = response["headers"].as_object()?;
    let body = response["body"].as_str()?.to_owned();
    let vary = "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch";
    let mut builder = Response::builder().status(status).header(
        header::VARY,
        if headers.contains_key("content-type") {
            format!("{vary}, Accept-Encoding")
        } else {
            vary.to_owned()
        },
    );
    for (name, value) in headers {
        builder = builder.header(name, value.as_str()?);
    }
    builder.body(Body::from(body)).ok()
}
