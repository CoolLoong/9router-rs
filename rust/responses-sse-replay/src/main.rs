#[path = "../../app/src/responses_sse.rs"]
mod responses_sse;

use responses_sse::ResponsesSse;
use serde_json::Value;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input: Value = serde_json::from_str(&std::env::args().nth(1).ok_or("missing input")?)?;
    let mut state = ResponsesSse::with_custom_tools(
        input["custom_tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned),
    );
    for (index, chunk) in input["chunks"]
        .as_array()
        .ok_or("missing chunks")?
        .iter()
        .enumerate()
    {
        let events = if chunk.is_null() {
            state.finish()
        } else {
            state.push(chunk)
        };
        for event in events {
            print!("{event}");
        }
        println!(
            "step {index} pending={} completed={}",
            state.completion_pending(),
            state.is_completed()
        );
    }
    Ok(())
}
