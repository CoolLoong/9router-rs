#!/bin/sh
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
exec unshare -Urnpf --mount-proc sh -c 'ip link set lo up && exec python3 "$@"' sh "$root/rust/reference/cline-transport-contract.py" "$@"
