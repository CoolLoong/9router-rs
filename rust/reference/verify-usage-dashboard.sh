#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."

work=$(mktemp -d -t 9router-usage-dashboard-contract.XXXXXXXX)
anchor=$(date +%s)
unshare -Urn -- sh -c '
  ip link set lo up &&
  rust-anything app record "$1" --no-traps --timeout 25 --name usage-dashboard-contract -- python3 "$2" reference "$3" &&
  rust-anything app replay "$1" --build "python3 $2 rust $3"
' sh "$work" "$PWD/rust/reference/usage-dashboard-contract.py" "$anchor"
