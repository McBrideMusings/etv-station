#!/bin/bash
# Container entrypoint: run the station daemon and the ETV-next server as one
# unit, with ETV-next's config derived from the station config at every start.
#
# The order below is the whole reason this script exists:
#
#   1. render ETV-next's lineup.json + channelN.json from the station config, so
#      the playout folders it reads are the ones the daemon is about to write;
#   2. create every channel's output folder BEFORE ETV-next boots — it resolves
#      each folder once at startup and silently drops any channel whose folder is
#      missing, which would otherwise make a channel's presence depend on how
#      fast its first generation ran;
#   3. start the daemon and let it emit a first playout file per channel;
#   4. start ETV-next.
#
# Either process exiting takes the container down (see the wait below), so a
# crashed daemon can't leave ETV-next serving a schedule that stops advancing.
set -uo pipefail

: "${ETV_STATION_CONFIG:=/config/station.yaml}"
: "${ETV_NEXT_DIR:=/config/etv-next}"
: "${ETV_LOG_FORMAT:=json}"
# How long to wait for the daemon's first playout file per channel before
# starting ETV-next anyway. A cold catalog ingest over a large Plex library
# takes well over a minute; the folders already exist by then, so a timeout
# costs a few "unable to find playout JSON" lines, not a missing channel.
: "${ETV_STARTUP_TIMEOUT:=180}"
: "${ETV_DIAG_DIR:=/data/diag}"
# How often the soak-test probe loop (#297) runs, in seconds. Default matches
# the issue's own example (`while true; do <probe>; sleep 3600; done`).
: "${SOAK_PROBE_INTERVAL_SECS:=3600}"

log() { printf '[entrypoint] %s\n' "$*"; }
die() { printf '[entrypoint] %s\n' "$*" >&2; exit 1; }

# Refuse to start on a directory we cannot write, and say why in the terms that
# actually diagnose it (#156).
#
# Without this the first symptom is the renderer below failing with
# "/config/etv-next/channel1.json: Permission denied (os error 13)", which reads
# like a bug in the renderer or in the channel that was just edited. It is
# neither: the *directory* arrived owned by somebody else. The config deploy
# mirrors the appdata folder from a laptop, and a sync that preserves ownership
# stamps the sending machine's uid onto directories the container then cannot
# write. `restart: unless-stopped` turns that into a loop, so every channel stays
# off the air until someone chowns the folder by hand.
#
# Nothing here can fix the ownership — the container is not root — so this exists
# purely to make one restart's log say the true thing instead of forty saying a
# misleading one.
require_writable() {
    local dir=$1 what=$2
    [ -w "$dir" ] && return 0
    # GNU stat (and BusyBox) take -c; BSD stat takes -f. The container is Linux
    # so -c is the one that fires there, but trying both means this message is
    # also correct when the script is exercised on a Mac.
    local owner
    owner=$(stat -c '%u:%g' "$dir" 2>/dev/null \
        || stat -f '%u:%g' "$dir" 2>/dev/null \
        || echo unknown)
    die "cannot write $what ($dir).
    directory is owned by uid:gid $owner
    this process runs as uid:gid $(id -u):$(id -g)
  Nothing is wrong with your station config. The deploy that copied this folder
  carried the sending machine's ownership onto it. Fix it on the host with:
    chown -R $(id -u):$(id -g) <the appdata folder>
  and make the deploy stop preserving owner/group so it does not recur."
}

[ -r "$ETV_STATION_CONFIG" ] || die "station config not readable: $ETV_STATION_CONFIG (bind-mount it, or set ETV_STATION_CONFIG)"

mkdir -p "$ETV_NEXT_DIR" || die "cannot create $ETV_NEXT_DIR"
# Checked before the render rather than after it fails, because the render is
# the first writer and its own error names only the file.
require_writable "$ETV_NEXT_DIR" "the ETV-next config directory"

# 1. ETV-next config, derived from the station config. Fatal on failure: booting
#    ETV-next on last run's lineup would serve a roster that no longer exists.
etv-station --config "$ETV_STATION_CONFIG" --render-etv-next "$ETV_NEXT_DIR" \
    || die "failed to render ETV-next config from $ETV_STATION_CONFIG"

