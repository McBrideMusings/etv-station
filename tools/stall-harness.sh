#!/bin/bash
# Reproduce and instrument the production gap that makes a channel freeze.
#
# The symptom this exists for: a channel stops emitting HLS segments for ~76
# seconds while somebody is watching, ETV-next's watchdog tears the session
# down, and the viewer's player is left on a dead playlist. The container log
# reports only that it happened. This runs unattended until it happens again and
# records what the transcode was doing while it happened.
#
# Runs ON the Unraid host, not in the container: it needs /proc for the ffmpeg
# process and the host's GPU counters, and the HLS directory is a bind mount it
# can watch directly.
#
#   ./stall-harness.sh [channel] [--gap N] [--rounds N]
#
# Leaves everything it learns in $DIAG/stall-harness.log.

set -u

CHANNEL="${1:-4}"
[ "${CHANNEL#-}" != "$CHANNEL" ] && CHANNEL=4   # first arg was a flag

GAP_TRIGGER=10          # seconds without a new segment before we start capturing
ROUNDS=0                # 0 = run until killed
POLL=1                  # segment-index poll interval
CAPTURE_EVERY=3         # capture cadence while inside a gap

while [ $# -gt 0 ]; do
    case "$1" in
        --gap) GAP_TRIGGER="$2"; shift 2 ;;
        --rounds) ROUNDS="$2"; shift 2 ;;
        *) shift ;;
    esac
done

APPDATA="${ETV_APPDATA:-/mnt/user/appdata/etv-station}"
HLS="$APPDATA/data/hls/$CHANNEL"
DIAG="$APPDATA/data/diag"
PORT="${ETV_PORT_HOST:-8419}"
LOG="$DIAG/stall-harness.log"

mkdir -p "$DIAG"

log() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*" >> "$LOG"; }

# ---------------------------------------------------------------- viewer ----
# A channel only misbehaves while it believes somebody is watching — the
# watchdog message is literally "while a viewer is watching", and with no viewer
# the session idles out instead. So the harness has to BE a viewer, continuously.
# ffmpeg reading the HLS playlist behaves like a player: it polls the playlist
# and pulls each segment at playback speed.
viewer_loop() {
    while true; do
        ffmpeg -nostdin -hide_banner -loglevel warning \
               -i "http://127.0.0.1:$PORT/channel/$CHANNEL.m3u8" \
               -f null - >> "$DIAG/stall-harness-viewer.log" 2>&1
        log "VIEWER exited (rc=$?) — retuning in 2s"
        sleep 2
    done
}

