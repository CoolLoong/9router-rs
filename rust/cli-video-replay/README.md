# xAI video CLI replay

The pinned JavaScript command and the Rust command both run against a synthetic loopback gateway. Replay compares exit status, output, request body and affinity, authorization handling, downloaded bytes, and temporary-file cleanup. No provider API is contacted.

```sh
replay="$PWD/rust/cli-video-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urn sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps -- python3 driver.py original &&
  rust-anything app replay "$1" --build "python3 driver.py native $2"' \
  sh "$replay" "$replay/target/debug/cli-video-replay"
```
