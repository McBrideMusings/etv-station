#!/usr/bin/env bash
# Grab a single video frame from a live ETV-next HLS channel and open it.
#
# Usage:
#   ./tools/frame-grab.sh
#   CHANNEL=2 ./tools/frame-grab.sh
#   CHANNEL=1 BASE_URL=http://localhost:8409 ./tools/frame-grab.sh
set -uo pipefail

CHANNEL="${CHANNEL:-1}"
BASE_URL="${BASE_URL:-http://127.0.0.1:8409}"
OUTPUT="/tmp/etv-frame-ch${CHANNEL}.jpg"
URL="${BASE_URL}/channel/${CHANNEL}.m3u8"

bold()  { printf '\033[1m%s\033[0m' "$1"; }
green() { printf '\033[32m%s\033[0m' "$1"; }
red()   { printf '\033[31m%s\033[0m' "$1"; }

printf '%s grabbing frame from channel %s — %s\n' "$(bold '==>')" "$CHANNEL" "$URL"

if ffmpeg -y -rw_timeout 15000000 -i "$URL" -frames:v 1 -q:v 2 "$OUTPUT" 2>/tmp/etv-frame-grab.log; then
  printf '%s saved to %s\n' "$(green '==>')" "$OUTPUT"
  open "$OUTPUT"
else
  printf '%s ffmpeg failed:\n' "$(red 'FAIL')"
  cat /tmp/etv-frame-grab.log
  exit 1
fi
