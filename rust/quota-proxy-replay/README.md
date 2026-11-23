# Account usage proxy replay

The fixture runs the pinned usage endpoint and the Rust server in isolated Linux namespaces. It verifies relay requests for ClinePass API keys, fresh GitHub OAuth, DeepSeek, Synthetic, Groq, GLM in both regions, Vercel AI Gateway and OpenCode Go and Zen. A local HTTP proxy rejects a CONNECT so the fixture can observe the direct TLS fallback. It also checks the saved pool's no-proxy rule and the connection's legacy proxy after an inactive pool.

The strict case verifies authenticated access, error redaction, relay redirect refusal and unchanged SQLite credentials. GitHub OAuth that needs refreshing still uses the compatibility route. The tests use a local relay, proxy and TLS socket. No provider receives requests.

```sh
replay="$PWD/rust/quota-proxy-replay"
binary="$PWD/rust/app/target/debug/server"
rust-anything app record "$replay" --inputs "$replay/inputs.jsonl" --no-traps --timeout 20 -- sh run.sh reference
rust-anything app replay "$replay"
rust-anything app replay "$replay" --build "sh run.sh native $binary" --timeout 20
sh "$replay/run.sh" native "$binary" strict
```
