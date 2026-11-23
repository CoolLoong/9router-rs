#!/bin/sh
set -eu
fixture=$(dirname "$0")/relay-ipv6-runtime-contract.mjs
scratch=$(mktemp -d -t 9router-ipv6-relay.XXXXXXXX)
trap 'rm -rf -- "$scratch"' EXIT HUP INT TERM
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=9Router-local-relay-fixture \
  -addext 'subjectAltName=DNS:*.ipv6-fixture.dev' -keyout "$scratch/key.pem" -out "$scratch/cert.pem" >/dev/null 2>&1
unshare -Urn sh -c '
  set -eu
  ip link set lo up
  ip addr add 93.184.216.34/32 dev lo
  ip -6 addr add 2606:4700:ffff::17/128 dev lo nodad
  NODE_EXTRA_CA_CERTS="$2/cert.pem" node "$1" "$2"
' sh "$fixture" "$scratch"
