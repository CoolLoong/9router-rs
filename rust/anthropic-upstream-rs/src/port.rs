//! The Rust port of `open-sse/translator/response/claude-to-openai.js`.
//!
//! Replace each `unimplemented!` with the port of the source function. Return `Err` where
//! the source throws or raises: the caller gets an error there too. After changing a
//! signature, run `rust-anything refresh` to regenerate the bindings. `rust-anything diff` checks
//! behavior against what the source recorded.

#![forbid(unsafe_code)]

use serde_json::{Value, json};

#[path = "../../app/src/anthropic_upstream.rs"]
pub mod anthropic_upstream;

fn fixture_time() -> i64 {
    1_785_000_123_456
}

pub fn claude_to_open_ai_response(chunk: Value, _state: Value) -> Result<Value, String> {
    match chunk["mode"].as_str() {
        Some("json") => Ok(anthropic_upstream::to_chat_at(
            chunk["data"].clone(),
            fixture_time(),
        )),
        Some("stream") => {
            let mut stream = anthropic_upstream::ClaudeToChat::with_clock(fixture_time);
            let mut frames = Vec::new();
            for frame in chunk["frames"].as_array().into_iter().flatten() {
                frames.push(stream.push(frame["event"].as_str(), &frame["data"]));
            }
            frames.push(stream.finish());
            Ok(json!({"frames": frames, "usage": stream.usage()}))
        }
        _ => Ok(Value::Null),
    }
}
