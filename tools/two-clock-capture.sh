#!/bin/bash
# Catch a channel freeze in the act and record which side stopped first.
#
# The freeze signature is symmetric from outside: ffmpeg's out_time stops
# advancing and nothing is logged. "The overlay stopped writing, starving
# ffmpeg" and "ffmpeg stopped reading, blocking the overlay" look identical.
# This reads both clocks at the same instant and records the one fact that
# separates them.
#
# Clock 1 -- ffmpeg: the newest HLS segment index for the channel. It advances
#            once per hls_time (4s) while the transcode is healthy.
# Clock 2 -- overlay: `overlay.heartbeat`, written every second by the phase
#            watchdog in crates/etv-overlay/src/phase_watchdog.rs, naming the
#            phase the frame loop is in and how long it has been there.
#
# The discriminator is the kernel wait channel of each process, sampled while
# both are wedged:
#
#   overlay in pipe_write, frames FROZEN               -> ffmpeg stopped first;
#                                                        the overlay is blocked
#                                                        writing to a reader that
#                                                        is not draining.
#
# `pipe_write` on its own proves nothing: one 1280x720 rgba frame is 3.5MB
# against a 64KB pipe buffer, so a HEALTHY overlay is inside write_all almost
# all the time. Measured healthy phase_age_ms is 16-241ms. The evidence is
# frames_written standing still across two heartbeat samples, not the wchan.
#   ffmpeg  in pipe_read   + overlay elsewhere        -> the overlay stopped
#                                                        first; ffmpeg is starving
#                                                        on the fifo. The overlay
#                                                        heartbeat names the phase.
#   both in pipe_*                                    -> record it; that is a
#                                                        genuine mutual block and
#                                                        the interesting case.
#
# Runs ON the Unraid host (needs /proc for both processes and the appdata mount).
#
#   ./two-clock-capture.sh <etv-next-channel-id> [--gap N] [--rounds N]
#
# Channel id is ETV-next's, not the station's folder number: 032-action is 10.
# Find it in `curl -s localhost:$ETV_PORT_HOST/channels.m3u`.

set -u

SELF_TEST=0
[ "${1:-}" = "--self-test" ] && SELF_TEST=1

CHANNEL="${1:-}"
if [ "$SELF_TEST" -eq 0 ] && { [ -z "$CHANNEL" ] || [ "${CHANNEL#-}" != "$CHANNEL" ]; }; then
    echo "usage: $0 <etv-next-channel-id> [--gap N] [--rounds N]" >&2
    echo "       $0 --self-test    # exercise the verdict logic, no host needed" >&2
    exit 2
fi
shift

GAP_TRIGGER=12      # seconds without a new segment before we call it a freeze
ROUNDS=0            # 0 = run until killed
POLL=1
CAPTURE_EVERY=5     # re-sample this often while still inside a freeze

while [ $# -gt 0 ]; do
    case "$1" in
        --gap) GAP_TRIGGER="$2"; shift 2 ;;
        --rounds) ROUNDS="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

[ "$SELF_TEST" -eq 1 ] && APPDATA="${ETV_APPDATA:-/tmp}"
APPDATA="${APPDATA:-${ETV_APPDATA:-/mnt/user/appdata/etv-station}}"
HLS="$APPDATA/data/hls/$CHANNEL"
DIAG="$APPDATA/data/diag"
LOG="$DIAG/two-clock-$CHANNEL.log"
CONTAINER="${ETV_CONTAINER:-etv-station}"

mkdir -p "$DIAG"

log() { printf '%s %s\n' "$(date -u +%FT%T.%3NZ)" "$*" >> "$LOG"; }

# ------------------------------------------------------------------ procs ----
# Both processes live inside the container, but their /proc entries are visible
# from the host under the host pid namespace, which is what lets us read wchan
# while they are blocked. Match on the fifo/output path so we get THIS channel.
find_ffmpeg_pid() {
    pgrep -f "hls/$CHANNEL" 2>/dev/null | while read -r p; do
        tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null | grep -q '^ffmpeg' && echo "$p"
    done | head -1
}

find_overlay_pid() {
    pgrep -f "etv-overlay" 2>/dev/null | while read -r p; do
        tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null | grep -q "playout/.*/overlay.fifo" && {
            # The overlay is named by station folder, not channel id, so resolve
            # it through the fifo path recorded in the ffmpeg argv.
            echo "$p"
        }
    done
}

