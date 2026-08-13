#!/bin/bash
# Measure what `-readrate N` actually delivers, against what it asks for.
#
# `tools/stall-bisect.sh` established that a channel's pipeline transcodes at
# ~18x unthrottled and ~0.69x with `-readrate 1.0` — so the throttle, not the
# hardware, is what puts a channel below realtime. Since ffmpeg's readrate can
# only ever slow the pipeline down and never let it catch up, a throttle that
# undershoots is a deficit the channel can never repay: the 44s initial burst
# drains, the segment cadence falls behind wall clock, and the watchdog tears
# the session down.
#
# This sweeps the value to answer the only question that decides the fix: does
# asking for slightly more than realtime deliver realtime?
#
#   ./readrate-sweep.sh [channel] [seconds-per-run] [rate ...]
#
# Each run transcodes the same slice of the same file with everything else held
# identical, including the initial burst. Delivered rate is measured from wall
# clock, and the burst is subtracted so the number reported is the sustained
# pace rather than the average including the head start.

set -u
CHANNEL="${1:-4}"; shift || true
DUR="${1:-180}"; shift || true
RATES=("$@")
[ "${#RATES[@]}" -gt 0 ] || RATES=(1.0 1.05 1.2 2.0)

APPDATA="${ETV_APPDATA:-/mnt/user/appdata/etv-station}"
DIAG="$APPDATA/data/diag"
ARGV_LOG="$DIAG/ffmpeg-argv-ch${CHANNEL}.log"
SCRATCH="/tmp/readrate-sweep-$CHANNEL"
OUT="$DIAG/readrate-sweep.log"

[ -r "$ARGV_LOG" ] || { echo "no argv log at $ARGV_LOG" >&2; exit 1; }

RUN_AS=$(docker inspect etv-station --format '{{.Config.User}}' 2>/dev/null)
[ -n "$RUN_AS" ] || RUN_AS="99:100"

rm -rf "$SCRATCH"; mkdir -p "$SCRATCH"
chown -R "${RUN_AS%%:*}:${RUN_AS##*:}" "$SCRATCH" 2>/dev/null

log() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$OUT"; }

mapfile -t ARGV < <(awk '
    /^argv_begin$/ { buf=""; inblk=1; real=0; next }
    /^argv_end$/   { if (inblk && real) last=buf; inblk=0; next }
    inblk          { if ($0 ~ /^\/media\//) real=1; buf = buf $0 "\n" }
    END            { printf "%s", last }
' "$ARGV_LOG")
[ "${#ARGV[@]}" -ge 5 ] || { echo "no media-backed argv block recorded yet" >&2; exit 1; }

BURST=$(for (( i=0; i<${#ARGV[@]}; i++ )); do
    [ "${ARGV[$i]}" = "-readrate_initial_burst" ] && echo "${ARGV[$((i+1))]}"
done | head -1)
BURST=${BURST:-0}

# Rebuild argv with a given readrate and our own duration/output.
build() {
    local rate="$1" segdir="$2" skip_next=0 i tok
    for (( i=0; i<${#ARGV[@]}; i++ )); do
        tok="${ARGV[$i]}"
        if [ "$skip_next" = 1 ]; then skip_next=0; continue; fi
        case "$tok" in
            -readrate) skip_next=1; printf -- '-readrate\n%s\n' "$rate"; continue ;;
            -t)        skip_next=1; printf -- '-t\n%sms\n' "$(( DUR * 1000 ))"; continue ;;
        esac
        case "$tok" in
            /data/hls/*/live%06d.ts) tok="$segdir/live%06d.ts" ;;
            /data/hls/*/ffmpeg.m3u8) tok="$segdir/ffmpeg.m3u8" ;;
        esac
        printf '%s\n' "$tok"
    done
}

log "================================================================"
log "readrate sweep: channel=$CHANNEL slice=${DUR}s burst=${BURST}s rates=${RATES[*]}"
log "source: $(printf '%s\n' "${ARGV[@]}" | grep -A1 '^-i$' | tail -1)"
log "================================================================"
log ""
printf '%-10s %-8s %-12s %-12s %s\n' "readrate" "wall" "expected" "delivered" "verdict" | tee -a "$OUT"

for rate in "${RATES[@]}"; do
    rm -rf "${SCRATCH:?}/run"; mkdir -p "$SCRATCH/run"
    chown -R "${RUN_AS%%:*}:${RUN_AS##*:}" "$SCRATCH" 2>/dev/null
    mapfile -t A < <(build "$rate" /scratch/run)

    t0=$(date +%s)
    docker run --rm --device /dev/dri --user "$RUN_AS" \
        -e LIBVA_DRIVER_NAME=iHD \
        -v /mnt/user/media/library:/media:ro \
        -v "$SCRATCH":/scratch \
        -v "$APPDATA/data":/data \
        --entrypoint ffmpeg etv-station:latest "${A[@]}" >>"$OUT" 2>&1
    rc=$?
    t1=$(date +%s)
    wall=$(( t1 - t0 )); [ "$wall" -eq 0 ] && wall=1

    # After the burst, `-readrate N` should deliver N x realtime. Expected wall
    # is therefore the burst (near-instant) plus the remainder at N.
    read -r expected delivered verdict <<<"$(awk -v d="$DUR" -v b="$BURST" -v r="$rate" -v w="$wall" 'BEGIN{
        rem = d - b; if (rem < 0) rem = 0
        exp_wall = rem / r
        got = (w > 0) ? (d - b) / w : 0
        v = (got >= r * 0.95) ? "ok" : "UNDERSHOOT"
        printf "%.0fs %.3fx %s", exp_wall, got, v
    }')"

    printf '%-10s %-8s %-12s %-12s %s%s\n' "$rate" "${wall}s" "$expected" "$delivered" "$verdict" \
        "$( [ "$rc" -ne 0 ] && echo " (rc=$rc)" )" | tee -a "$OUT"
done

log ""
log "sweep complete — a rate that delivers >= 1.0x sustained is what a channel needs"