# How long since ANY segment was last written, in seconds.
#
# Deliberately not the segment index. A teardown wipes the HLS directory and the
# next session starts numbering at zero again, so an index that moves is not
# evidence of progress — 31 -> 0 is a restart, which is the very failure being
# hunted, and comparing indices for inequality reads it as healthy. The mtime of
# the newest file on disk cannot be fooled that way: nothing written means
# nothing written, restart or not. An empty directory reports a large age, which
# is correct — a channel with no segments at all is not producing.
# Sets $SINCE rather than echoing it. `since=$(fn)` would run this in a
# subshell, and the `dir_empty_since` bookkeeping below would be discarded every
# call — leaving the empty-directory clock permanently unset and every wipe
# reported as a gap the length of the harness's own uptime.
SINCE=0
compute_since() {
    local newest now
    newest=$(stat -c %Y "$HLS"/*.ts 2>/dev/null | sort -n | tail -1)
    now=$(date +%s)
    if [ -n "$newest" ]; then
        dir_empty_since=""          # directory has content again
        SINCE=$(( now - newest ))
        return
    fi
    # Empty directory. Time it from when it FIRST went empty — a teardown wipes
    # the directory, and clocking that from harness start reports a gap of
    # however long the harness has been up, which is both wrong and alarming.
    [ -n "${dir_empty_since:-}" ] || dir_empty_since=$now
    SINCE=$(( now - dir_empty_since ))
}

newest_index() {
    ls "$HLS"/*.ts 2>/dev/null | sed 's#.*/live0*##; s#\.ts$##' | sort -n | tail -1
}

channel_pid() {
    pgrep -f "hls/$CHANNEL/ffmpeg.m3u8" | head -1
}

# --------------------------------------------------------------- capture ----
# Everything here answers one question: during the gap, what is the transcode
# actually doing? Each block distinguishes a different cause.
capture() {
    local why="$1" pid
    pid=$(channel_pid)

    log "---------------- CAPTURE ($why) ----------------"
    if [ -z "$pid" ]; then
        log "  no ffmpeg process for channel $CHANNEL — the session is gone, not stalled"
        log "  container log:"
        docker logs --since 2m etv-station 2>&1 | grep -E "channel $CHANNEL|no segments|stall|terminated|exited" \
            | tail -5 | sed 's/^/    /' >> "$LOG"
        return
    fi

    # Is ffmpeg still encoding? The progress stream is the direct answer: a live
    # stream during a gap means frames are being produced and the output is not
    # landing; a frozen one means the encode itself stopped.
    # Read the daemon's progress log, not a per-channel file. tools/ffmpeg-probe.sh
    # used to append its own `-progress`, which replaced ETV-next's `-progress
    # pipe:1` and killed the daemon log; the probe no longer does that, so this
    # is the only progress record there is. It is also the better one: every line
    # is timestamped, so a stale tail is visible without stat'ing a file.
    local prog="$DIAG/ffmpeg-progress.log"
    if [ -s "$prog" ]; then
        log "  progress tail (channel $CHANNEL, from $(basename "$prog")):"
        grep "channel $CHANNEL:" "$prog" 2>/dev/null \
            | tail -12 | sed 's/^/    /' >> "$LOG"
    else
        log "  no $prog — is the ffmpeg_progress DEBUG target on, and is docker/entrypoint.sh splitting it?"
    fi

    # Process and per-thread state. A gap where every thread sits in
    # futex_wait_queue is a lock/queue problem; one sitting in a read on the
    # media file is an I/O problem; hrtimer_nanosleep is just -readrate pacing.
    log "  ps: $(ps -o stat,pcpu,pmem,etime,wchan:26 -p "$pid" --no-headers 2>/dev/null)"
    log "  threads:"
    for t in /proc/"$pid"/task/*; do
        [ -d "$t" ] || continue
        printf '    tid=%s comm=%-16s state=%s wchan=%s\n' \
            "$(basename "$t")" \
            "$(cat "$t/comm" 2>/dev/null)" \
            "$(awk '{print $3}' "$t/stat" 2>/dev/null)" \
            "$(cat "$t/wchan" 2>/dev/null)" >> "$LOG"
    done
    log "  kernel stack:"
    head -8 /proc/"$pid"/stack 2>/dev/null | sed 's/^/    /' >> "$LOG"

    # Is it moving bytes at all, and in which direction? Reading but not writing
    # points downstream of the decoder; neither points at the input.
    local io0 io1
    io0=$(tr '\n' ' ' < /proc/"$pid"/io 2>/dev/null)
    sleep 2
    io1=$(tr '\n' ' ' < /proc/"$pid"/io 2>/dev/null)
    log "  io t0: $io0"
    log "  io t2: $io1"

    # Where is the read head in the source file, and is it advancing? A stuck
    # offset with the array busy is a storage stall, not a transcode stall.
    log "  fd offsets:"
    for fd in /proc/"$pid"/fd/*; do
        local tgt n
        tgt=$(readlink "$fd" 2>/dev/null) || continue
        case "$tgt" in
            /media/*|*/hls/*)
                n=$(basename "$fd")
                printf '    fd=%-4s pos=%-14s %s\n' "$n" \
                    "$(awk '/^pos:/{print $2}' /proc/"$pid"/fdinfo/"$n" 2>/dev/null)" "$tgt" >> "$LOG"
                ;;
        esac
    done

    # The GPU, since the encode is on it now. Near-zero engine busy during a gap
    # means nothing is being submitted; pegged means the GPU is the queue.
    if command -v intel_gpu_top >/dev/null 2>&1; then
        log "  igpu: $(timeout 4 intel_gpu_top -J -s 900 2>/dev/null | tr -d '\n ' | head -c 400)"
    fi

    log "  hls dir (newest 4):"
    ls -l --time-style=+%H:%M:%S "$HLS"/*.ts 2>/dev/null | tail -4 | awk '{print "    "$6" "$5" "$NF}' >> "$LOG"
    log "  host load: $(uptime | sed 's/.*load average/load/')"
}

# ------------------------------------------------------------------ main ----
log "================================================================"
log "harness start: channel=$CHANNEL gap_trigger=${GAP_TRIGGER}s hls=$HLS"
log "================================================================"

viewer_loop &
VIEWER_PID=$!
# shellcheck disable=SC2064
trap "kill $VIEWER_PID 2>/dev/null; log 'harness stopped'; exit 0" INT TERM

round=0
in_gap=0
gap_started=0
dir_empty_since=""

while true; do
    now=$(date +%s)
    compute_since
    since=$SINCE
    idx=$(newest_index)

    if [ "$since" -lt "$GAP_TRIGGER" ] && [ "$in_gap" = 1 ]; then
        dur=$(( now - gap_started ))
        log "*** GAP ENDED after ${dur}s — segments flowing again (index $idx)"
        capture "recovery, gap lasted ${dur}s"
        log "*** end of round $round"
        in_gap=0
        round=$(( round + 1 ))
        if [ "$ROUNDS" -gt 0 ] && [ "$round" -ge "$ROUNDS" ]; then
            log "reached $ROUNDS rounds — stopping"
            kill $VIEWER_PID 2>/dev/null
            exit 0
        fi
    fi

    if [ "$in_gap" = 0 ] && [ "$since" -ge "$GAP_TRIGGER" ]; then
        in_gap=1
        gap_started=$(( now - since ))
        log ""
        log "*** GAP DETECTED: nothing written for ${since}s (newest index ${idx:-none})"
        capture "gap opened, ${since}s"
    elif [ "$in_gap" = 1 ]; then
        # Keep sampling through the gap so the shape is visible, not just its
        # start — the watchdog fires at 76s, so this captures the whole run-up.
        if [ $(( since % CAPTURE_EVERY )) -eq 0 ]; then
            capture "still stalled, ${since}s"
        fi
    fi

    sleep "$POLL"
done
