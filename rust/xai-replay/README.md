# xAI gateway replay

The recorded cases exercise the pinned gateway and the standalone Rust server against isolated loopback endpoints. They cover bearer API-key Chat and translated Responses, public PKCE authorization and exchange, discovery, proactive OAuth rotation, fixed-port callback exchange, manual code completion and the original behavior for a provider rejection of a fresh access token.

The strict case checks dashboard authorization, redirect and PKCE validation, single refresh during concurrent turns, token error redaction, callback origin checks and cancellation. The callback poll response omits the stored verifier. Each candidate process gets a temporary home and an empty executable search path. The harness fails if the native gateway records a fallback.

```sh
replay="$PWD/rust/xai-replay"
binary="$PWD/rust/app/target/debug/server"
rust-anything app record "$replay" --inputs "$replay/inputs.jsonl" --no-traps --timeout 20 -- sh run.sh reference
rust-anything app replay "$replay"
rust-anything app replay "$replay" --build "sh run.sh native $binary" --timeout 20
sh "$replay/run.sh" native "$binary" strict
```
