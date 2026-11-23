#[allow(dead_code)]
#[path = "../../app/src/chat_input.rs"]
mod chat_input;
#[allow(dead_code)]
#[path = "../../app/src/github_executor.rs"]
mod github_executor;

#[allow(dead_code)]
#[path = "../../app/src/chat_format.rs"]
mod chat_format;
#[allow(dead_code)]
#[path = "../../app/src/chat_protocol_input.rs"]
mod chat_protocol_input;
#[allow(dead_code)]
#[path = "../../app/src/github_responses.rs"]
mod github_responses;
#[allow(dead_code)]
#[path = "../../app/src/responses_request.rs"]
mod responses_request;
#[allow(dead_code)]
#[path = "../../app/src/sse_reader.rs"]
mod sse_reader;

use serde_json::{Value, json};
#[allow(dead_code)]
#[path = "../../app/src/anthropic_request.rs"]
mod anthropic_request;
#[allow(dead_code)]
#[path = "../../app/src/anthropic_upstream.rs"]
mod anthropic_upstream;
#[path = "../../app/src/claude_thinking.rs"]
mod claude_thinking;
#[allow(dead_code)]
#[path = "../../app/src/github_claude.rs"]
mod github_claude;
#[path = "../../app/src/thinking_catalog.rs"]
mod thinking_catalog;
#[path = "../../app/src/thinking_intent.rs"]
mod thinking_intent;

mod openai_models {
    use std::sync::OnceLock;
    pub fn data() -> &'static serde_json::Value {
        static DATA: OnceLock<serde_json::Value> = OnceLock::new();
        DATA.get_or_init(|| {
            serde_json::from_str(include_str!("../../app/src/openai-catalog-data.json"))
                .expect("embedded fixture catalog")
        })
    }
}

use std::io::{self, Read};

#[tokio::main]
async fn main() {
    let mut fixture = String::new();
    io::stdin()
        .read_to_string(&mut fixture)
        .expect("fixture input");
    let fixture: Value = serde_json::from_str(&fixture).expect("fixture JSON");
    let mode = fixture["action"].as_str().unwrap_or("chat");
    let model = fixture["model"].as_str().expect("fixture model");
    let endpoint = fixture["endpoint"].as_str().expect("fixture endpoint");
    let stream = fixture["stream"] == true;
    match mode {
        "monthly" => {
            let status = fixture["status"].as_u64().unwrap() as u16;
            let message = fixture["error"].as_str().unwrap();
            let now = fixture["now"].as_i64().unwrap();
            println!(
                "{}",
                json!({"reset":github_executor::monthly_reset(status,message,now)})
            );
        }
        "classification" => {
            println!(
                "{}",
                json!({
                    "supported": github_executor::supported(model),
                    "needsResponses": github_executor::needs_responses(
                        model, fixture["status"].as_u64().unwrap_or(400) as u16,
                        fixture["error"].as_str().unwrap_or_default(),
                    )
                })
            );
        }
        "claude" => {
            let result = claude(&fixture, model).await;
            println!("{}", json!({"responses":[result]}));
        }
        "claude-eligibility" => {
            println!(
                "{}",
                json!({"supported":github_claude::supported_body(&fixture["body"])})
            );
        }
        "alternate" => {
            let client = reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("local client");
            let routes = github_responses::Routes::default();
            let mut results = Vec::new();
            for _ in 0..fixture["turns"].as_u64().unwrap_or(1) {
                let result = alternate(&client, &routes, &fixture, model).await;
                results.push(result);
            }
            println!("{}", json!({"responses":results}));
        }
        _ => {
            let mut body = fixture["body"].clone();
            github_executor::prepare(&mut body, model).expect("native request");
            let client = reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("fixture client");
            let request = github_executor::headers(
                client.post(endpoint).json(&body),
                &fixture["account"],
                &fixture["settings"],
                stream,
            );
            let response = request.send().await.expect("localhost fixture");
            let status = response.status().as_u16();
            let payload = response.text().await.expect("fixture response");
            println!(
                "{}",
                json!({"status":status,"body":payload,"url":github_executor::url()})
            );
        }
    }
}

async fn alternate(
    client: &reqwest::Client,
    routes: &github_responses::Routes,
    fixture: &Value,
    model: &str,
) -> Value {
    let stream = fixture["stream"] == true;
    let mut response = None;
    if !routes.has(model) {
        let mut chat_body = fixture["body"].clone();
        github_executor::prepare(&mut chat_body, model).expect("Chat fixture");
        let chat = github_executor::headers(
            client
                .post(fixture["endpoint"].as_str().unwrap())
                .json(&chat_body),
            &fixture["account"],
            &fixture["settings"],
            stream,
        )
        .send()
        .await
        .expect("local Chat fixture");
        let status = chat.status().as_u16();
        let body = chat.text().await.expect("Chat response");
        if !github_executor::needs_responses(model, status, &body) {
            response = Some(json!({"status":status,"body":body,"url":github_executor::url()}));
        } else {
            routes.remember(model);
        }
    }
    if let Some(response) = response {
        return response;
    }
    let body =
        github_responses::prepare(fixture["body"].clone(), model).expect("native alternate body");
    let response = github_responses::headers(
        client
            .post(fixture["responsesEndpoint"].as_str().unwrap())
            .json(&body),
        &fixture["account"],
        stream,
    )
    .send()
    .await
    .expect("local Responses fixture");
    let response =
        github_responses::adapt(response, model, stream, std::time::Duration::from_secs(5));
    json!({"status":response.status().as_u16(), "body":response.text().await.expect("converted stream"), "url":github_responses::url()})
}

async fn claude(fixture: &Value, model: &str) -> Value {
    let stream = fixture["stream"] == true;
    let body = github_claude::prepare(fixture["body"].clone(), model).expect("native Claude body");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("local Claude client");
    let response = github_claude::headers(
        client
            .post(fixture["messagesEndpoint"].as_str().unwrap())
            .json(&body),
        &fixture["account"],
        stream,
    )
    .send()
    .await
    .expect("local Claude shim fixture");
    let response = github_claude::adapt(response, model, stream, std::time::Duration::from_secs(5));
    json!({"status":response.status().as_u16(),"body":response.text().await.expect("converted stream"),"url":github_claude::url()})
}
