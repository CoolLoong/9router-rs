#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."
native=${2:-rust/app/target/debug/server}

if [ "${1:-}" = "record" ]; then
  combined=$(mktemp -t 9router-http-cases.XXXXXXXX)
  trap 'unlink "$combined"' EXIT
  cat rust/reference/http-inputs.jsonl rust/reference/*-http.jsonl > "$combined"
  unshare -Urn -- sh -c '
    ip link set lo up &&
    NINEROUTER_MIGRATION_FIXTURE=1 rust-anything app record . --http --no-traps --timeout 12 \
      --normalize timestamps --normalize uuids \
      --normalize "created-seconds=\"created\":[0-9]{10}" \
      --normalize "key-nonce=sk-[0-9a-f]{16}-[a-z0-9]{6}-[a-f0-9]{8}" \
      --normalize "web-timing=\"(response_time_ms|upstream_latency_ms)\":[0-9]+" \
      --normalize "locale-expiry=Expires=[A-Z][a-z]{2}, [0-9]{2} [A-Z][a-z]{2} [0-9]{4} ([0-9:]{8}|\[\.\.\]) GMT" \
      --inputs "$1" -- sh rust/reference/server.sh' sh "$combined"
fi

unshare -Urn -- sh -c '
  ip link set lo up && export NINEROUTER_MIGRATION_FIXTURE=1 &&
  rust-anything app replay . &&
  rust-anything app replay . --build "$1"' sh "$native"
