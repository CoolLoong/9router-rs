# Codex dashboard settings and profiles replay

The reference invokes the pinned Next.js handlers. Both launchers use a temporary home and a private loopback interface. The cases compare CLI detection, settings and profiles responses, semantic TOML configuration, legacy auth cleanup, symlinks, and preservation of existing permissions. Snapshots replace synthetic credentials with placeholders.

```sh
replay="$PWD/rust/codex-settings-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urnpf --mount-proc sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps -- python3 driver.py original &&
  rust-anything app replay "$1" --build "python3 driver.py native $2" &&
  python3 "$1/driver.py" strict "$2" settings-mode &&
  python3 "$1/driver.py" strict "$2" profile-mode' sh "$replay" "$replay/target/debug/codex-settings-replay"
```

The two native-only cases require permissions `0600` for new settings and profile files.
