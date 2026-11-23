# Speech provider replay

The reference runs the pinned speech handler and the generic Deepgram adapter. The Rust fixture uses the migrated transports against a loopback simulator. Replay compares endpoint paths, request fields, authorization, audio byte hashes, formats, and error handling. Deepgram remains a transcription provider in the pinned registry; its generic TTS adapter is exercised separately.

```sh
replay="$PWD/rust/speech-providers-replay"
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urnpf --mount-proc sh -c 'ip link set lo up &&
  rust-anything app record "$1" --inputs "$1/inputs.jsonl" --no-traps -- python3 driver.py original &&
  rust-anything app replay "$1" --build "python3 driver.py native $2" &&
  python3 "$1/driver.py" strict "$2" eleven-redacted' sh "$replay" "$replay/target/debug/speech-providers-replay"
```
