# Kiro model discovery replay

The reference uses the pinned account service's standard API-key and AWS profile requests on loopback. The pinned catalog resolver expands the mocked response to the same model variants shown by 9Router. The Rust adapter checks the request contract, error handling, variants, and cache.

```sh
replay="$PWD/rust/kiro-models-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urnpf --mount-proc sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps --timeout 20 -- python3 driver.py reference &&
  rust-anything app replay "$1" --build "python3 driver.py native $2" --timeout 20 &&
  for case in regional-cache-isolation profile-cache-isolation unsupported-modes redirect; do
    python3 "$1/driver.py" native "$2" "$case" || exit
  done' sh "$replay" "$replay/target/debug/kiro-models-replay"
```

The native transport identifies itself as 9Router. It supports saved headless API keys and directly authorized US East AWS Builder ID or IAM Identity Center profiles that can be read with the current bearer. The native-only checks verify that other account modes do not initiate a native request, cache results remain separated by region or profile, and redirects are rejected.
