#[allow(dead_code)]
#[path = "../../app/src/chat_input.rs"]
mod chat_input;
#[allow(dead_code)]
#[path = "../../app/src/github_executor.rs"]
mod github_executor;
#[path = "../../app/src/ollama_local.rs"]
mod ollama_local;
#[allow(dead_code)]
#[path = "../../app/src/sse_reader.rs"]
mod sse_reader;

use serde_json::{Value, json};
use std::io::{self, Read};

#[tokio::main]
async fn main() {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .expect("fixture input");
    let item: Value = serde_json::from_str(&input).expect("fixture JSON");
    let account = &item["account"];
    let url = ollama_local::url(account).expect("fixture host");
    if item["mode"] == "url" {
        println!("{}", json!({"url":url}));
        return;
    }
    let streaming = item["stream"] == true;
    let model = item["model"].as_str().expect("fixture model");
    let endpoint = item["endpoint"].as_str().expect("local fixture");
    let body =
        ollama_local::prepare(item["body"].clone(), model, streaming).expect("prepared body");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("fixture HTTP");
    let response = ollama_local::headers(
        client.post(endpoint).json(&body),
        account,
        &item["settings"],
        streaming,
    )
    .send()
    .await
    .expect("local Ollama fixture");
    let response = ollama_local::adapt(
        response,
        model,
        streaming,
        std::time::Duration::from_secs(5),
    )
    .await;
    let status = response.status().as_u16();
    let body = response.text().await.expect("converted body");
    println!("{}", json!({"url":url,"status":status,"body":body}));
}
