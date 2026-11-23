# Kiro OAuth proxy replay

Supported Kiro OAuth chats can use a configured HTTP CONNECT or SOCKS proxy with a matching saved regional profile. The gateway renews an expiring credential through the configured proxy. It can also renew after the first authentication rejection and resend the same request once. Direct backup accounts and redirects stay outside that protected attempt.

The local TLS fixture verifies JSON, SSE, tool calls, inline images and session continuation. It checks credential rotation, request equality across the authenticated retry, and cancellation of renewal after the proxy setting changes. A separate loopback run observes the canonical AWS CONNECT authority and TLS server name and checks certificate rejection before bearer delivery. The Rust gateway runs with an empty executable search path.

```sh
sh rust/kiro-proxy-replay/run.sh "$PWD/rust/app/target/debug/server"
```
