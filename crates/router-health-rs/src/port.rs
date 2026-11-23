//! The Rust port of `src/app/api/health/`.
//!
//! Replace each `unimplemented!` with the port of the source function. Return `Err` where
//! the source throws or raises: the caller gets an error there too. After changing a
//! signature, run `rust-anything refresh` to regenerate the bindings. `rust-anything diff` checks
//! behavior against what the source recorded.

#![forbid(unsafe_code)]

/// Port of `GET` (`src/app/api/health/route.js:9`).
pub async fn get() -> Result<serde_json::Value, String> {
    Ok(response(
        200,
        serde_json::json!({ "ok": true }).to_string(),
        true,
    ))
}

/// Port of `OPTIONS` (`src/app/api/health/route.js:13`).
pub async fn options() -> Result<serde_json::Value, String> {
    Ok(response(204, String::new(), false))
}

fn response(status: u16, body: String, json: bool) -> serde_json::Value {
    let mut headers = serde_json::json!({
        "access-control-allow-headers": "*",
        "access-control-allow-methods": "GET, OPTIONS",
        "access-control-allow-origin": "*",
    });
    if let Some(headers) = headers.as_object_mut()
        && json
    {
        headers.insert("content-type".into(), "application/json".into());
    }
    serde_json::json!({ "status": status, "headers": headers, "body": body })
}