# The station folder this channel's ffmpeg is actually reading its overlay from.
overlay_pid_for_channel() {
    local fpid="$1"
    local fifo
    fifo=$(tr '\0' '\n' < "/proc/$fpid/cmdline" 2>/dev/null | grep -m1 'overlay.fifo')
    [ -z "$fifo" ] && { find_overlay_pid | head -1; return; }
    local dir
    dir=$(dirname "$fifo")
    find_overlay_pid | while read -r p; do
        tr '\0' '\n' < "/proc/$p/cmdline" 2>/dev/null | grep -q "^$dir/overlay.fifo$" && echo "$p"
    done | head -1
}

newest_segment_index() {
    ls "$HLS" 2>/dev/null | grep -oE '^live[0-9]+\.ts$' | sed 's/[^0-9]//g' \
        | sed 's/^0*//' | sort -n | tail -1
}

proc_field() {
    local pid="$1" file="$2"
    cat "/proc/$pid/$file" 2>/dev/null | head -1
}

# ---------------------------------------------------------------- verdict ----
# Pure: kernel wait channel of each process in, one verdict line out. Kept
# separate from capture() so it can be exercised without a freeze, a container,
# or a Linux /proc -- see --self-test.
# verdict <overlay_wchan> <ffmpeg_wchan> <frames_advanced:yes|no|unknown>
verdict() {
    local ow="$1" fw="$2" advanced="${3:-unknown}"

    # The overlay is still doing its job if it put frames in the pipe between
    # samples, whatever wchan caught it in. That rules it out as the stalled
    # side before wchan is even consulted.
    if [ "$advanced" = "yes" ]; then
        case "$fw" in
            *pipe_read*) echo "VERDICT overlay-alive-ffmpeg-starving (overlay still writing frames, ffmpeg blocked in pipe_read -- ffmpeg is not consuming what it is given)"; return ;;
            *)           echo "VERDICT overlay-alive (frames still advancing; the freeze is downstream of the overlay, ffmpeg wchan='$fw')"; return ;;
        esac
    fi

    if [ "$advanced" = "no" ]; then
        case "$ow" in
            *pipe_write*)
                case "$fw" in
                    *pipe_read*) echo "VERDICT mutual-pipe-block (overlay wedged in pipe_write AND ffmpeg in pipe_read) -- record this, it is the interesting case"; return ;;
                    *)           echo "VERDICT ffmpeg-stopped-first (overlay frames frozen while blocked in pipe_write; ffmpeg is not draining the fifo)"; return ;;
                esac ;;
            *)
                echo "VERDICT overlay-stopped-first (overlay frames frozen in wchan='$ow', not on the pipe; the heartbeat phase above names the phase it is stuck in)"; return ;;
        esac
    fi

    echo "VERDICT inconclusive (overlay wchan='$ow' ffmpeg wchan='$fw' frames_advanced=$advanced)"
}

self_test() {
    local fails=0
    check() {
        local got want desc
        got=$(verdict "$1" "$2" "$3"); want="$4"; desc="$5"
        case "$got" in
            "VERDICT $want"*) echo "  ok   $desc" ;;
            *) echo "  FAIL $desc"; echo "       want: $want"; echo "       got : $got"; fails=$(( fails + 1 )) ;;
        esac
    }
    echo "verdict self-test:"
    # The load-bearing case: pipe_write while frames STILL ADVANCE is healthy,
    # and must never be reported as a stall.
    check "pipe_write" "do_sys_poll" "yes"  "overlay-alive"                  "healthy overlay resting in pipe_write"
    check "pipe_write" "pipe_read"   "yes"  "overlay-alive-ffmpeg-starving"  "overlay feeding, ffmpeg not consuming"
    check "pipe_write" "do_sys_poll" "no"   "ffmpeg-stopped-first"           "overlay frames frozen in pipe_write"
    check "pipe_write" "pipe_read"   "no"   "mutual-pipe-block"              "both wedged on the fifo"
    check "futex_wait" "pipe_read"   "no"   "overlay-stopped-first"          "overlay frozen off-pipe, ffmpeg starving"
    check "do_sys_poll" "pipe_read"  "no"   "overlay-stopped-first"          "overlay frozen polling, ffmpeg starving"
    check "futex_wait" "futex_wait"  "unknown" "inconclusive"                "neither on a pipe, no frame delta"
    check "" ""                      "unknown" "inconclusive"                "no processes found"
    if [ "$fails" -eq 0 ]; then echo "verdict self-test: PASS"; return 0; fi
    echo "verdict self-test: $fails FAILED"; return 1
}

