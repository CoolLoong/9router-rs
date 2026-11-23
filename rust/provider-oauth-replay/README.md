# Provider OAuth replay

The upstream adapter calls the pinned OAuth provider modules and connection repository. The Rust adapter exposes the migrated management routes and writes the same SQLite schema. Both run against a synthetic loopback provider. The checks compare outbound fields, authorization responses, and persisted account details. Credential bytes are verified before being removed from the snapshots.

```sh
replay="$PWD/rust/provider-oauth-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urnpf --mount-proc sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps -- python3 driver.py original &&
  rust-anything app replay "$1" --build "python3 driver.py native $2" &&
  for fixture in gemini-state gemini-concurrent cross-provider claude-mismatched-suffix gemini-rejected github-unissued github-concurrent unauthorized unhandled codex-raw refresh-concurrent refresh-concurrent-invalid refresh-other-writer refresh-deleted refresh-disabled refresh-metadata refresh-cancel refresh-github-other-copilot refresh-copilot-raced \
    cline-state clinepass-state cline-token-rejected cline-direct-verified cline-direct-unverified \
    cline-direct-mismatched clinepass-direct-access-only refresh-background-due \
    refresh-background-fresh refresh-background-proxy; do
    python3 "$1/driver.py" strict "$2" "$fixture" || exit
  done' sh "$replay" "$replay/target/debug/provider-oauth-replay"
```

The additional cases exercise single-use authorization, state and redirect verification, delegated routes, concurrent polling, and redaction of provider errors. Refresh fixtures check the provider-specific encodings, rotation, overlapping requests, cancellation, and writes from another SQLite connection.

For Cline and ClinePass, Rust sends state in the authorization URL and requires the callback to return it. The OAuth provider receives the original authorization-code exchange fields. For an encoded legacy callback, the adapter uses the provider's profile response to verify the bearer. When the payload includes a refresh token, Rust exchanges it and requires the renewed bearer to resolve to the same account before saving. The tests keep the full flow on loopback.
