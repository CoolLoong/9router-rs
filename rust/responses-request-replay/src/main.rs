#[path = "../../app/src/chat_format.rs"]
mod chat_format;
#[allow(dead_code)]
#[path = "../../app/src/chat_input.rs"]
mod chat_input;
#[path = "../../app/src/chat_protocol_input.rs"]
mod chat_protocol_input;
#[path = "../../app/src/responses_request.rs"]
mod responses_request;

use chat_format::ChatFormat;
use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let value: Value = serde_json::from_str(&std::env::args().nth(1).ok_or("missing body")?)?;
    let format = ChatFormat::from_path(value["path"].as_str().ok_or("path")?);
    let model = value["model"].as_str().ok_or("model")?;
    let accept = value["accept"].as_str().unwrap_or("");
    let shape = responses_request::prepare(format, value["body"].clone(), model, accept)
        .ok_or("unexpected preparation fallback")?;
    println!("{}", json!({"stream":shape.stream,"body":shape.body}));
    Ok(())
}
