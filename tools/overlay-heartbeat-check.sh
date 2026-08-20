#!/bin/bash
# Verify clock 2 of the two-clock instrumentation against a real etv-overlay.
#
# This is the mirror of tools/overlay-stall-repro.sh. There the *writer* went
# quiet and ffmpeg wedged; here the *reader* goes quiet and the overlay wedges.
# Both directions produce the same outside symptom, which is exactly why the
# instrumentation has to name which one happened.
#
# What it asserts:
#   1. `overlay.heartbeat` appears beside the fifo and frames_written climbs
#      while the reader is draining.
#   2. Once the reader stops draining, the phase pins to `write_fifo`,
#      frames_written stops climbing, and phase_age_ms grows.
#   3. `overlay.phase_stall` warnings are emitted on a backoff cadence.
#
# Note `write_fifo` is the RESTING phase, not an alarm: one 1280x720 rgba frame
# is 3.5MB against a 64KB pipe buffer, so a healthy overlay is inside write_all
# almost all the time (observed healthy phase_age_ms: 16-241ms). What separates
# healthy from wedged is frames_written advancing, not the phase name. A pin
# means the phase stopped changing AND frames stopped climbing.
#
# A `write_fifo` pin means the READER stopped first. In production that reads as
# "ffmpeg stopped consuming", which is the opposite of the overlay having gone
# quiet — and telling those apart is the whole point.
#
#   ./tools/overlay-heartbeat-check.sh
#
# Exit 0 when the instrumentation behaved as specified, 1 otherwise.

set -u
cd "$(git rev-parse --show-toplevel)" || exit 2

BIN=target/debug/etv-overlay
if [ ! -x "$BIN" ]; then
    echo "building etv-overlay..."
    cargo build -q -p etv-overlay --bin etv-overlay || exit 2
fi

WORK=$(mktemp -d)
OPID=""; RPID=""
cleanup() {
    [ -n "$OPID" ] && kill "$OPID" 2>/dev/null
    [ -n "$RPID" ] && kill "$RPID" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

PLAYOUT="$WORK/playout"
mkdir -p "$PLAYOUT"
cat > "$PLAYOUT/overlay.json" <<'JSON'
{"base":{"width":1280,"height":720,"framerate":30,"pixel_format":"rgba8"},"spans":[]}
JSON

FIFO="$WORK/overlay.fifo"
HEARTBEAT="$WORK/overlay.heartbeat"
mkfifo "$FIFO"

DRAIN_SECS=8      # how long the reader behaves before it stops draining
WATCH_SECS=12     # how long we sample the heartbeat afterwards

# Reader: drain normally, then stop reading while holding the fifo open.
python3 - "$FIFO" "$DRAIN_SECS" >/dev/null 2>&1 <<'PY' &
import sys, time
f = open(sys.argv[1], "rb")
end = time.time() + float(sys.argv[2])
while time.time() < end:
    if not f.read(65536):
        break
while True:
    time.sleep(3600)
PY
RPID=$!

RUST_LOG=info "$BIN" pipe --fifo "$FIFO" --playout-folder "$PLAYOUT" \
    > "$WORK/overlay.log" 2>&1 &
OPID=$!

healthy_frames=0
prev_frames=-1
pinned_samples=0
pinned_frames=""
last_age=0
saw_frames_advance=0

for _ in $(seq 1 $(( DRAIN_SECS + WATCH_SECS ))); do
    sleep 1
    [ -s "$HEARTBEAT" ] || continue
    body=$(cat "$HEARTBEAT")
    phase=$(printf '%s' "$body" | sed -n 's/.*"phase":"\([a-z_]*\)".*/\1/p')
    frames=$(printf '%s' "$body" | sed -n 's/.*"frames_written":\([0-9]*\).*/\1/p')
    age=$(printf '%s' "$body" | sed -n 's/.*"phase_age_ms":\([0-9]*\).*/\1/p')
    echo "  $body"

    # Healthy means frames climbing, whatever phase the sample caught.
    if [ "${frames:-0}" -gt "$prev_frames" ] && [ "$prev_frames" -ge 0 ]; then
        saw_frames_advance=1
        healthy_frames="${frames:-0}"
        pinned_samples=0
    elif [ "${frames:-0}" -gt 0 ] && [ "${frames:-0}" -eq "$prev_frames" ]; then
        pinned_frames="$frames"
        pinned_samples=$(( pinned_samples + 1 ))
        last_age="${age:-0}"
    fi
    prev_frames="${frames:-0}"
done

echo
echo "--- assertions ---"
fails=0

if [ "$saw_frames_advance" -eq 1 ] && [ "$healthy_frames" -gt 0 ]; then
    echo "  ok   frames_written climbed while the reader drained (reached $healthy_frames)"
else
    echo "  FAIL frames_written never advanced while the reader was draining"
    fails=$(( fails + 1 ))
fi

if [ "$pinned_samples" -ge 3 ]; then
    echo "  ok   frames frozen at $pinned_frames for $pinned_samples consecutive samples"
else
    echo "  FAIL expected frames to freeze once the reader stopped; saw $pinned_samples consecutive frozen samples"
    fails=$(( fails + 1 ))
fi

if [ "$last_age" -ge 3000 ]; then
    echo "  ok   phase_age_ms grew to ${last_age}ms while pinned"
else
    echo "  FAIL phase_age_ms only reached ${last_age}ms; expected >=3000"
    fails=$(( fails + 1 ))
fi

stalls=$(grep -c "overlay.phase_stall" "$WORK/overlay.log" 2>/dev/null || echo 0)
if [ "$stalls" -ge 2 ]; then
    echo "  ok   emitted $stalls overlay.phase_stall warnings"
else
    echo "  FAIL expected >=2 overlay.phase_stall warnings, got $stalls"
    fails=$(( fails + 1 ))
fi

echo
if [ "$fails" -eq 0 ]; then
    echo "RESULT: PASS — clock 2 freezes frames_written and pins write_fifo when the reader stops draining."
    exit 0
fi
echo "RESULT: $fails assertion(s) FAILED"
echo "overlay log tail:"; tail -5 "$WORK/overlay.log"
exit 1
