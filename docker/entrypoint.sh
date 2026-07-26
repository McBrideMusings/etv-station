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

log() { printf '[entrypoint] %s\n' "$*"; }
die() { printf '[entrypoint] %s\n' "$*" >&2; exit 1; }

[ -r "$ETV_STATION_CONFIG" ] || die "station config not readable: $ETV_STATION_CONFIG (bind-mount it, or set ETV_STATION_CONFIG)"

mkdir -p "$ETV_NEXT_DIR" || die "cannot create $ETV_NEXT_DIR"
[ -r "$ETV_NEXT_DIR/normalization.default.json" ] \
    || die "missing $ETV_NEXT_DIR/normalization.default.json — it carries the ffmpeg/normalization block every channel is built from"

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
shutdown() {
    trap - TERM INT EXIT
    [ -n "$station_pid" ] && kill -TERM "$station_pid" 2>/dev/null
    [ -n "$etv_pid" ] && kill -TERM "$etv_pid" 2>/dev/null
    wait 2>/dev/null
}
trap shutdown TERM INT EXIT

# 3. The daemon.
etv-station --config "$ETV_STATION_CONFIG" --log-format "$ETV_LOG_FORMAT" &
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
ersatztv "$ETV_NEXT_DIR/lineup.json" &
etv_pid=$!
log "serving on ${ETV_BIND_ADDRESS:-0.0.0.0}:${ETV_PORT:-8409}"

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
        etv-station --config "$ETV_STATION_CONFIG" --log-format "$ETV_LOG_FORMAT" &
        station_pid=$!
    fi
done
