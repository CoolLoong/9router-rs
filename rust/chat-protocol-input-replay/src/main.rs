#[path = "../../app/src/chat_format.rs"]
mod chat_format;
#[path = "../../app/src/chat_input.rs"]
mod chat_input;
#[path = "../../app/src/chat_protocol_input.rs"]
mod chat_protocol_input;

use chat_format::ChatFormat;
use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let value: Value = serde_json::from_str(&std::env::args().nth(1).ok_or("missing body")?)?;
    let format = ChatFormat::from_path(value["path"].as_str().ok_or("path")?);
    let model = value["model"].as_str().ok_or("model")?;
    let accept = value["accept"].as_str().unwrap_or("");
    let converted = chat_protocol_input::translate(format, value["body"].clone(), accept)
        .ok_or("unexpected translation fallback")?;
    if chat_input::requires_original(
        &json!({}),
        &converted,
        "openai-compatible-chat-fixture",
        Some("chat"),
        "http://127.0.0.1:9/v1",
        "fixture",
        "off",
    ) {
        return Err("unexpected preparation fallback".into());
    }
    let shape = match format {
        ChatFormat::Chat => chat_input::prepare(converted, model, accept),
        _ => chat_input::prepare_translated(converted, model, accept),
    }
    .ok_or("unexpected preparation fallback")?;
    println!("{}", json!({"stream":shape.stream,"body":shape.body}));
    Ok(())
}
