# OpenAI Responses SSE replay

The reference calls the original direct OpenAI to Responses translator and SSE formatter. The Rust program runs the gateway's pure state machine and captures each push, its completion state and its complete frames. No provider connection is made.

```sh
replay=rust/responses-sse-replay
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urn -- rust-anything app record "$replay" \
  --inputs "$replay/inputs.jsonl" --no-traps \
  --normalize 'response-time="created_at":[0-9]+' \
  --normalize 'fallback-id=resp_[0-9]{10,}' -- node --no-warnings original.mjs
unshare -Urn -- rust-anything app replay "$replay" --build target/debug/responses-sse-replay
```