# ---------------------------------------------------------------- capture ----
capture() {
    local why="$1"
    local fpid opid
    fpid=$(find_ffmpeg_pid)
    opid=""
    [ -n "$fpid" ] && opid=$(overlay_pid_for_channel "$fpid")

    log "==== CAPTURE ($why) channel=$CHANNEL ffmpeg_pid=${fpid:-none} overlay_pid=${opid:-none}"

    # Clock 1 -- ffmpeg
    log "  clock1.ffmpeg newest_segment=$(newest_segment_index) hls_dir=$HLS"
    if [ -n "$fpid" ]; then
        log "  clock1.ffmpeg wchan=$(proc_field "$fpid" wchan) state=$(awk '{print $3}' "/proc/$fpid/stat" 2>/dev/null) syscall=$(proc_field "$fpid" syscall)"
        log "  clock1.ffmpeg utime_ticks=$(awk '{print $14}' "/proc/$fpid/stat" 2>/dev/null)"
    else
        log "  clock1.ffmpeg NO PROCESS -- session already torn down"
    fi

    # Clock 2 -- overlay. Sampled TWICE so we get a frames_written delta: a
    # single sample cannot tell a healthy overlay resting in write_all from one
    # wedged there, because both look identical in wchan.
    local advanced=unknown
    if [ -n "$opid" ]; then
        local fifo hb hb1 hb2 f1 f2
        fifo=$(tr '\0' '\n' < "/proc/$opid/cmdline" 2>/dev/null | grep -m1 'overlay.fifo')
        hb="$(dirname "$fifo")/overlay.heartbeat"
        hb1=$(cat "$hb" 2>/dev/null)
        log "  clock2.overlay wchan=$(proc_field "$opid" wchan) state=$(awk '{print $3}' "/proc/$opid/stat" 2>/dev/null) syscall=$(proc_field "$opid" syscall)"
        log "  clock2.overlay heartbeat.t0=${hb1:-<missing -- build predates the phase watchdog>}"
        sleep 2
        hb2=$(cat "$hb" 2>/dev/null)
        log "  clock2.overlay heartbeat.t1=${hb2:-<missing>}"
        f1=$(printf '%s' "$hb1" | sed -n 's/.*"frames_written":\([0-9]*\).*/\1/p')
        f2=$(printf '%s' "$hb2" | sed -n 's/.*"frames_written":\([0-9]*\).*/\1/p')
        if [ -n "$f1" ] && [ -n "$f2" ]; then
            if [ "$f2" -gt "$f1" ]; then advanced=yes; else advanced=no; fi
            log "  clock2.overlay frames_written $f1 -> $f2 over 2s (advanced=$advanced)"
        else
            log "  clock2.overlay frames_written unreadable; verdict will be inconclusive"
        fi
        log "  clock2.overlay utime_ticks=$(awk '{print $14}' "/proc/$opid/stat" 2>/dev/null)"
    else
        log "  clock2.overlay NO PROCESS"
    fi

    # ---- the verdict ----
    local ow fw
    ow=$(proc_field "${opid:-0}" wchan)
    fw=$(proc_field "${fpid:-0}" wchan)
    log "  $(verdict "$ow" "$fw" "$advanced")"

    # Recent container log for this channel, for the timeline around the freeze.
    docker logs --since 3m "$CONTAINER" 2>&1 \
        | grep -E "channel $CHANNEL |overlay.phase_stall" \
        | grep -v ffmpeg_progress | tail -15 \
        | sed 's/^/  ctx /' >> "$LOG" 2>/dev/null
}

if [ "$SELF_TEST" -eq 1 ]; then self_test; exit $?; fi

log "two-clock capture armed: channel=$CHANNEL gap_trigger=${GAP_TRIGGER}s log=$LOG"
echo "armed; watching channel $CHANNEL -- results land in $LOG"

last_idx=$(newest_segment_index)
last_change=$(date +%s)
in_gap=0
rounds_done=0
last_capture=0

while true; do
    sleep "$POLL"
    idx=$(newest_segment_index)
    now=$(date +%s)

    if [ "${idx:-}" != "${last_idx:-}" ]; then
        if [ "$in_gap" -eq 1 ]; then
            log "RECOVERED after $(( now - last_change ))s; segment index $last_idx -> $idx"
            in_gap=0
            rounds_done=$(( rounds_done + 1 ))
            [ "$ROUNDS" -gt 0 ] && [ "$rounds_done" -ge "$ROUNDS" ] && { log "done"; exit 0; }
        fi
        last_idx="$idx"
        last_change="$now"
        continue
    fi

    gap=$(( now - last_change ))
    if [ "$gap" -ge "$GAP_TRIGGER" ]; then
        if [ "$in_gap" -eq 0 ]; then
            in_gap=1
            last_capture="$now"
            capture "gap=${gap}s first"
        elif [ $(( now - last_capture )) -ge "$CAPTURE_EVERY" ]; then
            last_capture="$now"
            capture "gap=${gap}s ongoing"
        fi
    fi
done
