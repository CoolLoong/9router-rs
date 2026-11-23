# Cline and ClinePass transport replay

Each case starts the pinned server or the Rust gateway in its own loopback network namespace with an isolated SQLite store. The fixtures check saved bearer credentials, JSON envelope decoding, SSE, upstream errors, rotating OAuth credentials, and both model discovery routes. Rust cases require zero route fallbacks.

```sh
replay="$PWD/rust/cline-transport-replay"
binary="$PWD/rust/app/target/debug/server"
rust-anything app record "$replay" --inputs "$replay/inputs.jsonl" --no-traps --timeout 20 -- sh run.sh reference
rust-anything app replay "$replay" --build "sh run.sh rust $binary" --timeout 20
for case in oauth-normalized oauth-rejected-refresh pass-oauth-refresh; do
  sh "$replay/run.sh" rust "$binary" "$case"
done
```

The three extra Rust cases cover a bare WorkOS JWT, rejected-token refresh with an unprefixed token response, and ClinePass OAuth refresh. The pinned chat executor overwrites its normalized header with the original bare token. Rust retains the WorkOS prefix while faithfully sending plain ClinePass API keys. The Rust request reports `unknown` for the JavaScript runtime version.
