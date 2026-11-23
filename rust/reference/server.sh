#!/usr/bin/env sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
test -f "$root/.next/standalone/server.js"
reference_data=${NINEROUTER_MIGRATION_DATA_DIR:-$(mktemp -d -t 9router-reference.XXXXXXXX)}
if [ "${NINEROUTER_MIGRATION_FIXTURE:-}" = 1 ]; then
    python3 "$root/rust/reference/seed.py" "$reference_data"
    exec env -i PATH="$PATH" PORT="${PORT:?}" HOSTNAME=127.0.0.1 \
      DATA_DIR="$reference_data" NEXT_TELEMETRY_DISABLED=1 \
      MODEL_CATALOG_SYNC="${MODEL_CATALOG_SYNC:-}" \
      node --import "$root/rust/reference/mock-models.mjs" "$root/.next/standalone/custom-server.js"
fi
exec env PORT="${PORT:?}" HOSTNAME=127.0.0.1 \
  DATA_DIR="$reference_data" NEXT_TELEMETRY_DISABLED=1 \
  node "$root/.next/standalone/custom-server.js"
