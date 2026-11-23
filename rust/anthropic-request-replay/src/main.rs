#[path = "../../app/src/claude_thinking.rs"]
mod claude_thinking;
#[path = "../../app/src/thinking_intent.rs"]
mod thinking_intent;
#[path = "../../app/src/thinking_catalog.rs"]
mod thinking_catalog;

mod openai_models {
    use std::sync::OnceLock;
    pub fn data() -> &'static serde_json::Value {
        static DATA: OnceLock<serde_json::Value> = OnceLock::new();
        DATA.get_or_init(|| serde_json::from_str(include_str!("../../app/src/openai-catalog-data.json"))
            .expect("embedded independent comparison data"))
    }
}

#[path = "../../app/src/anthropic_request.rs"]
mod anthropic_request;
#[path = "../../app/src/chat_format.rs"]
mod chat_format;
#[allow(dead_code)]
#[path = "../../app/src/chat_input.rs"]
mod chat_input;
#[path = "../../app/src/chat_protocol_input.rs"]
mod chat_protocol_input;

use chat_format::ChatFormat;
use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let value: Value = serde_json::from_str(&std::env::args().nth(1).ok_or("missing body")?)?;
    let client = ChatFormat::from_path(value["path"].as_str().ok_or("path")?);
    let model = value["model"].as_str().ok_or("model")?;
    let accept = value["accept"].as_str().unwrap_or("");
    let shape = anthropic_request::prepare(client, value["body"].clone(), model, accept)
        .ok_or("unexpected preparation fallback")?;
    println!("{}", json!({"stream":shape.stream,"body":shape.body}));
    Ok(())
}
