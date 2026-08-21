#!/bin/bash
# Record which side stopped first when a channel freezes.
#
# The freeze signature is symmetric from outside: ffmpeg's out_time stops
# advancing and nothing is logged. "The overlay stopped writing, starving
# ffmpeg" and "ffmpeg stopped reading, blocking the overlay" look identical.
# This reads both clocks at the same instant and records the fact that
# separates them.
#
# Clock 1 -- ffmpeg: the newest HLS segment index for the channel. It advances
#            once per hls_time (4s) while the transcode is healthy.
# Clock 2 -- overlay: `overlay.heartbeat`, written every second by the phase
#            watchdog in crates/etv-overlay/src/phase_watchdog.rs, naming the
#            phase the frame loop is in and how long it has been there.
#
# The discriminator is each process's kernel wait channel, sampled together
# with a frames_written delta:
#
#   overlay in pipe_write, frames FROZEN -> ffmpeg stopped first; the overlay is
#                                           blocked writing to a reader that is
#                                           not draining.
#   ffmpeg in pipe_read, overlay feeding -> ffmpeg is not consuming what it is
#                                           given.
#   frames still advancing               -> the overlay is not the stalled side;
#                                           look downstream (input file reads).
#
# `pipe_write` on its own proves nothing: one 1280x720 rgba frame is 3.5MB
# against a 64KB pipe buffer, so a HEALTHY overlay is inside write_all almost
# all the time. Measured healthy phase_age_ms is 16-241ms. The evidence is
# frames_written standing still across two samples, not the wchan.
#
# RUNS INSIDE THE CONTAINER, started by docker/entrypoint.sh. It watches every
# channel that has an HLS folder, so there is nothing to arm and nothing to
# re-arm: it comes back with the container, exactly like stream-access-log.py
# and stream-watch.py beside it. An earlier version ran on the Unraid host and
# had to be started by hand over ssh -- the same arrangement the comment above
# those two records having already thrown away, because it died at every
# restart and was off whenever something finally went wrong.
#
#   two-clock-capture.sh              # watch every channel, forever
#   two-clock-capture.sh --self-test  # exercise the verdict logic, no host needed
#
# Env: ETV_HLS_OUTPUT (default /data/hls), ETV_DIAG_DIR (default /data/diag),
#      ETV_TWO_CLOCK_GAP (default 12 seconds).

set -u

HLS_ROOT="${ETV_HLS_OUTPUT:-/data/hls}"
DIAG="${ETV_DIAG_DIR:-/data/diag}"
GAP_TRIGGER="${ETV_TWO_CLOCK_GAP:-12}"
POLL=1
CAPTURE_EVERY=30      # re-sample this often while a channel stays frozen
LOG="$DIAG/two-clock.log"

log() { printf '%s %s\n' "$(date -u +%FT%T.%3NZ)" "$*" >> "$LOG"; }