# 2. Output folders, up front, for every channel in the config.
mapfile -t output_folders < <(etv-station --config "$ETV_STATION_CONFIG" --list-folders)
[ "${#output_folders[@]}" -gt 0 ] || die "no channels resolved from $ETV_STATION_CONFIG"
for folder in "${output_folders[@]}"; do
    mkdir -p "$folder" || die "cannot create playout folder $folder"
done
log "${#output_folders[@]} channel folder(s) ready"

# Teardown: whichever child ends first, stop the other and exit with its status.
station_pid=
etv_pid=
diag_pids=()
shutdown() {
    trap - TERM INT EXIT
    [ -n "$station_pid" ] && kill -TERM "$station_pid" 2>/dev/null
    [ -n "$etv_pid" ] && kill -TERM "$etv_pid" 2>/dev/null
    for pid in "${diag_pids[@]}"; do
        kill -TERM "$pid" 2>/dev/null
    done
    wait 2>/dev/null
}
trap shutdown TERM INT EXIT

# 2b. Diagnostics dir + station/etv log tee.
#
# diag_dir backs three things: the two opt-in packet-level diagnostics below
# (switched by ETV_DIAG_CAPTURE), the always-on tee of station/ersatztv stdout
# into a file (soak-probe.sh's log-scan check needs a file to grep — today
# that output only reaches the Docker log driver, which a process inside the
# container cannot read), and the soak probe loop's own evidence. Computed
# once, unconditionally, so an unwritable mount degrades all three the same
# way rather than each rediscovering the same failure separately.
diag_dir="$ETV_DIAG_DIR"
diag_ready=0
if mkdir -p "$diag_dir" 2>/dev/null && [ -w "$diag_dir" ]; then
    diag_ready=1
else
    log "WARNING: ETV_DIAG_DIR ($diag_dir) is not writable — no station/etv log capture, no soak probe"
fi

# How much ffmpeg-progress history to keep on disk, and how many rolled files.
# ~150 bytes a line, one line per second per channel: at six channels that is
# ~78MB/day, so the default keeps roughly a day and a half.
PROGRESS_LOG_MAX_BYTES="${ETV_PROGRESS_LOG_MAX_BYTES:-67108864}"
PROGRESS_LOG_KEEP="${ETV_PROGRESS_LOG_KEEP:-2}"

# How much station-etv.log to keep on disk, and how many rolled files (#299).
# station-etv.log only carries the two daemons' non-progress output —
# ffmpeg_progress is already split off above — so its volume is far below the
# progress log's. 32MB x 2 keeps a worst case of ~96MB on /data for the whole
# tee, still holds days of history for the soak (#20), and soak-probe.sh only
# ever needs the window since its last hourly run, not the full history.
STATION_LOG_MAX_BYTES="${ETV_STATION_LOG_MAX_BYTES:-33554432}"
STATION_LOG_KEEP="${ETV_STATION_LOG_KEEP:-2}"

