#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."
work=$(mktemp -d -t 9router-accounts-replay.XXXXXXXX)
exec unshare -Urn -- sh -c '
  ip link set lo up &&
  export NINEROUTER_MIGRATION_FIXTURE=1 &&
  rust-anything app record "$1" --http --no-traps --timeout 12 --normalize timestamps     --inputs "$2/chat-account-rules-rs/accounts-http.jsonl" -- sh "$2/reference/server.sh" &&
  rust-anything app replay "$1" &&
  rust-anything app replay "$1" --build "$2/app/target/debug/server"
' sh "$work" "$PWD/rust"
