# Kiro AWS device authorization replay

The comparison uses the upstream GET and POST handlers and the Rust router against a loopback AWS simulator. It checks Builder ID, regional IAM Identity Center, pending polls, profile ARN discovery, and SQLite account fields. Snapshots retain the call sequence and account outcomes.

The Rust `device_code` is an opaque local handle. The service keeps AWS device credentials until the grant completes or expires. The strict checks cover concurrent polling, cooldown, redirects, transport errors, caller disconnect, single-use completion and credential redaction.

```sh
replay="$PWD/rust/kiro-device-replay"
binary="$PWD/rust/app/target/debug/server"
unshare -Urn sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps --timeout 24 -- python3 driver.py reference &&
  rust-anything app replay "$1" &&
  rust-anything app replay "$1" --build "python3 driver.py native $2" --timeout 24 &&
  python3 "$1/driver.py" native "$2" strict' sh "$replay" "$binary"
```
