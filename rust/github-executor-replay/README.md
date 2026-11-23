# GitHub Copilot executor replay

The reference calls the pinned GitHub executor through a loopback transport. The Rust adapter builds the same requests and converts Responses events to Chat Completions chunks. Replay compares the credential selected, request headers and bodies, alternate-endpoint learning, usage and tool deltas. It normalizes generated request IDs and chunk timestamps.

```sh
replay="$PWD/rust/github-executor-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urnpf --mount-proc sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps -- python3 driver.py original &&
  rust-anything app replay "$1" --build "python3 driver.py native $2" &&
  python3 "$1/driver.py" strict "$2" blocked' sh "$replay" "$replay/target/debug/github-executor-replay"
```

The blocked-header case checks that direct database overrides cannot replace the stored credential or request framing. The provider OAuth replay covers Copilot-first refresh and GitHub OAuth rotation when the first Copilot exchange fails.