# ---------------------------------------------------------------- verdict ----
# Pure: wait channels + whether the overlay produced frames between samples.
# Kept separate from capture() so it can be exercised without a freeze, a
# container, or a Linux /proc -- see --self-test.
# verdict <overlay_wchan> <ffmpeg_wchan> <frames_advanced> [has_overlay] [cpu_advanced]
verdict() {
    local ow="$1" fw="$2" advanced="${3:-unknown}" has_overlay="${4:-yes}" cpu="${5:-unknown}"

    # A pipeline with no overlay input cannot be starved by the overlay, so the
    # two-clock question does not apply and "inconclusive" buries the finding.
    # Not hypothetical: on 2026-08-21 channels 1 and 4 stalled repeatedly with no
    # overlay in the graph at all, wedged in futex_wait_queue rather than on any
    # pipe, and all 145 captures read as inconclusive.
    if [ "$has_overlay" = "no" ]; then
        local spin="wedged"
        [ "$cpu" = "yes" ] && spin="burning CPU"
        case "$fw" in
            *pipe_read*|*pipe_write*)
                echo "VERDICT no-overlay-but-on-a-pipe ($spin in '$fw' with no overlay input -- some OTHER pipe in the graph is blocked)"; return ;;
            *futex*)
                echo "VERDICT no-overlay-ffmpeg-futex ($spin in '$fw'; no overlay in the pipeline, so the overlay cannot be the cause -- an internal ffmpeg lock or thread wait)"; return ;;
            "")
                echo "VERDICT no-overlay-ffmpeg-gone (no overlay input and no ffmpeg process; the session was already torn down)"; return ;;
            *)
                echo "VERDICT no-overlay-ffmpeg-elsewhere ($spin in '$fw'; no overlay in the pipeline, so look at the input read or the output write)"; return ;;
        esac
    fi

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
    check_no() {
        local got want desc
        got=$(verdict "" "$1" "unknown" "no" "unknown"); want="$2"; desc="$3"
        case "$got" in
            "VERDICT $want"*) echo "  ok   $desc" ;;
            *) echo "  FAIL $desc"; echo "       want: $want"; echo "       got : $got"; fails=$(( fails + 1 )) ;;
        esac
    }
    echo "verdict self-test:"
    # The load-bearing case: pipe_write while frames STILL ADVANCE is healthy,
    # and must never be reported as a stall.
    check "pipe_write"  "do_sys_poll" "yes"     "overlay-alive"                  "healthy overlay resting in pipe_write"
    check "pipe_write"  "pipe_read"   "yes"     "overlay-alive-ffmpeg-starving"  "overlay feeding, ffmpeg not consuming"
    check "pipe_write"  "do_sys_poll" "no"      "ffmpeg-stopped-first"           "overlay frames frozen in pipe_write"
    check "pipe_write"  "pipe_read"   "no"      "mutual-pipe-block"              "both wedged on the fifo"
    check "futex_wait"  "pipe_read"   "no"      "overlay-stopped-first"          "overlay frozen off-pipe, ffmpeg starving"
    check "do_sys_poll" "pipe_read"   "no"      "overlay-stopped-first"          "overlay frozen polling, ffmpeg starving"
    check "futex_wait"  "futex_wait"  "unknown" "inconclusive"                   "neither on a pipe, no frame delta"
    check ""            ""            "unknown" "inconclusive"                   "no processes found"
    # No overlay in the pipeline: the two-clock question does not apply, and the
    # answer must name what ffmpeg is actually stuck on instead of "inconclusive".
    check_no "futex_wait_queue" "no-overlay-ffmpeg-futex"     "no overlay, ffmpeg in futex"
    check_no "pipe_read"        "no-overlay-but-on-a-pipe"    "no overlay, but blocked on some other pipe"
    check_no ""                 "no-overlay-ffmpeg-gone"      "no overlay, ffmpeg already gone"
    check_no "do_sys_poll"      "no-overlay-ffmpeg-elsewhere" "no overlay, ffmpeg blocked elsewhere"
    if [ "$fails" -eq 0 ]; then echo "verdict self-test: PASS"; return 0; fi
    echo "verdict self-test: $fails FAILED"; return 1
}

[ "${1:-}" = "--self-test" ] && { self_test; exit $?; }

mkdir -p "$DIAG" 2>/dev/null

