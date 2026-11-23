# GLM Coding Plan consent replay

The fixture runs the pinned GLM consent routes and the standalone Rust server against local Z.ai endpoint simulations. It compares pending and denied authorization, selecting the default personal project, reuse of an existing coding plan key, and creating and copying a missing key.

The strict case checks the dashboard session and Z.ai issuer, the local poll interval, an expired code, malformed key lists, redirect refusal, and two simultaneous polls during an ambiguous create. A new authorization reads the key list and reuses the previously created key. Rust stores the coding plan API key and account identity. The auxiliary sign-in and business tokens stay in memory.

```sh
replay="$PWD/rust/glm-device-replay"
binary="$PWD/rust/app/target/debug/server"
rust-anything app record "$replay" --inputs "$replay/inputs.jsonl" --no-traps --timeout 20 -- sh run.sh reference
rust-anything app replay "$replay"
rust-anything app replay "$replay" --build "sh run.sh native $binary" --timeout 20
sh "$replay/run.sh" native "$binary" strict
```
