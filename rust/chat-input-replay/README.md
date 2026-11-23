# OpenAI-compatible chat input replay

Compare gateway input framing with the original OpenAI translator and DefaultExecutor. The reference calls request-shaping functions directly and never opens a provider connection. From the repository root:

```sh
replay=rust/chat-input-replay
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urn -- rust-anything app record "$replay" --inputs "$replay/inputs.jsonl" --no-traps -- node --no-warnings original.mjs
unshare -Urn -- rust-anything app replay "$replay" --build target/debug/chat-input-replay
```
