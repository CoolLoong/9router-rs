# Ollama Local executor replay

The reference runs the pinned executor and Ollama translators. The Rust adapter builds the same requests, calls a loopback simulator, and returns Chat Completions JSON or SSE. The cases compare endpoint selection, the credential chosen, options, tool history and deltas, request headers, and token counts.

```sh
replay="$PWD/rust/ollama-local-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urnpf --mount-proc sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps -- python3 driver.py original &&
  rust-anything app replay "$1" --build "python3 driver.py native $2" &&
  for fixture in ndjson-standard bad-stream-media; do
    python3 "$1/driver.py" strict "$2" "$fixture" || exit
  done' sh "$replay" "$replay/target/debug/ollama-local-replay"
```

The extra cases verify native handling of the standard `application/x-ndjson` content type and preserve the existing gateway's handling of an HTML relay response.
