# Configured gateway parity

The recorded reference is upstream 9Router commit `1855aa44473e88feac9d3f131b9c0c5fc9706b68`, the base of the Rust migration. It runs the upstream CLI package's custom HTTP entry point so direct and forwarded requests get the same peer checks as an installed launcher.

The fixture in `profile.json` captures the configured providers, visible models, disabled models, combo, and request paths observed on 2026-11-23. Credentials, node addresses, and account IDs come from the harness. Each case runs in a new Linux network namespace with a temporary home and database. Mock upstreams supply fixed completions, streaming events, tool calls, and errors.

The comparison covers the HTTP result and upstream calls. It captures JSON, SSE data, status, content type, redirect location, and cookie names. Generated IDs, secrets, OAuth nonces, temporary paths, and timing fields are normalized. HTML cases check page access and redirect behavior. Live provider authentication, quota, actual inference, and browser interactions require separate tests.

## Replay the recorded cases

Run from this directory with a release binary and `rust-anything 1.5.1`.

```sh
export PARITY_RUST_BIN="$(realpath ../../target/release/ninerouter)"
export PYTHONDONTWRITEBYTECODE=1
rust-anything app replay . --build 'python3 run.py rust'
```

Point `PARITY_RUST_BIN` at the actual binary if Cargo used a target triple. Replay only one group with `--case NAME`. A strict mismatch exits with status 1 and writes a diff to `rust/app-cases/replay-report.md`.

## Rebuild the original

Use Node.js 22, npm, Python 3, `unshare -Urn`, and `ip`. The upstream build downloads fonts. Build outside the workspace and set `DATA_DIR` before running the upstream build.

```sh
reference=$(mktemp -d)
mkdir -p "$reference/source" "$reference/home" "$reference/data"
git -C ../.. archive 1855aa44473e88feac9d3f131b9c0c5fc9706b68 | tar -x -C "$reference/source"
cp locks/app-package-lock.json "$reference/source/package-lock.json"
cp locks/cli-package-lock.json "$reference/source/cli/package-lock.json"
(
  cd "$reference/source"
  export HOME="$reference/home" DATA_DIR="$reference/data"
  export DISABLE_BACKGROUND_TOKEN_REFRESH=1 NEXT_TELEMETRY_DISABLED=1
  npm ci --ignore-scripts
  npm --prefix cli ci --ignore-scripts
  npm --prefix cli run build
)
export PARITY_NODE_SERVER="$reference/source/cli/app/custom-server.js"
```

Write new expectations only when changing the version or adding a scenario. Run the original against the recorded corpus to check the reference.

```sh
export PYTHONDONTWRITEBYTECODE=1
rust-anything app replay . --build 'python3 run.py node'
```

Record after making an intentional corpus change.

```sh
python3 run.py cases cases.jsonl
rust-anything app record . --inputs cases.jsonl --no-traps --name 9router-upstream-1855aa44 --timeout 45 -- python3 run.py node
```
