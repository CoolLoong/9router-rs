use std::time::Duration;

use axum::http::StatusCode;
use reqwest::{Method, Response, header};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

use crate::provider_connections::truthy;
use crate::storage::Storage;

use super::image_queue_egress::{Delivery, Endpoint};
use super::{ImageError, QueuedImageError, fixture, image_key, proxy_required};
use QueuedImageError::{BeforeSubmit, PossiblyAccepted, Protected};

const ENDPOINT: &str = "https://queue.fal.run";
const FIXTURE: &str = "http://127.0.0.1:52989/fal-image";
const RESPONSE_LIMIT: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);

pub(super) async fn generate(
    storage: &Storage,
    settings: &Value,
    model: &str,
    account: &Value,
    body: &Value,
) -> Result<Value, QueuedImageError> {
    let fail_before_submit = |failure| {
        if proxy_required(settings, account) {
            Protected(failure)
        } else {
            BeforeSubmit(failure)
        }
    };
    let mut segments = model.split('/');
    let owner = segments.next().filter(|segment| valid_segment(segment));
    let app = segments.next().filter(|segment| valid_segment(segment));
    let Some((owner, app)) = owner
        .zip(app)
        .filter(|_| model.len() <= 256 && segments.all(valid_segment))
    else {
        return Err(fail_before_submit(ImageError {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid Fal image model ID".into(),
        }));
    };
    let key = image_key(account).ok_or_else(|| {
        fail_before_submit(ImageError {
            status: StatusCode::UNAUTHORIZED,
            message: "Missing Fal API key".into(),
        })
    })?;
    let mut authorization = header::HeaderValue::from_str(&format!("Key {key}")).map_err(|_| {
        fail_before_submit(ImageError {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid Fal API key".into(),
        })
    })?;
    authorization.set_sensitive(true);
    let submit_path = format!("/{model}");
    let endpoint = Endpoint {
        name: "Fal",
        origin: ENDPOINT,
        direct: if fixture() { FIXTURE } else { ENDPOINT },
    };
    let delivery = Delivery::select(storage, settings, account, endpoint, &submit_path).await?;
    if delivery.protected && delivery.contains_secret(&submit_path, key) {
        return Err(Protected(bad("Invalid Fal image model ID")));
    }
    let response = delivery
        .request(Method::POST, &submit_path, &authorization)
        .json(&payload(body))
        .send()
        .await
        .map_err(|_| PossiblyAccepted(bad("Fal submission did not return a response")))?;
    let status = response.status();
    if delivery.protected && status.is_redirection() {
        return Err(Protected(bad("Fal submission redirect refused")));
    }
    let accepted = json_response(response)
        .await
        .map_err(|_| PossiblyAccepted(bad("Invalid Fal submission response")))?;
    if !status.is_success() {
        let message = ["message", "detail", "error"]
            .into_iter()
            .find_map(|name| accepted[name].as_str().filter(|text| !text.is_empty()))
            .unwrap_or_else(|| status.canonical_reason().unwrap_or("Fal submission failed"));
        let message = delivery.redact(message, key);
        let failure = ImageError {
            status,
            message: format!("[{}]: {message}", status.as_u16()),
        };
        return Err(if delivery.protected {
            Protected(failure)
        } else if status.is_client_error() && status != StatusCode::REQUEST_TIMEOUT {
            BeforeSubmit(failure)
        } else {
            PossiblyAccepted(failure)
        });
    }
    let Some(id) = accepted["request_id"].as_str().filter(|id| {
        id.len() <= 128
            && valid_segment(id)
            && (!delivery.protected || !delivery.contains_secret(id, key))
    }) else {
        return Err(PossiblyAccepted(bad(
            "Fal submission did not return a valid request ID",
        )));
    };
    let result_path = format!("/{owner}/{app}/requests/{id}");
    let status_path = format!("{result_path}/status");
    if !valid_link(&accepted["status_url"], &status_path)
        || !valid_link(&accepted["response_url"], &result_path)
    {
        return Err(PossiblyAccepted(bad(
            "Fal returned a queue URL outside the submitted request",
        )));
    }
    let id = id.to_owned();
    drop(accepted);

    let poll = async {
        let interval = if fixture() {
            Duration::from_millis(15)
        } else {
            Duration::from_millis(1500)
        };
        loop {
            sleep(interval).await;
            let response = delivery
                .request(Method::GET, &status_path, &authorization)
                .send()
                .await
                .map_err(|_| bad("Fal status request failed"))?;
            if !response.status().is_success() {
                return Err(bad(format!("Fal status {}", response.status().as_u16())));
            }
            let state = json_response(response)
                .await
                .map_err(|_| bad("Invalid Fal status response"))?;
            if let Some(returned) = state["request_id"].as_str()
                && returned != id
            {
                return Err(bad("Fal returned a status for a different request"));
            }
            if let Some(message) = state["error"]
                .as_str()
                .filter(|message| !message.is_empty())
            {
                return Err(bad(delivery.redact(message, key)));
            }
            match state["status"].as_str() {
                Some("COMPLETED") => break,
                Some("FAILED" | "CANCELLED") => return Err(bad("Fal generation failed")),
                Some("IN_QUEUE" | "IN_PROGRESS") => (),
                _ => return Err(bad("Invalid Fal request status")),
            }
        }
        let response = delivery
            .request(Method::GET, &result_path, &authorization)
            .send()
            .await
            .map_err(|_| bad("Fal result request failed"))?;
        if !response.status().is_success() {
            return Err(bad(format!(
                "Fal result status {}",
                response.status().as_u16()
            )));
        }
        let result = json_response(response)
            .await
            .map_err(|_| bad("Invalid Fal result response"))?;
        let rows = result["images"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_else(|| {
                if truthy(&result["image"]) {
                    std::slice::from_ref(&result["image"])
                } else {
                    &[]
                }
            });
        let data = rows
            .iter()
            .filter_map(|row| {
                let url = row["url"]
                    .as_str()
                    .or_else(|| row.as_str())
                    .filter(|url| !url.is_empty())?;
                Some(json!({"url":if delivery.contains_secret(url, key) { "[redacted]" } else { url }}))
            })
            .collect::<Vec<_>>();
        Ok(json!({"created":chrono::Utc::now().timestamp(),"data":data}))
    };
    timeout_at(Instant::now() + TIMEOUT, poll)
        .await
        .map_err(|_| PossiblyAccepted(bad("Fal polling timeout")))?
        .map_err(PossiblyAccepted)
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_link(value: &Value, path: &str) -> bool {
    value
        .as_str()
        .and_then(|link| Url::parse(link).ok())
        .is_some_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some("queue.fal.run")
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == path
        })
}

fn payload(body: &Value) -> Value {
    let count = body
        .get("n")
        .filter(|n| truthy(n))
        .cloned()
        .unwrap_or(json!(1));
    let mut request = json!({"prompt":body["prompt"],"num_images":count});
    if truthy(&body["size"]) {
        let dimensions = body["size"]
            .as_str()
            .and_then(|size| size.split_once('x'))
            .and_then(|(width, height)| width.parse::<u32>().ok().zip(height.parse::<u32>().ok()))
            .filter(|(width, height)| *width > 0 && *height > 0);
        request["image_size"] = dimensions.map_or(
            json!("square_hd"),
            |(width, height)| json!({"width":width,"height":height}),
        );
    }
    if truthy(&body["image"]) {
        request["image_url"] = body["image"].clone();
    }
    request
}

async fn json_response(mut response: Response) -> Result<Value, ()> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if chunk.len() > RESPONSE_LIMIT - bytes.len() {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn bad(message: impl Into<String>) -> ImageError {
    ImageError::bad_gateway(message)
}
