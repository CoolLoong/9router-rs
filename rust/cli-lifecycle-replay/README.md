# Linux launcher lifecycle replay

The replay compares publication and cleanup of the launcher PID file, port reservation, replacement on the same or a different port, and recovery from a stale record. The upstream adapter calls the pinned launcher process helpers with launcher subprocesses owned by the fixture. Extra Rust checks cover identity mismatch, changing the executable at its installed path, a conflicting listener, concurrent startup, and shutdown timeout.

```sh
replay="$PWD/rust/cli-lifecycle-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urnpf --mount-proc sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps -- python3 driver.py original &&
  rust-anything app replay "$1" --build "python3 driver.py native $2" &&
  for fixture in unrelated-port different-occupied-port address-mismatch start-time-mismatch lock-proof-mismatch unpublished legacy-live copied-identity startup-lock ipv6-replacement executable-replaced stubborn; do
    python3 "$1/driver.py" safety "$2" "$fixture" || exit
  done' sh "$replay" "$replay/target/debug/cli-lifecycle-replay"
```
