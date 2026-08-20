#!/bin/bash
# Reproduce the channel freeze as a two-arm experiment on the overlay fifo.
#
# The production symptom (channel 10, 2026-08-20T13:35:17Z): ffmpeg's frame
# counter and out_time stop advancing mid-item with no ffmpeg error at all, and
# ETV-next's stall detector kills the session ~60s later with exit 75.
#
# The hypothesis this tests: the overlay writer stops producing frames but keeps
# the fifo OPEN. ffmpeg's `-i pipe:10` then blocks forever on a read that never
# returns, and because the overlay is a filter-graph input, the whole graph --
# including the main video branch -- stalls behind it. No EOF is ever delivered,
# so eof_action never comes into play and ffmpeg has nothing to report.
#
# Two arms, identical except for what the writer does after WRITE_SECS:
#
#   stall   writer stops writing and holds the fifo open  -> expect out_time to
#           freeze at ~WRITE_SECS and never reach DUR
#   close   writer stops writing and closes the fifo      -> expect EOF, and the
#           main video to run to DUR
#
# The control arm matters: if BOTH arms freeze, the bug is not about holding the
# descriptor open and this hypothesis is wrong.
#
#   ./tools/overlay-stall-repro.sh [--dur N] [--write-secs N] [--arm stall|close|both]
#
# Exit status is 0 when the arms behaved as the hypothesis predicts, 1 otherwise,
# so this is usable as a pass/fail loop.

set -u

DUR=20          # total length of the main video
WRITE_SECS=6    # how long the overlay writer feeds frames before it stops
PAUSE_SECS=10   # resume arm: how long the writer pauses before feeding again
ARM=both
W=1280
H=720
FPS=30

while [ $# -gt 0 ]; do
    case "$1" in
        --dur) DUR="$2"; shift 2 ;;
        --write-secs) WRITE_SECS="$2"; shift 2 ;;
        --arm) ARM="$2"; shift 2 ;;
        --pause-secs) PAUSE_SECS="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# ---------------------------------------------------------------- writer ----
# Mirrors etv-overlay: raw rgba frames at the channel's geometry, straight into
# the fifo. `hold` is the whole experiment -- it stops writing but never closes.
write_frames() {
    local fifo="$1" mode="$2"
    # stdout/stderr must be detached from this function: the pid is captured with
    # a command substitution, which blocks until every writer of the captured
    # stdout closes it. A backgrounded child inheriting stdout would hang that
    # substitution forever in the `hold` arm, which never exits.
    python3 - "$fifo" "$mode" "$W" "$H" "$FPS" "$WRITE_SECS" "$PAUSE_SECS" >/dev/null 2>&1 <<'PY' &
import sys, time
fifo, mode, w, h, fps, secs = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]), float(sys.argv[6])
frame = bytes(w * h * 4)
f = open(fifo, "wb")
n = int(fps * secs)
for i in range(n):
    f.write(frame)
    f.flush()
    time.sleep(1.0 / fps)
if mode == "close":
    f.close()
elif mode == "resume":
    # Pause like a writer that is briefly busy, then start feeding again. If
    # ffmpeg picks up where it left off, a short overlay hiccup is survivable
    # and only a pause longer than the stall detector's window is fatal.
    time.sleep(float(sys.argv[7]))
    while True:
        f.write(frame)
        f.flush()
        time.sleep(1.0 / fps)
else:
    # hold the descriptor open forever, writing nothing.
    while True:
        time.sleep(3600)
PY
    echo $!
}

run_arm() {
    local mode="$1"
    local fifo="$WORK/overlay-$mode.fifo"
    local prog="$WORK/progress-$mode.txt"
    local log="$WORK/ffmpeg-$mode.log"
    rm -f "$fifo"; mkfifo "$fifo"

    local wpid
    wpid=$(write_frames "$fifo" "$mode")

    # Same input shape as the production argv: rawvideo/rgba/1280x720/framerate 30
    # arriving on fd 10, overlaid onto the main video.
    ffmpeg -nostdin -hide_banner -loglevel error \
        -progress "$prog" \
        -f lavfi -i "testsrc=size=${W}x${H}:rate=${FPS}" \
        -f rawvideo -pixel_format rgba -video_size "${W}x${H}" -framerate "$FPS" -i pipe:10 \
        -filter_complex "[0:v]format=yuv420p[v_m];[1:0]format=yuva420p[v_s];[v_m][v_s]overlay=x=0:y=0[v_o]" \
        -map "[v_o]" -t "${DUR}" -c:v libx264 -preset ultrafast -f null - \
        10<"$fifo" >"$log" 2>&1 &
    local fpid=$!

    # Give it DUR plus generous headroom; a healthy run finishes well inside this.
    local deadline=$(( DUR * 3 + 15 ))
    local waited=0
    while kill -0 "$fpid" 2>/dev/null && [ "$waited" -lt "$deadline" ]; do
        sleep 1
        waited=$(( waited + 1 ))
    done

    local hung=0
    if kill -0 "$fpid" 2>/dev/null; then
        hung=1
        kill -9 "$fpid" 2>/dev/null
    fi
    kill -9 "$wpid" 2>/dev/null
    wait "$fpid" 2>/dev/null
    wait "$wpid" 2>/dev/null

    local out_us
    out_us=$(grep '^out_time_us=' "$prog" 2>/dev/null | tail -1 | cut -d= -f2)
    [ -z "${out_us:-}" ] && out_us=0
    local out_s=$(( out_us / 1000000 ))
    local frames
    frames=$(grep '^frame=' "$prog" 2>/dev/null | tail -1 | cut -d= -f2 | tr -d ' ')
    [ -z "${frames:-}" ] && frames=0

    echo "arm=$mode hung=$hung out_time_s=$out_s frames=$frames wall_s=$waited (writer fed ${WRITE_SECS}s, target ${DUR}s)"
    if [ -s "$log" ]; then
        echo "  ffmpeg stderr: $(head -3 "$log" | tr '\n' ' ')"
    else
        echo "  ffmpeg stderr: <empty>"
    fi

    ARM_HUNG=$hung
    ARM_OUT=$out_s
}

echo "=== overlay fifo stall repro (${W}x${H}@${FPS}, main video ${DUR}s, overlay stops at ${WRITE_SECS}s) ==="

stall_hung=-1; stall_out=-1; close_hung=-1; close_out=-1
if [ "$ARM" = both ] || [ "$ARM" = stall ]; then
    run_arm stall; stall_hung=$ARM_HUNG; stall_out=$ARM_OUT
fi
if [ "$ARM" = both ] || [ "$ARM" = close ]; then
    run_arm close; close_hung=$ARM_HUNG; close_out=$ARM_OUT
fi
if [ "$ARM" = both ] || [ "$ARM" = resume ]; then
    run_arm resume
    echo "  (resume arm paused ${PAUSE_SECS}s mid-stream; reaching ${DUR}s means a short overlay hiccup is survivable)"
fi

echo
if [ "$ARM" = both ]; then
    # Predicted: stall arm wedges near WRITE_SECS, close arm reaches DUR.
    if [ "$stall_hung" -eq 1 ] && [ "$stall_out" -lt "$DUR" ] \
       && [ "$close_hung" -eq 0 ] && [ "$close_out" -ge $(( DUR - 2 )) ]; then
        echo "RESULT: REPRODUCED — holding the fifo open wedges the whole graph; closing it does not."
        exit 0
    fi
    echo "RESULT: NOT reproduced as predicted (stall_hung=$stall_hung stall_out=$stall_out close_hung=$close_hung close_out=$close_out)"
    exit 1
fi
exit 0
