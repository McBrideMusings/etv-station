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
#   INTERVAL=4 ./tools/overlay-watch.sh 010-pierce       # cycle even tighter
#   INTERVAL= ./tools/overlay-watch.sh 010-pierce        # the real on-air gap
#   TITLE="Die Hard" ./tools/overlay-watch.sh 010-pierce
#   BG=bg-checkerboard-20s.mp4 ./tools/overlay-watch.sh 036-the-academy  # alpha check
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

# Real speed. The animation itself — a slide, a fade — is authored in seconds
# and should be judged at the speed it will air at; what makes a preview
# unwatchable is the multi-minute *gap between* cycles, and INTERVAL below
# shortens that directly instead. Raising TIME_SCALE compresses the animation
# and the gap together, so a legible 8-second hold becomes an 0.8-second
# flash. Production (`etv-overlay pipe`) never takes this flag.
TIME_SCALE="${TIME_SCALE:-1}"
# Overrides `config.interval_secs` — the key both shipped animated scripts use
# for "seconds from one cycle's start to the next" (title-chyron.rhai,
# now-next-snipe.rhai). On air it is 300; at 12 the graphic is on screen
# almost continuously, which is what makes it possible to actually look at.
# INTERVAL= (empty) leaves the channel's own value alone.
INTERVAL="${INTERVAL-12}"
TITLE="${TITLE:-Preview Movie Title}"
# Solid black, so white text reads. The checkerboard is still here as
# `BG=bg-checkerboard-20s.mp4` — it exists to prove the overlay's alpha is
# actually transparent, which is a different question from whether the
# graphic is legible, and it is terrible at the second job.
BG="${BG:-bg-black-20s.mp4}"
# Stand-in program metadata for a script that formats differently per item
# kind (now-next-snipe.rhai). The defaults describe two films, because -1 is
# "absent" and an absent season is exactly what marks an item as not an
# episode. To eyeball the series form instead:
#   SEASON=2 EPISODE=5 NEXT_SEASON=2 NEXT_EPISODE=6 admin overlay-watch <ch>
NEXT_TITLE="${NEXT_TITLE:-The Next Preview Feature}"
SEASON="${SEASON:--1}"
EPISODE="${EPISODE:--1}"
YEAR="${YEAR:-1988}"
NEXT_SEASON="${NEXT_SEASON:--1}"
NEXT_EPISODE="${NEXT_EPISODE:--1}"
NEXT_YEAR="${NEXT_YEAR:-2019}"
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

EXTRACT_ARGS=()
if [ -n "$INTERVAL" ]; then
  EXTRACT_ARGS+=(--set-config "interval_secs=$INTERVAL")
fi

printf '%s extracting overlay from %s...\n' "$(bold '==>')" "$CHANNEL_YAML"
WATCH_TARGET="$(uv run tools/overlay-extract.py "$CHANNEL_DIR" --out "$SPEC_PATH" ${EXTRACT_ARGS[@]+"${EXTRACT_ARGS[@]}"})" || exit 1

printf '%s building etv-overlay...\n' "$(bold '==>')"
cargo build -q -p etv-overlay || exit 1

# Always relay through a re-extracted copy, whether the channel carries its
# overlay inline or points at a shared file with `file:`. Handing
# `etv-overlay watch` the shared file directly used to be a shortcut for the
# `file:` case, but it cannot coexist with --set-config: the retimed spec is
# not what is on disk, so streaming the original would silently ignore the
# override. WATCH_TARGET is whichever file a human actually edits; SPEC_PATH
# is the derived copy the renderer polls.
printf '%s watching %s for edits\n' "$(bold '==>')" "$WATCH_TARGET"
(
  LAST_MTIME=""
  while true; do
    MTIME=$(stat -f %m "$WATCH_TARGET" 2>/dev/null)
    if [ "$MTIME" != "$LAST_MTIME" ]; then
      LAST_MTIME="$MTIME"
      uv run tools/overlay-extract.py "$CHANNEL_DIR" --out "$SPEC_PATH" \
        ${EXTRACT_ARGS[@]+"${EXTRACT_ARGS[@]}"} >/dev/null 2>&1
    fi
    sleep 1
  done
) &
REEXTRACT_PID=$!

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
  --season "$SEASON" \
  --episode "$EPISODE" \
  --year "$YEAR" \
  --next-title "$NEXT_TITLE" \
  --next-season "$NEXT_SEASON" \
  --next-episode "$NEXT_EPISODE" \
  --next-year "$NEXT_YEAR" \
  | "$VLC" -q -
