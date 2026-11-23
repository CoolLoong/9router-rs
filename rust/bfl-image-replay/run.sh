#!/bin/sh
set -eu
entry=$(dirname "$0")/driver.py
exec unshare -Urn sh -c 'ip link set lo up && ip addr add 93.184.216.34/32 dev lo && exec python3 "$@"' sh "$entry" "$@"
