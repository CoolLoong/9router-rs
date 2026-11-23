#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."
for profile in chat responses; do
  work=$(mktemp -d -t "9router-$profile-usage.XXXXXXXX")
  unshare -Urn -- sh -c '
    ip link set lo up &&
    rust-anything app record "$1" --no-traps --timeout 20 --name usage-contract -- python3 "$2" reference "$3" &&
    rust-anything app replay "$1" --build "python3 $2 rust $3"
  ' sh "$work" "$PWD/rust/reference/usage-contract.py" "$profile"
done
