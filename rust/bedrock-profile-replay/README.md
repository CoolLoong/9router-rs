# Bedrock named AWS profile replay

The reference calls the pinned gateway credential resolver and its installed AWS SDK. The Rust cases exercise the full gateway. Both run against loopback STS and IAM Identity Center fixtures with temporary HOME and configuration files.

The cases cover shared credentials overriding configuration fields, static session tokens, raw profile names, a two-level `source_profile` chain, signing-region separation, active named and legacy SSO caches, and renewal of an expired named SSO token. The strict checks cover concurrent STS use, profile cycles, unsupported process and metadata sources, JSON and EventStream redaction, concurrent SSO renewal across gateway processes, atomic cache replacement, and rejected OIDC calls.

The native Bedrock path uses the selected profile with static keys, bounded AssumeRole chains and cached SSO tokens. Expired named sessions can renew through the AWS OIDC refresh grant when the cache has a valid client registration and the configured issuer and region match. Legacy sessions require a new login after expiry.

```sh
replay="$PWD/rust/bedrock-profile-replay"
binary="$PWD/rust/app/target/debug/server"
rust-anything app record "$replay" --inputs "$replay/inputs.jsonl" --no-traps --timeout 24 -- sh run.sh reference
rust-anything app replay "$replay" --build "sh run.sh native $binary" --timeout 24
sh "$replay/run.sh" native "$binary" strict
sh "$replay/run.sh" native "$binary" renew-strict
```
