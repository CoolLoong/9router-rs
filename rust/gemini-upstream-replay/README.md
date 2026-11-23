# Gemini upstream decoding replay

The reference executes the pinned Gemini JSON and SSE conversion code with a fixed clock and an in-memory signature receiver. The Rust program runs the gateway decoder. No provider or SQLite connection is opened.

```sh
replay=rust/gemini-upstream-replay
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urn -- rust-anything app record "$replay" --inputs "$replay/inputs.jsonl" --no-traps -- node --no-warnings original.mjs
unshare -Urn -- rust-anything app replay "$replay" --build target/debug/gemini-upstream-replay
```
