# Chat protocol input replay

The reference runs the original Anthropic or Responses translator and OpenAI-compatible executor's pure request shaping. The Rust program runs the gateway's protocol input and preparation modules. No provider connection is made.

From the repository root:

```sh
replay=rust/chat-protocol-input-replay
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urn -- rust-anything app record "$replay" --inputs "$replay/inputs.jsonl" --no-traps -- node --no-warnings original.mjs
unshare -Urn -- rust-anything app replay "$replay" --build target/debug/chat-protocol-input-replay
```
