#!/usr/bin/env bash
# Render a single Vello overlay frame as a PNG and open it.
#
# Usage:
#   ./tools/overlay-still.sh
#   CONFIG=crates/etv-overlay/fixtures/watermark_fade.yaml TIME=5 ./tools/overlay-still.sh
#   OVERLAY_FONTS=path/to/fonts ./tools/overlay-still.sh   # see tools/overlay-fonts.sh
set -euo pipefail

# shellcheck source=tools/overlay-fonts.sh
source "$(dirname "${BASH_SOURCE[0]}")/overlay-fonts.sh"

CONFIG="${CONFIG:-crates/etv-overlay/fixtures/watermark.yaml}"
TIME="${TIME:-0}"
OUTPUT="${OUTPUT:-/tmp/etv-overlay-still.png}"

bold()  { printf '\033[1m%s\033[0m' "$1"; }
green() { printf '\033[32m%s\033[0m' "$1"; }

printf '%s building etv-overlay...\n' "$(bold '==>')"
cargo build -p etv-overlay

printf '%s rendering still (t=%s) -> %s\n' "$(bold '==>')" "$TIME" "$OUTPUT"
./target/debug/etv-overlay ${FONT_ARGS[@]+"${FONT_ARGS[@]}"} render-still --config "$CONFIG" --output "$OUTPUT" --time "$TIME"

printf '%s opening %s\n' "$(green '==>')" "$OUTPUT"
open "$OUTPUT"
