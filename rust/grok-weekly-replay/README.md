# Grok Build weekly usage replay

The direct Grok CLI quota reader uses the pinned `GetGrokCreditsConfig` read-only gRPC-Web call. It sends the saved bearer and identifies itself as 9Router. Stored subscription details are used for display. The native reader covers the weekly shared pool. The pinned REST branches cover monthly, on-demand and prepaid figures. [Weekly usage help](https://docs.x.ai/grok/faq)

The replay compares the pinned gRPC branch with the Rust gateway using framed and raw protobuf data, omitted usage, malformed data and a denial. The strict run covers authorization, expired and missing credentials, redirect refusal, gRPC error trailers and unchanged saved credentials. The gateway runs without Node and all requests stay on loopback. Live endpoint acceptance remains unverified.

```sh
replay="$PWD/rust/grok-weekly-replay"
binary="$PWD/rust/app/target/debug/server"
rust-anything app record "$replay" --inputs "$replay/inputs.jsonl" --no-traps --timeout 20 -- sh run.sh reference
rust-anything app replay "$replay"
rust-anything app replay "$replay" --build "sh run.sh native $binary" --timeout 20
sh "$replay/run.sh" native "$binary" strict
```
