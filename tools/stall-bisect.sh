#!/bin/bash
# Bisect why a channel's transcode runs below realtime.
#
# `tools/ffmpeg-probe.sh` records the exact argv ETV-next built for a channel.
# This replays that argv with one thing changed at a time, so the difference
# between "the same file benchmarks at 6x by hand" and "the channel manages
# 0.48x" can be attributed instead of guessed at.
#
# Variants, each run for a fixed slice of the same file:
#   baseline   argv as ETV-next built it, minus the readrate throttle, writing
#              segments to local disk        -> what the pipeline can actually do
#   to-array   baseline, but segments to the real HLS directory on the array
#              -> cost of writing to the user share
#   throttled  argv exactly as ETV-next built it, segments local
#              -> cost of the readrate pacing itself
#   no-audio   baseline with the audio branch dropped
#              -> cost of the audio filter/encode path
#
# Nothing here touches the running channel: every variant writes to its own
# scratch directory and none of them serves anybody.
#
#   ./stall-bisect.sh [channel] [seconds]

set -u
CHANNEL="${1:-4}"
DUR="${2:-60}"

APPDATA="${ETV_APPDATA:-/mnt/user/appdata/etv-station}"
DIAG="$APPDATA/data/diag"
ARGV_LOG="$DIAG/ffmpeg-argv-ch${CHANNEL}.log"
SCRATCH="/tmp/stall-bisect-$CHANNEL"
OUT="$DIAG/stall-bisect.log"

[ -r "$ARGV_LOG" ] || { echo "no argv log at $ARGV_LOG — is ffmpeg_path pointed at ffmpeg-probe.sh?" >&2; exit 1; }

# Run every variant as the same user the real container runs as, so the
# comparison is like-for-like and the scratch directories are writable.
RUN_AS=$(docker inspect etv-station --format '{{.Config.User}}' 2>/dev/null)
[ -n "$RUN_AS" ] || RUN_AS="99:100"

rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/local"
chown -R "${RUN_AS%%:*}:${RUN_AS##*:}" "$SCRATCH" 2>/dev/null

log() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$OUT"; }

# The most recent recorded invocation is the one currently airing. The probe
# writes one argument per line between argv_begin/argv_end precisely so this can
# be read back without guessing where a path with spaces ends.
# Only blocks that actually read a media file. ETV-next also spawns filler
# sessions built on `anullsrc` (silence and black) between items and after a
# failure, and those are both common and pointless to bisect — they transcode
# nothing. Taking the last block blindly picks one of those about as often as
# not.
mapfile -t ARGV < <(awk '
    /^argv_begin$/ { buf=""; inblk=1; real=0; next }
    /^argv_end$/   { if (inblk && real) last=buf; inblk=0; next }
    inblk          { if ($0 ~ /^\/media\//) real=1; buf = buf $0 "\n" }
    END            { printf "%s", last }
' "$ARGV_LOG")

if [ "${#ARGV[@]}" -lt 5 ]; then
    echo "argv log has no argv_begin/argv_end block yet — the channel has not respawned" >&2
    echo "since ffmpeg-probe.sh was updated. Wait for the next session, or restart it." >&2
    exit 1
fi

# ffmpeg is run through the container, because the pipeline references container
# paths (/media, /data) and the host's own ffmpeg is a different build.
run_variant() {
    local name="$1"; shift
    local -a args=("$@")
    log "--- $name ---"
    local t0 t1
    t0=$(date +%s)
    # Same uid as the real container. Without it the variant runs as the image's
    # default user, cannot write its segments, and every run "finishes" in a
    # second or two with a permission error — which reads as an implausibly fast
    # result rather than as a failure.
    docker run --rm --device /dev/dri \
        --user "$RUN_AS" \
        -e LIBVA_DRIVER_NAME=iHD \
        -v /mnt/user/media/library:/media:ro \
        -v "$SCRATCH":/scratch \
        -v "$APPDATA/data":/data \
        --entrypoint ffmpeg etv-station:latest "${args[@]}" >>"$OUT" 2>&1
    local rc=$?
    t1=$(date +%s)
    local wall=$(( t1 - t0 ))
    [ "$wall" -eq 0 ] && wall=1
    log "    rc=$rc wall=${wall}s for ${DUR}s of video => $(awk -v d="$DUR" -v w="$wall" 'BEGIN{printf "%.2fx", d/w}')"
}

# Rebuild the argv, applying substitutions. Reads the recorded argv and emits a
# modified copy on stdout, one token per line.
build() {
    local drop_readrate="$1" segdir="$2" drop_audio="$3"
    local skip_next=0
    local i tok
    for (( i=0; i<${#ARGV[@]}; i++ )); do
        tok="${ARGV[$i]}"
        if [ "$skip_next" = 1 ]; then skip_next=0; continue; fi
        case "$tok" in
            -readrate|-readrate_initial_burst)
                if [ "$drop_readrate" = 1 ]; then skip_next=1; continue; fi ;;
            -t)
                # Replace ETV-next's item duration (the rest of the film) with
                # our fixed slice, so every variant transcodes the same amount.
                skip_next=1
                printf -- '-t\n%sms\n' "$(( DUR * 1000 ))"
                continue ;;
        esac
        # Redirect the segment and playlist paths at the scratch directory.
        case "$tok" in
            /data/hls/*/live%06d.ts) tok="$segdir/live%06d.ts" ;;
            /data/hls/*/ffmpeg.m3u8) tok="$segdir/ffmpeg.m3u8" ;;
        esac
        printf '%s\n' "$tok"
    done
}

log "================================================================"
log "bisect channel=$CHANNEL slice=${DUR}s"
log "source: $(printf '%s\n' "${ARGV[@]}" | grep -A1 '^-i$' | tail -1)"
log "================================================================"

mapfile -t A_BASE < <(build 1 /scratch/local 0)
run_variant "baseline (no throttle, segments to local disk)" "${A_BASE[@]}"

mapfile -t A_ARRAY < <(build 1 "/data/hls/bisect$CHANNEL" 0)
mkdir -p "$APPDATA/data/hls/bisect$CHANNEL"
chown "${RUN_AS%%:*}:${RUN_AS##*:}" "$APPDATA/data/hls/bisect$CHANNEL" 2>/dev/null
run_variant "to-array (no throttle, segments to the user share)" "${A_ARRAY[@]}"
rm -rf "${APPDATA:?}/data/hls/bisect$CHANNEL"

mapfile -t A_THROTTLE < <(build 0 /scratch/local 0)
run_variant "throttled (readrate exactly as ETV-next set it)" "${A_THROTTLE[@]}"

log "bisect complete — full ffmpeg output above in $OUT"
