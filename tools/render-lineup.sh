#!/usr/bin/env bash
# Render examples/etv-next/lineup.json from lineup.json.tpl, substituting
# ETV_BIND_ADDRESS and ETV_PORT from the environment.
#
# Defaults bind to 0.0.0.0 so the dev server is reachable as localhost,
# 127.0.0.1, the LAN IP, and Tailscale — without leaking host-specific
# values into committed config.
set -eu

: "${ETV_BIND_ADDRESS:=0.0.0.0}"
: "${ETV_PORT:=8409}"

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
tpl="$repo_root/examples/etv-next/lineup.json.tpl"
out="$repo_root/examples/etv-next/lineup.json"

sed \
  -e "s|\${ETV_BIND_ADDRESS}|$ETV_BIND_ADDRESS|g" \
  -e "s|\${ETV_PORT}|$ETV_PORT|g" \
  "$tpl" > "$out"

echo "[render-lineup] $out (bind=$ETV_BIND_ADDRESS port=$ETV_PORT)"
