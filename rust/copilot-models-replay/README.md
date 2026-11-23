# Copilot model discovery replay

The replay exercises the pinned and standalone Rust implementations of `/api/providers/{id}/models` and `/v1/models`. Each case uses temporary SQLite storage and an upstream simulator in its own loopback-only network namespace. Snapshots compare the management response, public GitHub model rows, outbound calls, and credential writeback. The simulator also checks the upstream headers and reports credentials by fixture label.

```sh
replay="$PWD/rust/copilot-models-replay"
binary="$PWD/rust/app/target/debug/server"
cargo build --manifest-path rust/app/Cargo.toml --features native-package --locked
rust-anything app record "$replay" --inputs "$replay/inputs.jsonl" --no-traps -- python3 driver.py original
rust-anything app replay "$replay" --build "python3 driver.py native $binary"
for fixture in live-proactive live-proxy; do
  python3 "$replay/driver.py" strict "$binary" "$fixture" || exit
done
```

`live-force` compares requests with a cold cache. The pinned service clears its cache and the Rust replay restarts the gateway with the same database. The Rust API has no force-refresh option. The extra cases verify proactive refresh and that an unavailable configured proxy prevents a direct upstream call. The driver requires an empty gateway command path and checks for zero legacy fallback calls.