# ------------------------------------------------------------------ procs ----
# Everything here is a sibling process in this container, so /proc is directly
# readable -- no docker exec, no ssh, no host pid namespace.
# Scan /proc ONCE per tick and map channel -> ffmpeg pid. Doing this per channel
# instead would re-walk every process for every channel every second, which on a
# lineup with dozens of channels is a lot of work to answer one question.
declare -A FFMPEG_PID
scan_ffmpeg_pids() {
    local p args ch
    FFMPEG_PID=()
    for p in /proc/[0-9]*; do
        [ -r "$p/cmdline" ] || continue
        # Brace-wrap the redirection: a process can exit between the glob and
        # this read, and `2>/dev/null` on `tr` alone does not suppress the
        # shell's own "No such file or directory" for a failed `<`.
        args=$({ tr '\0' ' ' < "$p/cmdline"; } 2>/dev/null) || continue
        case "$args" in
            ffmpeg\ *"$HLS_ROOT/"*)
                ch=${args##*"$HLS_ROOT/"}
                ch=${ch%%/*}
                case "$ch" in
                    ''|*[!0-9]*) continue ;;
                    *) FFMPEG_PID[$ch]=$(basename "$p") ;;
                esac ;;
        esac
    done
}

ffmpeg_pid_for() { printf '%s' "${FFMPEG_PID[$1]:-}"; }

# The overlay fifo this channel's ffmpeg is reading from, resolved through the
# argv rather than assumed: the overlay is named by station folder while the
# channel is named by id.
overlay_fifo_for() {
    local fpid="$1"
    { tr '\0' '\n' < "/proc/$fpid/cmdline"; } 2>/dev/null | grep -m1 'overlay\.fifo'
}

overlay_pid_for_fifo() {
    local fifo="$1" p args
    [ -n "$fifo" ] || return
    for p in /proc/[0-9]*; do
        [ -r "$p/cmdline" ] || continue
        args=$({ tr '\0' '\n' < "$p/cmdline"; } 2>/dev/null) || continue
        case "$args" in
            *etv-overlay*) printf '%s\n' "$args" | grep -qxF "$fifo" && { basename "$p"; return; } ;;
        esac
    done
}

newest_segment_index() {
    local ch="$1"
    ls "$HLS_ROOT/$ch" 2>/dev/null | sed -n 's/^live0*\([0-9][0-9]*\)\.ts$/\1/p' \
        | sort -n | tail -1
}

proc_field() {
    local pid="$1" file="$2"
    [ -n "$pid" ] || return
    head -1 "/proc/$pid/$file" 2>/dev/null
}

frames_from() { printf '%s' "$1" | sed -n 's/.*"frames_written":\([0-9]*\).*/\1/p'; }

# ---------------------------------------------------------------- capture ----
capture() {
    local ch="$1" why="$2"
    local fpid opid fifo hb hb1 hb2 f1 f2 advanced=unknown ow fw

    fpid=$(ffmpeg_pid_for "$ch")
    fifo=""; opid=""
    if [ -n "$fpid" ]; then
        fifo=$(overlay_fifo_for "$fpid")
        opid=$(overlay_pid_for_fifo "$fifo")
    fi

    log "==== CAPTURE ($why) channel=$ch ffmpeg_pid=${fpid:-none} overlay_pid=${opid:-none} overlay_in_pipeline=$( [ -n "$fifo" ] && echo yes || echo no )"
    log "  clock1.ffmpeg newest_segment=$(newest_segment_index "$ch") dir=$HLS_ROOT/$ch"
    local cpu=unknown u1 u2
    if [ -n "$fpid" ]; then
        u1=$(awk '{print $14+$15}' "/proc/$fpid/stat" 2>/dev/null)
        log "  clock1.ffmpeg wchan=$(proc_field "$fpid" wchan) state=$(awk '{print $3}' "/proc/$fpid/stat" 2>/dev/null) syscall=$(proc_field "$fpid" syscall)"
        # Which THREAD is stuck matters for a futex wedge: one blocked worker
        # with the rest idle reads differently from every thread parked.
        log "  clock1.ffmpeg threads=$( { for t in /proc/$fpid/task/[0-9]*; do head -1 "$t/wchan" 2>/dev/null; echo; done; } 2>/dev/null | sort | uniq -c | tr '\n' ' ' )"
    else
        log "  clock1.ffmpeg NO PROCESS -- session already torn down"
    fi

    # Clock 2 is sampled TWICE: a single sample cannot tell a healthy overlay
    # resting in write_all from one wedged there.
    if [ -n "$opid" ] && [ -n "$fifo" ]; then
        hb="$(dirname "$fifo")/overlay.heartbeat"
        hb1=$(cat "$hb" 2>/dev/null)
        log "  clock2.overlay wchan=$(proc_field "$opid" wchan) state=$(awk '{print $3}' "/proc/$opid/stat" 2>/dev/null) syscall=$(proc_field "$opid" syscall)"
        log "  clock2.overlay heartbeat.t0=${hb1:-<missing>}"
        sleep 2
        hb2=$(cat "$hb" 2>/dev/null)
        log "  clock2.overlay heartbeat.t1=${hb2:-<missing>}"
        f1=$(frames_from "$hb1"); f2=$(frames_from "$hb2")
        if [ -n "$f1" ] && [ -n "$f2" ]; then
            if [ "$f2" -gt "$f1" ]; then advanced=yes; else advanced=no; fi
            log "  clock2.overlay frames_written $f1 -> $f2 over 2s (advanced=$advanced)"
        else
            log "  clock2.overlay frames_written unreadable; verdict will be inconclusive"
        fi
    else
        log "  clock2.overlay NO PROCESS (fifo='${fifo:-none}') -- channel may have no overlay configured"
    fi

    # A futex wait that still accrues CPU is a spin or contention; one that does
    # not is a genuine wedge. Same wchan, opposite meanings.
    if [ -n "$fpid" ] && [ -n "${u1:-}" ]; then
        u2=$(awk '{print $14+$15}' "/proc/$fpid/stat" 2>/dev/null)
        if [ -n "$u2" ]; then
            if [ "$u2" -gt "$u1" ]; then cpu=yes; else cpu=no; fi
            log "  clock1.ffmpeg cpu_ticks $u1 -> $u2 (advanced=$cpu)"
        fi
    fi

    ow=$(proc_field "$opid" wchan); fw=$(proc_field "$fpid" wchan)
    log "  $(verdict "$ow" "$fw" "$advanced" "$( [ -n "$fifo" ] && echo yes || echo no )" "$cpu")"
}

# ------------------------------------------------------------------- loop ----
log "two-clock capture started (watching $HLS_ROOT/*, gap_trigger=${GAP_TRIGGER}s)"

declare -A last_idx last_change in_gap last_capture

while true; do
    now=$(date +%s)
    scan_ffmpeg_pids
    for dir in "$HLS_ROOT"/*/; do
        [ -d "$dir" ] || continue
        ch=$(basename "$dir")
        case "$ch" in ''|*[!0-9]*) continue ;; esac   # numeric channel dirs only

        idx=$(newest_segment_index "$ch")
        [ -n "$idx" ] || continue                      # nothing served yet

        # A channel with no ffmpeg is not frozen, it is simply not running —
        # nobody is watching it, or the session was torn down. Its HLS folder
        # keeps the segments from the last session forever, so without this the
        # segment index sits still and every stale folder reports a permanent
        # freeze. Only a channel that HAS a transcode can stall.
        if [ -z "$(ffmpeg_pid_for "$ch")" ]; then
            if [ "${in_gap[$ch]:-0}" -eq 1 ]; then
                log "channel=$ch session ended; no longer watching for a stall"
                in_gap[$ch]=0
            fi
            unset "last_idx[$ch]" "last_change[$ch]"
            continue
        fi

        if [ "${last_idx[$ch]:-}" != "$idx" ]; then
            if [ "${in_gap[$ch]:-0}" -eq 1 ]; then
                log "RECOVERED channel=$ch after $(( now - ${last_change[$ch]} ))s; segment ${last_idx[$ch]} -> $idx"
                in_gap[$ch]=0
            fi
            last_idx[$ch]="$idx"
            last_change[$ch]="$now"
            continue
        fi

        gap=$(( now - ${last_change[$ch]:-$now} ))
        if [ "$gap" -ge "$GAP_TRIGGER" ]; then
            if [ "${in_gap[$ch]:-0}" -eq 0 ]; then
                in_gap[$ch]=1
                last_capture[$ch]="$now"
                capture "$ch" "gap=${gap}s first"
            elif [ $(( now - ${last_capture[$ch]:-0} )) -ge "$CAPTURE_EVERY" ]; then
                last_capture[$ch]="$now"
                capture "$ch" "gap=${gap}s ongoing"
            fi
        fi
    done
    sleep "$POLL"
done
