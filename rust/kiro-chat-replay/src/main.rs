use std::io::{self, Read};
use std::time::Duration;

use axum::http::HeaderMap;
use serde_json::{Value, json};

#[allow(dead_code)]
#[path = "../../app/src/chat_format.rs"]
mod chat_format;
#[allow(dead_code)]
#[path = "../../app/src/kiro_chat.rs"]
mod kiro_chat;
#[allow(dead_code)]
#[path = "../../app/src/kiro_models.rs"]
mod kiro_models;
#[allow(dead_code)]
#[path = "../../app/src/provider_index.rs"]
mod provider_index;
#[allow(dead_code)]
#[path = "../../app/src/thinking_intent.rs"]
mod thinking_intent;

mod provider_connections {
    use serde_json::Value;

    pub fn truthy(value: &Value) -> bool {
        match value {
            Value::Null | Value::Bool(false) => false,
            Value::String(text) => !text.is_empty(),
            Value::Number(number) => number.as_f64() != Some(0.0),
            _ => true,
        }
    }
}

mod chat_input {
    pub use crate::provider_connections::truthy;
    pub fn js_space(input: char) -> bool {
        let mut text = [0; 4];
        crate::provider_index::js_trim(input.encode_utf8(&mut text)).is_empty()
    }
}

mod bedrock_transport {
    #[allow(dead_code)]
    pub mod eventstream {
        include!("../../app/src/bedrock_eventstream.rs");
    }

    fn secret_values(_: &serde_json::Value) -> impl Iterator<Item = &str> {
        std::iter::empty()
    }

    fn redact_values<'a>(text: &str, _: impl Iterator<Item = &'a str>) -> String {
        text.to_owned()
    }
}

#[tokio::main]
async fn main() {
    let endpoint = std::env::args()
        .nth(1)
        .expect("local upstream fixture endpoint");
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .expect("fixture input");
    let input = serde_json::from_str::<Value>(&input).expect("fixture JSON");
    let output = if let Some(requests) = input["requests"].as_array() {
        let mut results = Vec::new();
        for request in requests {
            results.push(exchange(&endpoint, request).await);
        }
        json!({"responses":results})
    } else {
        exchange(&endpoint, &input).await
    };
    println!("{output}");
}

async fn exchange(endpoint: &str, input: &Value) -> Value {
    let model = input["model"].as_str().unwrap_or_default();
    let account = &input["account"];
    let mut headers = HeaderMap::new();
    if let Some(fields) = input["headers"].as_object() {
        for (name, value) in fields {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("fixture header"),
                axum::http::HeaderValue::from_str(value.as_str().unwrap_or_default())
                    .expect("fixture header value"),
            );
        }
    }
    let (Some(body), Some(selected)) = (
        kiro_chat::supports(account)
            .then(|| {
                kiro_chat::prepare(
                    &input["body"],
                    model,
                    &headers,
                    input["sessionOwner"].as_str(),
                )
            })
            .flatten(),
        kiro_chat::Connection::from_account(account),
    ) else {
        return json!({"native":false});
    };
    let primary = selected.url();
    let region = url_region(&primary, "q").expect("regional AWS endpoint");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("local fixture client");
    let request = |endpoint: &str, codewhisperer| {
        selected.headers(
            client
                .post(endpoint)
                .json(&selected.body(&body, account["id"].as_str())),
            codewhisperer,
        )
    };
    let mut response = request(
        &format!("{endpoint}/{region}/generateAssistantResponse"),
        false,
    )
    .send()
    .await
    .expect("fixture response");
    if let Some(alternate) = selected.alternate(response.status()) {
        let region = url_region(&alternate, "codewhisperer").expect("regional fallback endpoint");
        response = request(
            &format!("{endpoint}/{region}/codewhisperer/generateAssistantResponse"),
            true,
        )
        .send()
        .await
        .expect("fixture fallback response");
    }
    let response = kiro_chat::adapt(response, model, &body, Duration::from_secs(3));
    let status = response.status().as_u16();
    let wire = response.text().await.expect("fixture response text");
    json!({"native":true,"status":status,"wire":kiro_chat::redact_error(account,&wire)})
}

fn url_region<'a>(url: &'a str, service: &str) -> Option<&'a str> {
    Some(
        url.strip_prefix("https://")?
            .strip_prefix(service)?
            .strip_prefix('.')?
            .split_once(".amazonaws.com/")?
            .0,
    )
}
