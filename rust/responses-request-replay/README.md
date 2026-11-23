# Native Responses request replay

The reference runs the pinned translator and DefaultExecutor request shaping with a synthetic OpenAI-compatible Responses provider. The Rust program runs the gateway's Responses target builder. Replay checks the transformed body and transport stream preference separately. No provider request is made.

```sh
replay=rust/responses-request-replay
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urn -- rust-anything app record "$replay" --inputs "$replay/inputs.jsonl" --no-traps -- node --no-warnings original.mjs
unshare -Urn -- rust-anything app replay "$replay" --build target/debug/responses-request-replay
```