# Runs "$@" in the background, splitting its combined stdout+stderr three ways
# via process substitution — which, unlike piping into a command directly, keeps
# $! pointing at "$@"'s own pid rather than the filter's, so station_pid/etv_pid
# tracking below is unaffected. Falls back to a plain background start when
# diag_dir isn't writable.
#
# ffmpeg_progress is split off from the Docker log driver deliberately. It logs
# once per second per channel; at six channels that is ~21,600 lines an hour and
# it had the container's log buffer down to ~2.8 hours of history even after
# days of uptime — so the record of what led up to a freeze was routinely gone
# before anyone looked. It is far too useful to switch off (it is the only thing
# that distinguishes "ffmpeg stopped encoding" from "ffmpeg is encoding and the
# output is not landing"), so it goes to its own rotated file instead of
# competing with everything else for the buffer.
#
# Everything else still reaches both the Docker log and station-etv.log, which
# soak-probe.sh greps.
# The splitter only sorts lines; it never rotates. Rotation by size from inside
# awk needs system() to interleave correctly with awk's own buffered writes, and
# that differs between awk implementations — it rotated at 2x the limit when
# tested. Size is handled by rotate_log() on its own loop below, where it is a
# plain stat-and-cp and cannot lose a line. The same reasoning is why
# station-etv.log's cap (#299) is also done there rather than inside this awk.
run_logged() {
    if [ "$diag_ready" -eq 1 ]; then
        "$@" > >(awk \
            -v prog="$diag_dir/ffmpeg-progress.log" \
            -v all="$diag_dir/station-etv.log" '
            /ffmpeg_progress/ { print >> prog; fflush(prog); next }
            { print >> all; fflush(all); print; fflush() }
        ') 2>&1 &
    else
        "$@" &
    fi
}

# Keep a log bounded by size. Called on a slow loop, so a rotation costs one
# stat and one copy-then-truncate and never blocks a writer. Generalized from
# the ffmpeg-progress-only version (#299) so the same shape covers
# station-etv.log too — both are held open for the container's whole life by
# a long-lived writer (the awk splitter below), so both need the same
# copy-then-truncate treatment, never a rename.
rotate_log() {
    local f="$1" max_bytes="$2" keep="$3" size i
    [ -f "$f" ] || return 0
    size=$(stat -c %s "$f" 2>/dev/null || stat -f %z "$f" 2>/dev/null) || return 0
    [ "${size:-0}" -ge "$max_bytes" ] || return 0
    for (( i = keep; i > 1; i-- )); do
        mv -f "$f.$(( i - 1 ))" "$f.$i" 2>/dev/null
    done
    # Copy-then-truncate, NOT rename. The awk splitter holds this path open for
    # the container's whole life; renaming it would leave awk appending to the
    # rolled file forever and the live path would never come back. `print >> f`
    # opens with O_APPEND, so a truncate makes the next write land at offset 0
    # instead of leaving a sparse hole.
    cp -f "$f" "$f.1" 2>/dev/null && : > "$f"
    log "rotated $(basename "$f") at ${size} bytes (keeping $keep)"
}

# These used to be two scripts copied onto the Unraid host by hand and started
# over ssh, on the belief that a container could not see a client's real
# address. It can — ETV-next logs the peer from the socket — so the only thing
# that arrangement bought was a capture that died at every reboot and was
# usually off when something finally went wrong.
#
# What the wire still answers that the access log cannot: a request that never
# reached the server at all, and a connection that died mid-answer. That is a
# small enough set to be worth having and too small to run always, so it is a
# switch — unlike the log tee and the soak probe loop above/below it, which
# are always on.
#
# Neither is supervised. If a capture dies the stream is unaffected, and the
# wait loop below only ends the container for its two real children — an
# unrecognised pid falls through and it waits again.
if [ -n "${ETV_DIAG_CAPTURE:-}" ] \
    && ! [[ "${ETV_DIAG_CAPTURE}" =~ ^(0|off|false|no)$ ]]; then
    if [ "$diag_ready" -eq 1 ]; then
        PORT="${ETV_PORT:-8409}" LOG_FILE="$diag_dir/access.log" \
            stream-access-log.py &
        diag_pids+=($!)
        HLS_ROOT="${ETV_HLS_OUTPUT:-/data/hls}" LOG_FILE="$diag_dir/stream-events.log" \
            stream-watch.py &
        diag_pids+=($!)
        log "diagnostics capturing to $diag_dir (ETV_DIAG_CAPTURE=${ETV_DIAG_CAPTURE})"
    else
        log "WARNING: ETV_DIAG_CAPTURE is set but $diag_dir is not writable — no capture"
    fi
fi

# The freeze capture is NOT behind ETV_DIAG_CAPTURE. A freeze is unannounced and
# rare — one every ~14 minutes across six channels at the worst observed rate,
# and none at all for hours before that — so a capture that is only on when
# somebody remembered to turn it on is a capture that is off when it matters.
# It costs one `ls` per channel per second and writes only when a channel has
# already stopped producing segments.
#
# Watches every channel itself, so adding a channel arms nothing and removing
# one cleans up nothing. Same non-fatal shape as the diagnostics above: its pid
# goes in diag_pids so shutdown() reaps it, and it is never fed to the
# `wait -n` recognized set, so if it dies the stream is unaffected.
if [ "$diag_ready" -eq 1 ]; then
    (
        while true; do
            ETV_DIAG_DIR="$diag_dir" ETV_HLS_OUTPUT="${ETV_HLS_OUTPUT:-/data/hls}" \
                /usr/local/bin/two-clock-capture.sh || true
            # Only reached if the watcher exits, which it should not. Restart it
            # rather than silently losing capture for the container's lifetime.
            log "WARNING: two-clock capture exited; restarting in 5s"
            sleep 5
        done
    ) &
    diag_pids+=($!)
    log "two-clock freeze capture started (evidence in $diag_dir/two-clock.log)"

    # Keep the split-off progress log, and station-etv.log (#299), bounded.
    # Slow loop: the files only need checking often enough that they cannot
    # overshoot badly between looks.
    (
        while true; do
            rotate_log "$diag_dir/ffmpeg-progress.log" "$PROGRESS_LOG_MAX_BYTES" "$PROGRESS_LOG_KEEP" || true
            rotate_log "$diag_dir/station-etv.log" "$STATION_LOG_MAX_BYTES" "$STATION_LOG_KEEP" || true
            sleep "${ETV_PROGRESS_ROTATE_INTERVAL_SECS:-60}"
        done
    ) &
    diag_pids+=($!)
fi

# 3. The daemon.
run_logged etv-station --config "$ETV_STATION_CONFIG" --log-format "$ETV_LOG_FORMAT"
station_pid=$!

# Poll all folders each tick and drop them as they fill, so the wait is
# max(per-channel), not the sum. A dead daemon short-circuits the wait — there
# is nothing left to wait for.
deadline=$((SECONDS + ETV_STARTUP_TIMEOUT))
pending=("${output_folders[@]}")
log "waiting up to ${ETV_STARTUP_TIMEOUT}s for the first playout JSON in ${#pending[@]} folder(s)..."
while [ "${#pending[@]}" -gt 0 ] && [ "$SECONDS" -lt "$deadline" ]; do
    kill -0 "$station_pid" 2>/dev/null || die "station daemon exited during startup"
    still=()
    for folder in "${pending[@]}"; do
        compgen -G "$folder/*.json" >/dev/null 2>&1 || still+=("$folder")
    done
    pending=("${still[@]}")
    [ "${#pending[@]}" -eq 0 ] && break
    sleep 0.5
done
if [ "${#pending[@]}" -gt 0 ]; then
    log "WARNING: timed out waiting for ${pending[*]} — starting ETV-next anyway"
fi

# 4. The server.
run_logged ersatztv "$ETV_NEXT_DIR/lineup.json"
etv_pid=$!
log "serving on ${ETV_BIND_ADDRESS:-0.0.0.0}:${ETV_PORT:-8409}"

# 5. Soak-test probe loop (#297): a third background process, same non-fatal
# shape as the diagnostics above — its pid goes into diag_pids so shutdown()
# reaps it, but it is never assigned to station_pid/etv_pid, so it is never
# fed into the `wait -n -p dead_pid` loop's recognized set below. A probe
# crash must never take the stack down — only a dead daemon or a dead
# ETV-next does that. No separate install step and no host cron: this loop
# starts with the container, exactly like the two diagnostics above it.
if [ "$diag_ready" -eq 1 ]; then
    (
        while true; do
            ETV_DIAG_DIR="$diag_dir" /usr/local/bin/soak-probe.sh || true
            sleep "$SOAK_PROBE_INTERVAL_SECS"
        done
    ) &
    diag_pids+=($!)
    log "soak probe loop started (every ${SOAK_PROBE_INTERVAL_SECS}s, evidence in $diag_dir)"
fi

# The two processes are not equals here. ETV-next streams the window the daemon
# has already written, so a dead daemon costs nothing until that window runs out
# — it is restarted in place and the stream never drops. A dead ETV-next is the
# service being down, so it ends the container and the restart policy takes it
# from there. This keeps the failure separation that having two containers used
# to provide.
restarts=0
while true; do
    wait -n -p dead_pid
    status=$?
    if [ "${dead_pid:-}" = "$etv_pid" ]; then
        log "ETV-next exited (status $status) — shutting down"
        exit "$status"
    fi
    if [ "${dead_pid:-}" = "$station_pid" ]; then
        restarts=$((restarts + 1))
        if [ "$restarts" -gt "${ETV_STATION_MAX_RESTARTS:-5}" ]; then
            log "station daemon exited (status $status) and has restarted $((restarts - 1)) time(s) already — shutting down"
            exit "$status"
        fi
        log "station daemon exited (status $status) — restarting (attempt $restarts); the stream keeps serving the window already written"
        sleep 2
        run_logged etv-station --config "$ETV_STATION_CONFIG" --log-format "$ETV_LOG_FORMAT"
        station_pid=$!
    fi
done
