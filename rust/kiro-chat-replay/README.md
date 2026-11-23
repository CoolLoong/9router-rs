# Kiro chat replay

The reference runs the pinned OpenAI-to-Kiro translator and AWS EventStream decoder. Both adapters send standard account authorization and a 9Router User-Agent to a loopback simulator. The Rust checks cover text history, OpenAI tool calls and results, inline images, model aliases, reasoning settings, both client stream preferences, UTF-8, CRC-checked frames, stop handling, and usage accounting.

```sh
replay="$PWD/rust/kiro-chat-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urnpf --mount-proc sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps --timeout 20 -- python3 driver.py reference &&
  rust-anything app replay "$1" --build "python3 driver.py native $2" --timeout 20 &&
  for case in refusal cancelled unknown-stop exception corrupt truncated tool-output wrong-content-type empty-event \
    split-thinking unauthorized redirect unsupported-region explicit-session oauth-input tools-input vision-input \
    thinking-input oauth-expired-input oauth-refresh-input oauth-cross-region-input oauth-proxy-input \
    oauth-missing-profile-input oauth-invalid-profile-input oauth-invalid-method-input oauth-rejected oauth-bad-payload; do
    python3 "$1/driver.py" native "$2" "$case" || exit
  done' sh "$replay" "$replay/target/debug/kiro-chat-replay"
```

The native Kiro route supports saved API keys and current OAuth credentials with an explicit, same-region profile in `us-east-1` or `eu-central-1`. OAuth covers IAM Identity Center, Builder ID, social methods and external IdP. Native OAuth carries the saved profile in the body and header, with `TokenType: EXTERNAL_IDP` for that method. Direct `us-east-1` OAuth may retry CodeWhisperer once after a 401, 403 or 404 from Amazon Q.

The route validates the complete turn before emitting assistant deltas. The initial SSE comment keeps the connection active while validating. An invalid tool fragment cannot erase a complete declared tool call in the same turn. The reference route handles explicit session replay, non-canonical tool history, provider repair retries, and remaining multimodal formats. Verification uses loopback providers.

The full gateway can be checked in a private loopback namespace with `rust/reference/kiro-chat-contract.py`. The [Kiro OAuth proxy replay](../kiro-proxy-replay/README.md) exercises HTTP CONNECT and SOCKS.

Native refresh uses the saved same-region account scope and the issuer associated with the saved method. The first refresh attempt is recorded before transport so an ambiguous result cannot automatically replay the same token after restart. A confirmed throttle can be retried by a later request. The issuer and persistence checks run without Node using `rust/reference/kiro-refresh-contract.py`, and its `reference` and `compare` modes compare the pinned provider's issuer and token mapping.
