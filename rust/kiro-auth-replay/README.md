# Kiro account import replay

The reference uses the pinned Kiro dashboard handlers. The Rust adapter reuses the provider OAuth harness and exercises both Kiro routes with temporary SQLite storage. Every provider request is limited to the loopback simulator. Snapshots retain credential validation and rotation results and omit the credential strings.

```sh
replay="$PWD/rust/kiro-auth-replay"
adapter="$PWD/rust/provider-oauth-replay"
cargo build --manifest-path "$adapter/Cargo.toml"
unshare -Urnpf --mount-proc sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps --timeout 20 -- python3 driver.py reference &&
  rust-anything app replay "$1" --build "python3 driver.py native $2" --timeout 20 &&
  for case in idc-proprietary idc-bad-profile idc-malformed-client idc-rejected idc-missing-access key-redirect \
    idc-distinct-profile idc-overlap-identical idc-overlap-different idc-client-disconnect; do
    python3 "$1/driver.py" native "$2" "$case" || exit
  done' sh "$replay" "$adapter/target/debug/provider-oauth-replay"
```

API-key import always checks the current catalog response before creating a row. IAM Identity Center import uses the regional CreateToken refresh grant. Identical overlapping refresh requests share the result. Reimport matches the email, client, region and profile so distinct AWS grants stay separate. Import errors omit provider-supplied text that could contain a credential.

The full Rust gateway can also be checked with `rust/reference/kiro-auth-contract.py` in a private loopback namespace. It imports both credential types, verifies the SQLite rows and uses the imported API key to read the model catalog.
