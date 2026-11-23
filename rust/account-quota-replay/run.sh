#!/bin/sh
set -eu
entry=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/driver.py
exec unshare -Urnpf --mount-proc sh -c 'ip link set lo up && exec python3 "$@"' sh "$entry" "$@"
