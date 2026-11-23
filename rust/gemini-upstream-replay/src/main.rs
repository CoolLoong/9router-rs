#[allow(dead_code)]
#[path = "../../app/src/chat_input.rs"]
mod chat_input;
#[allow(dead_code)]
#[path = "../../app/src/gemini_upstream.rs"]
mod gemini_upstream;

use gemini_upstream::{GeminiToChat, raw_usage, to_chat_at};
use serde_json::{Value, json};

fn fixture_time() -> i64 {
    1_785_000_123_456
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input: Value = serde_json::from_str(&std::env::args().nth(1).ok_or("missing fixture")?)?;
    let result = if input["mode"] == "json" {
        let data = &input["data"];
        json!({"result":to_chat_at(data.clone(), fixture_time), "accounting_usage":raw_usage(data)})
    } else {
        let model = input["model"].as_str().unwrap_or_default();
        let mut state = GeminiToChat::with_clock(model, fixture_time);
        let mut frames = Vec::new();
        for frame in input["frames"].as_array().ok_or("missing frames")? {
            frames.push(state.push(frame["event"].as_str(), &frame["data"]));
        }
        frames.push(state.finish());
        let signatures: Vec<_> = state.take_thought_signatures().into_iter().map(|record| {
            json!({"tool_call_id":record.tool_call_id, "signature":record.signature, "model":record.model})
        }).collect();
        json!({"frames":frames,"usage":state.usage(),"signatures":signatures})
    };
    println!("{result}");
    Ok(())
}
