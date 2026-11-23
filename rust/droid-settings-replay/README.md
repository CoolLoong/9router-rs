# Factory Droid settings replay

The reference uses the pinned Droid settings handlers. Both commands run in a temporary home. The fixtures compare installation detection, JSONC reads, multi-model updates, activation order, reset behavior, symlinks, and existing permissions. Synthetic API keys are checked and removed from snapshots.

```sh
replay="$PWD/rust/droid-settings-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urnpf --mount-proc sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps -- python3 driver.py original &&
  rust-anything app replay "$1" --build "python3 driver.py native $2" &&
  python3 "$1/driver.py" strict "$2" new-mode' sh "$replay" "$replay/target/debug/droid-settings-replay"
```

The native-only case checks mode `0600` for a newly created settings file.
