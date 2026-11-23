# Bedrock dashboard proxy replay

Bedrock and Bedrock xAI use the signed AWS `GET /foundation-models` request for credential validation. Validate uses the configured outbound proxy. Saved Test and batch also honor the account's proxy pool and connection settings. HTTP CONNECT and SOCKS use the AWS HTTPS origin and certificate verification. A relay or unavailable strict proxy returns an error. The probe makes one signed request and refuses redirects and direct retries. The signed AWS probe serves as the connectivity check for this path. The pinned JavaScript Test handler separately checks Google before querying AWS.

The fixture exercises HTTP Basic proxy authentication, SOCKS5h, SigV4, session-token handling, IAM `AccessDeniedException`, redacted signature rejection, redirects, and saved and batch results. A local CA authenticates only `localhost` in the debug fixture. A separate run with normal trust and the real AWS destination checks the CONNECT authority and TLS name through the loopback tunnel. That run rejects the localhost certificate before any signed HTTP request. Relay and direct-fallback traps stay unused. Both runs use an empty executable path and an isolated network namespace.

```sh
sh rust/bedrock-proxy-replay/run.sh "$PWD/rust/app/target/debug/server"
```
