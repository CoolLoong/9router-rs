#!/bin/sh
set -eu
dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
exec unshare -Urn sh -c 'ip link set lo up && exec python3 "$@"' sh "$dir/driver.py" "$@"
