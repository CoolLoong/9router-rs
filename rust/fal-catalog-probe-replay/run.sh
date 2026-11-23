#!/bin/sh
set -eu
entry=$(dirname "$0")/driver.py
exec unshare -Urn sh -c 'ip link set lo up && exec python3 "$@"' sh "$entry" "$@"
