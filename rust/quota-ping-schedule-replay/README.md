# Quota ping schedule replay

The reference runs the pinned JavaScript normalization and due-slot selection in the same sequence as the scheduler. The Rust driver uses the production schedule module. The 27 cases evaluate 411 timestamps, including DST transitions, day rollover, numeric settings, and the grace window boundary.

From the repository root:

```sh
quota_replay="$PWD/rust/quota-ping-schedule-replay"
cargo build --locked --manifest-path "$quota_replay/Cargo.toml"
rust-anything app record "$quota_replay" --inputs "$quota_replay/inputs.jsonl" --no-traps --timeout 8 -- node --disable-warning=MODULE_TYPELESS_PACKAGE_JSON original.mjs
rust-anything app replay "$quota_replay"
rust-anything app replay "$quota_replay" --build "$quota_replay/target/debug/quota-ping-schedule-replay"
```
