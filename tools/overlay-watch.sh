#!/usr/bin/env bash
# Live preview of one channel's overlay, isolated from the rest of the
# station: a looping background fixture, muxed with the real Vello+Rhai
# render, streamed straight into VLC. Edit the channel's overlay (or the
# shared file it references) and save — the running stream picks it up
# without a restart.
#
# Usage:
#   ./tools/overlay-watch.sh 054-dragon-ball
#   TIME_SCALE=30 ./tools/overlay-watch.sh 010-pierce   # compress a 5-minute
#                                                        # cycle into ~10s
#   TITLE="Die Hard" ./tools/overlay-watch.sh 010-pierce
#   BG=station-bumper-12s.mp4 ./tools/overlay-watch.sh 054-dragon-ball
set -u

CHANNEL="${1:?usage: overlay-watch.sh <channel-dir-name, e.g. 054-dragon-ball>}"
CHANNEL_DIR="deploy/appdata/channels/${CHANNEL}"

if [ ! -d "$CHANNEL_DIR" ]; then
  echo "no such channel dir: $CHANNEL_DIR" >&2
  exit 1
fi

# Compressed by default: this is a test loop, not the on-air clock — nobody
# wants to sit through a real 5-minute cycle to see whether a chyron slides
# the right way. Production (`etv-overlay pipe`) never takes this flag.
TIME_SCALE="${TIME_SCALE:-10}"
TITLE="${TITLE:-Preview Movie Title}"
BG="${BG:-bg-checkerboard-20s.mp4}"
BG_PATH="crates/etv-query-test/fixtures/bumpers/${BG}"
VLC="${VLC:-/Applications/VLC.app/Contents/MacOS/vlc}"

if [ ! -f "$BG_PATH" ]; then
  echo "no such background fixture: $BG_PATH" >&2
  exit 1
fi

bold() { printf '\033[1m%s\033[0m' "$1"; }

TMP_DIR="$(mktemp -d -t "overlay-watch-${CHANNEL}")"
SPEC_PATH="${TMP_DIR}/spec.yaml"
cleanup() {
  trap - EXIT INT TERM
  [ -n "${REEXTRACT_PID:-}" ] && kill "$REEXTRACT_PID" 2>/dev/null
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM

printf '%s extracting overlay from %s...\n' "$(bold '==>')" "$CHANNEL_DIR/channel.yaml"
WATCH_TARGET="$(uv run tools/overlay-extract.py "$CHANNEL_DIR" --out "$SPEC_PATH")" || exit 1

printf '%s building etv-overlay...\n' "$(bold '==>')"
cargo build -q -p etv-overlay || exit 1

# Only the inline case needs a re-extraction loop: `channel.yaml` carries the
# spec inline, so an edit there changes the *decl*, not just a value inside
# an already-standalone spec file. A `file:` reference already points
# `etv-overlay watch` straight at the shared spec (see overlay-extract.py) —
# that file's own mtime is what it polls, no relay needed.
if [ "$WATCH_TARGET" = "$(cd "$CHANNEL_DIR" && pwd)/channel.yaml" ]; then
  printf '%s watching %s for edits (inline overlay)\n' "$(bold '==>')" "$WATCH_TARGET"
  (
    LAST_MTIME=""
    while true; do
      MTIME=$(stat -f %m "$WATCH_TARGET" 2>/dev/null)
      if [ "$MTIME" != "$LAST_MTIME" ]; then
        LAST_MTIME="$MTIME"
        uv run tools/overlay-extract.py "$CHANNEL_DIR" --out "$SPEC_PATH" >/dev/null 2>&1
      fi
      sleep 1
    done
  ) &
  REEXTRACT_PID=$!
else
  printf '%s watching %s for edits (shared overlay file)\n' "$(bold '==>')" "$WATCH_TARGET"
  SPEC_PATH="$WATCH_TARGET"
fi

printf '%s streaming (time_scale=%s, title=%s) into VLC — Ctrl-C to stop\n' "$(bold '==>')" "$TIME_SCALE" "$TITLE"
# The built binary directly, not `cargo run` — one less process in the way of
# a signal reaching `watch`'s own SIGINT/SIGTERM handler, which is what
# actually kills its ffmpeg child (see etv-overlay.rs `watch`). Run in the
# foreground (no backgrounding here) so a real terminal's Ctrl-C delivers
# SIGINT to this whole pipeline directly, no trap relay needed.
./target/debug/etv-overlay watch \
  --input "$BG_PATH" \
  --config "$SPEC_PATH" \
  --time-scale "$TIME_SCALE" \
  --title "$TITLE" \
  | "$VLC" -q -
