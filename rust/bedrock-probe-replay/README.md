# Bedrock dashboard credential probe replay

The recorded case exercises the pinned dashboard handlers and the Rust gateway against a loopback AWS control plane. It verifies SigV4 on `GET /foundation-models`, IAM denial as credential acceptance, invalid signatures, saved connection state and batch verdicts. The per-connection listing route returns the pinned unsupported reply.

The strict case covers process-profile rejection, redirect refusal, the eight-second probe timeout, error redaction and the static gateway catalog. The direct cases also verify named AWS profiles. The [Bedrock proxy fixture](../bedrock-proxy-replay/README.md) covers configured proxy routes for explicit credentials.

```sh
replay="$PWD/rust/bedrock-probe-replay"
binary="$PWD/rust/app/target/debug/server"
unshare -Urn sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps --timeout 24 -- python3 driver.py reference &&
  rust-anything app replay "$1" &&
  rust-anything app replay "$1" --build "python3 driver.py native $2" --timeout 24 &&
  python3 "$1/driver.py" native "$2" strict' sh "$replay" "$binary"
```
