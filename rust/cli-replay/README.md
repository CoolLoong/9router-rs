# 9Router command line replay

The reference executes the pinned launcher help and subcommands with runtime installation stubbed out and all configuration reads restricted to synthetic fixture directories. Startup mode evaluates the pinned argument parser in isolation, before any server or browser can start. No real config, service or provider is touched.

```sh
replay=rust/cli-replay
cargo build --manifest-path "$replay/Cargo.toml"
unshare -Urn -- rust-anything app record "$replay" \
  --inputs "$replay/inputs.jsonl" --no-traps \
  --normalize 'dir=/[^"[:space:]\(\)]+/fixture-(home|xdg)' -- node --no-warnings original.cjs
unshare -Urn -- rust-anything app replay "$replay" --build target/debug/cli-replay
```
