#!/bin/sh
set -eu
fixture=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/update-fixture.py
exec unshare -Urnpf --mount-proc sh -c 'ip link set lo up && exec python3 "$@"' sh "$fixture" "$@"
