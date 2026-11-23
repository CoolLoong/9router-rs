use axum::extract::Request;
use axum::http::Method;
use axum::response::Response;

use crate::observed_response::into_http;

pub async fn handle(req: Request) -> Response {
    let result = if req.method() == Method::OPTIONS {
        router_health_rs::port::options().await
    } else {
        router_health_rs::port::get().await
    };
    into_http(result)
}
