#!/bin/sh
set -eu
entry=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/driver.py
exec unshare -Urnm sh -c '
  ip link set lo up
  files=$(mktemp -d)
  trap '\''rm -f "$files/hosts" "$files/nsswitch.conf"; rmdir "$files" 2>/dev/null || : '\'' EXIT
  printf "%s\n" "127.0.0.1 localhost api.cline.bot api.github.com api.deepseek.com api.synthetic.new" > "$files/hosts"
  printf "%s\n" "hosts: files" > "$files/nsswitch.conf"
  mount --bind "$files/hosts" /etc/hosts
  mount --bind "$files/nsswitch.conf" /etc/nsswitch.conf
  rm "$files/hosts" "$files/nsswitch.conf"
  rmdir "$files"
  exec python3 "$@"
' sh "$entry" "$@"
