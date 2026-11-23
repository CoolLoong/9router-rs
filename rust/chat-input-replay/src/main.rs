#[path = "../../app/src/chat_input.rs"]
mod chat_input;
use serde_json::{Value, json};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let value: Value = serde_json::from_str(&std::env::args().nth(1).ok_or("missing body")?)?;
    let (model, accept) = (
        value["model"].as_str().ok_or("model")?,
        value["accept"].as_str().unwrap_or(""),
    );
    let body = value["body"].clone();
    let output = chat_input::prepare(body, model, accept);
    let result = output.map(|output| json!({"stream":output.stream,"body":output.body}));
    println!("{}", json!(result));
    Ok(())
}
