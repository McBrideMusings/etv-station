#!/usr/bin/env bash
# Render the Vello watermark overlay onto a station-bumper fixture and open the result.
#
# Usage:
#   ./tools/overlay-test.sh
#   FIXTURE=ident-45s.mp4 ./tools/overlay-test.sh
#   CONFIG=crates/etv-overlay/fixtures/watermark_fade.yaml ./tools/overlay-test.sh
#   OVERLAY_FONTS=path/to/fonts ./tools/overlay-test.sh   # see tools/overlay-fonts.sh
set -euo pipefail

# shellcheck source=tools/overlay-fonts.sh
source "$(dirname "${BASH_SOURCE[0]}")/overlay-fonts.sh"

FIXTURE="${FIXTURE:-station-bumper-12s.mp4}"
INPUT="crates/etv-query-test/fixtures/bumpers/${FIXTURE}"
CONFIG="${CONFIG:-crates/etv-overlay/fixtures/watermark.yaml}"
OUTPUT="${OUTPUT:-/tmp/etv-overlay-test.mp4}"

bold()  { printf '\033[1m%s\033[0m' "$1"; }
green() { printf '\033[32m%s\033[0m' "$1"; }

printf '%s building etv-overlay...\n' "$(bold '==>')"
cargo build -p etv-overlay

printf '%s rendering %s + %s -> %s\n' "$(bold '==>')" "$INPUT" "$CONFIG" "$OUTPUT"
./target/debug/etv-overlay ${FONT_ARGS[@]+"${FONT_ARGS[@]}"} run --input "$INPUT" --config "$CONFIG" --output "$OUTPUT"

printf '%s opening %s\n' "$(green '==>')" "$OUTPUT"
open "$OUTPUT"
