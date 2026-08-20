#!/bin/bash
# Verify the entrypoint's ffmpeg_progress log split and rotation.
#
# Why this exists: ffmpeg_progress logs once per second per channel. At six
# channels that is ~21,600 lines an hour, and it had the container's Docker log
# buffer down to ~2.8 hours of history even after days of uptime — so the record
# of what led up to a freeze was routinely gone before anyone looked. The
# entrypoint now splits those lines into their own rotated file.
#
# The two things that must hold, and that are easy to get wrong:
#   1. progress lines never reach stdout (the Docker log driver), while every
#      other line still reaches BOTH stdout and station-etv.log, which
#      soak-probe.sh greps.
#   2. rotation is copy-then-truncate, NOT rename. A long-lived awk writer holds
#      the path open; renaming it would leave awk appending to the rolled file
#      forever and the live path would never come back.
#
#   ./tools/progress-split-check.sh
#
# Exit 0 when the split and rotation behave, 1 otherwise.

set -u

W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
prog="$W/ffmpeg-progress.log"
all="$W/station-etv.log"
fails=0

# Mirror the entrypoint's splitter exactly.
splitter() {
    awk -v prog="$prog" -v all="$all" '
        /ffmpeg_progress/ { print >> prog; fflush(prog); next }
        { print >> all; fflush(all); print; fflush() }
    '
}

# Mirror the entrypoint's rotate_progress_log().
PROGRESS_LOG_MAX_BYTES=8000
PROGRESS_LOG_KEEP=2
rotate_progress_log() {
    local f="$prog" size i
    [ -f "$f" ] || return 0
    size=$(stat -c %s "$f" 2>/dev/null || stat -f %z "$f" 2>/dev/null) || return 0
    [ "${size:-0}" -ge "$PROGRESS_LOG_MAX_BYTES" ] || return 0
    for (( i = PROGRESS_LOG_KEEP; i > 1; i-- )); do
        mv -f "$f.$(( i - 1 ))" "$f.$i" 2>/dev/null
    done
    cp -f "$f" "$f.1" 2>/dev/null && : > "$f"
}

# A writer that stays open across rotations, like the real one does.
mkfifo "$W/in"
splitter < "$W/in" > "$W/stdout.txt" &
SPLIT_PID=$!
exec 9> "$W/in"

emit() { printf '%s\n' "$1" >&9; }

for i in $(seq 1 350); do
    emit "[2026-08-20T18:00:00Z DEBUG ffmpeg_progress] channel 13: frame=$i fps=23.99 out_time=00:0$i xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    if [ $(( i % 100 )) -eq 0 ] && [ "$i" -lt 350 ]; then
        emit "[2026-08-20T18:00:00Z ERROR ersatztv_channel] channel 13 terminated after ffmpeg stall"
        sleep 0.2
        rotate_progress_log
    fi
done
sleep 0.5
exec 9>&-
wait "$SPLIT_PID" 2>/dev/null

echo "--- assertions ---"

if grep -q ffmpeg_progress "$W/stdout.txt"; then
    echo "  FAIL ffmpeg_progress leaked to stdout (would still fill the Docker buffer)"; fails=$((fails+1))
else
    echo "  ok   no ffmpeg_progress on stdout"
fi

n=$(grep -c "terminated after ffmpeg stall" "$W/stdout.txt" 2>/dev/null || echo 0)
if [ "$n" -eq 3 ]; then
    echo "  ok   all 3 non-progress lines reached stdout"
else
    echo "  FAIL expected 3 stall lines on stdout, got $n"; fails=$((fails+1))
fi

n=$(grep -c "terminated after ffmpeg stall" "$all" 2>/dev/null || echo 0)
if [ "$n" -eq 3 ]; then
    echo "  ok   non-progress lines also reached station-etv.log"
else
    echo "  FAIL expected 3 in station-etv.log, got $n"; fails=$((fails+1))
fi

# The load-bearing one: the writer must still be feeding the LIVE path after
# rotations, not the rolled file.
tail_frames=$(grep -o 'frame=[0-9]*' "$prog" 2>/dev/null | tail -1)
if [ -n "$tail_frames" ]; then
    last=${tail_frames#frame=}
    if [ "$last" -gt 300 ]; then
        echo "  ok   live log still receiving after rotation (last $tail_frames)"
    else
        echo "  FAIL live log stopped at $tail_frames — writer followed the rolled file"; fails=$((fails+1))
    fi
else
    echo "  FAIL live log empty after rotation — writer lost the path"; fails=$((fails+1))
fi

total=$(cat "$prog" "$prog".1 "$prog".2 2>/dev/null | grep -c ffmpeg_progress || echo 0)
if [ "$total" -ge 350 ]; then
    echo "  ok   $total progress lines retained across rotation (350 emitted)"
else
    echo "  note $total of 350 retained — older lines evicted by keep=$PROGRESS_LOG_KEEP (expected when bounded)"
fi

rolled=$(ls "$prog".* 2>/dev/null | wc -l | tr -d ' ')
if [ "$rolled" -le "$PROGRESS_LOG_KEEP" ]; then
    echo "  ok   kept $rolled rolled file(s), within keep=$PROGRESS_LOG_KEEP"
else
    echo "  FAIL kept $rolled rolled files, expected <= $PROGRESS_LOG_KEEP"; fails=$((fails+1))
fi

echo
[ "$fails" -eq 0 ] && { echo "RESULT: PASS"; exit 0; }
echo "RESULT: $fails assertion(s) FAILED"
exit 1
