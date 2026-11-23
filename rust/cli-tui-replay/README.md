# Linux management terminal replay

A private loopback server accepts a synthetic CLI token and records management requests. The reference runs the pinned Node Terminal UI. The Rust replay enters the launcher through a pseudoterminal before running the same actions. Replay checks menu navigation, selected status messages, the paths and bodies of mutations, and the set of management reads. The Rust run also checks terminal restoration and hides provider key input.

```sh
replay="$PWD/rust/cli-tui-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urn sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps -- python3 driver.py original &&
  rust-anything app replay "$1" --build "python3 driver.py native $2"' \
  sh "$replay" "$replay/target/debug/cli-tui-replay"
```
