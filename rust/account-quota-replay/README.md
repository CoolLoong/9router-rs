# Account quota replay

The reference runs the pinned account usage, Codex reset-credit status and redemption, and Claude reset-grant handlers. The Rust adapter exercises the native quota routes, shared OAuth refresh and SQLite credential storage. Each case uses a private loopback network and temporary data directory. Snapshots record credential checks and rotation without printing credential values.

```sh
replay="$PWD/rust/account-quota-replay"
cargo build --manifest-path "$replay/Cargo.toml"
binary="$replay/target/debug/account-quota-replay"
rust-anything app record "$replay" --inputs "$replay/inputs.jsonl" --no-traps --timeout 20 -- sh run.sh original
rust-anything app replay "$replay" --build "sh run.sh native $binary" --timeout 20
for fixture in codex-reset-redacted proxied proxy-added-on-refresh clinepass-proxied github-proxied \
  clinepass-malformed-key clinepass-redacted github-redacted codex-consume-ambiguous \
  codex-consume-malformed codex-consume-redirect codex-consume-disconnected codex-consume-redacted \
  claude-consume-profile-invalid claude-consume-redacted; do
  sh "$replay/run.sh" strict "$binary" "$fixture" || exit
done
```

The fixtures exercise quota reads, refresh retries, Claude caching and cooldown, and Codex reset-credit status reads. ClinePass cases cover API keys, WorkOS OAuth, percentage windows, and malformed provider responses. GitHub cases cover paid and free quota payloads, access token selection, and GitHub/Copilot refresh order.

Manual reset cases validate the server-generated IDs, the same ID after a Codex authentication rejection and OAuth refresh, and Claude quota cache invalidation. The full Rust entry point also runs without a Node binary with a temporary session cookie and synthetic provider accounts.

```sh
server="$PWD/rust/app/target/debug/server"
for fixture in codex-consume-access codex-consume-unauthorized claude-consume; do
  sh "$replay/run.sh" full "$server" "$fixture" || exit
done
```
