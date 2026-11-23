# Connect CLI replay

The reference and the Rust command use a loopback dashboard, fixture credentials, and temporary config directories. Replay compares requests, exit status, stdout, stderr, configuration, backup contents, file modes, and symlinks for all nine supported tools.

TOML and YAML settings are compared after parsing. JSON settings and backups are compared as written. Python needs PyYAML for the OMP fixtures. Linux PTY cases check selection, cancellation, password masking, and terminal restoration.

```sh
replay="$PWD/rust/cli-connect-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urn sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps -- python3 driver.py original &&
  rust-anything app replay "$1" --build "python3 driver.py native $2"' \
  sh "$replay" "$replay/target/debug/cli-connect-replay"
```
