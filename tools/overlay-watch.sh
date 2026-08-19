#!/usr/bin/env bash
# Live preview of one channel's overlay, isolated from the rest of the
# station: a looping background fixture, muxed with the real Vello+Rhai
# render, streamed straight into VLC. Edit the channel's overlay (or the
# shared file it references) and save — the running stream picks it up
# without a restart.
#
# Usage:
#   ./tools/overlay-watch.sh 054-dragon-ball                    # bare name -> deploy/appdata/channels/<name>
#   ./tools/overlay-watch.sh deploy/appdata/channels/054-dragon-ball
#   ./tools/overlay-watch.sh examples/channels/diehard.yaml      # dev-side flat channel file
#   TIME_SCALE=30 ./tools/overlay-watch.sh 010-pierce   # compress a 5-minute
#                                                        # cycle into ~10s
#   TITLE="Die Hard" ./tools/overlay-watch.sh 010-pierce
#   BG=station-bumper-12s.mp4 ./tools/overlay-watch.sh 054-dragon-ball
set -u

CHANNEL="${1:?usage: overlay-watch.sh <channel path, or a bare deploy/appdata channel name>}"
# A bare name with no path separator is shorthand for the deploy/appdata
# layout (the common case, typed by hand); anything with a "/" in it — a
# deploy/appdata dir or an examples/channels/*.yaml file — is used as-is.
case "$CHANNEL" in
  */*) CHANNEL_DIR="$CHANNEL" ;;
  *) CHANNEL_DIR="deploy/appdata/channels/${CHANNEL}" ;;
esac

if [ ! -e "$CHANNEL_DIR" ]; then
  echo "no such channel path: $CHANNEL_DIR" >&2
  exit 1
fi
if [ -d "$CHANNEL_DIR" ]; then
  CHANNEL_YAML="$CHANNEL_DIR/channel.yaml"
else
  CHANNEL_YAML="$CHANNEL_DIR"
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

TMP_LABEL="$(printf '%s' "$CHANNEL" | tr -c 'A-Za-z0-9._-' '-')"
TMP_DIR="$(mktemp -d -t "overlay-watch-${TMP_LABEL}")"
SPEC_PATH="${TMP_DIR}/spec.yaml"
cleanup() {
  trap - EXIT INT TERM
  [ -n "${REEXTRACT_PID:-}" ] && kill "$REEXTRACT_PID" 2>/dev/null
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM

printf '%s extracting overlay from %s...\n' "$(bold '==>')" "$CHANNEL_YAML"
WATCH_TARGET="$(uv run tools/overlay-extract.py "$CHANNEL_DIR" --out "$SPEC_PATH")" || exit 1

printf '%s building etv-overlay...\n' "$(bold '==>')"
cargo build -q -p etv-overlay || exit 1

# Only the inline case needs a re-extraction loop: the channel config carries
# the spec inline, so an edit there changes the *decl*, not just a value
# inside an already-standalone spec file. A `file:` reference already points
# `etv-overlay watch` straight at the shared spec (see overlay-extract.py) —
# that file's own mtime is what it polls, no relay needed. Compared by inode
# (`-ef`), not path string, since CHANNEL_YAML may be relative and
# WATCH_TARGET always comes back absolute.
if [ "$WATCH_TARGET" -ef "$CHANNEL_YAML" ]; then
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
