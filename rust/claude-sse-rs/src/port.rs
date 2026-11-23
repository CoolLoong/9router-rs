//! The Rust port of `open-sse/translator/response/openai-to-claude.js`.
//!
//! Replace each `unimplemented!` with the port of the source function. Return `Err` where
//! the source throws or raises: the caller gets an error there too. After changing a
//! signature, run `rust-anything refresh` to regenerate the bindings. `rust-anything diff` checks
//! behavior against what the source recorded.

#![forbid(unsafe_code)]

use serde_json::{Value, json};

#[path = "../../app/src/claude_sse.rs"]
pub mod claude_sse;

pub fn openai_to_claude_response(chunk: Value, state: Value) -> Result<Value, String> {
    let Some(chunks) = chunk.as_array() else {
        return Ok(Value::Null);
    };
    let mut stream = claude_sse::ClaudeSse::new();
    let mut frames = Vec::with_capacity(chunks.len() + 1);
    stream.set_usage(state.get("usage").cloned().filter(|usage| !usage.is_null()));
    for chunk in chunks {
        frames.push(stream.push(chunk));
    }
    frames.push(stream.finish());
    Ok(json!({
        "frames": frames,
        "usage": stream.usage(),
        "finished": stream.finished(),
    }))
}
