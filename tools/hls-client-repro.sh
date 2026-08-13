#!/bin/bash
# Reproduce the freeze with a real VLC, and record what the client saw.
#
# The Apple TV that freezes identifies as `VLC/3.0.4 LibVLC/3.0.4` — the engine
# iPlayTV bundles. tvOS will not hand over its logs (devicectl sysdiagnose fails,
# and Console.app has no export), so the client half of the fault has only ever
# been visible as a shape in the server's access log: video requests stop, and
# subtitle requests carry on forever.
#
# Desktop VLC is the same demuxer. Run it against the same channel with `-vvv`
# and it narrates the decisions the Apple TV makes silently — which segment it
# wants next, what it thinks the playlist contains, and what it does when the
# one it wants is not there.
#
# Alongside it this snapshots the playlist once a second, so when the client
# gives up there is a record of the exact playlist it was looking at. That is the
# one piece of evidence the server-side logs cannot reconstruct after the fact:
# the playlist is rewritten in place, and the window has moved on by the time
# anybody notices the picture stopped.
#
#   ./hls-client-repro.sh [channel] [minutes]
#
# Everything lands in tmp/claude/repro/<channel>-<stamp>/.

set -u

CHANNEL="${1:-4}"
MINUTES="${2:-10}"

REPO=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
# shellcheck disable=SC1091
[ -r "$REPO/.env" ] && { set -a; . "$REPO/.env"; set +a; }

HOST_IP="${TAILSCALE_HOST_UNRAID:-${UNRAID_HOST:-}}"
PORT="${ETV_PORT_HOST:-8419}"
[ -n "$HOST_IP" ] || { echo "set UNRAID_HOST (or TAILSCALE_HOST_UNRAID) in .env" >&2; exit 1; }

URL="http://$HOST_IP:$PORT/channel/$CHANNEL.m3u8"
VLC="${VLC_BIN:-/Applications/VLC.app/Contents/MacOS/vlc}"
[ -x "$VLC" ] || { echo "no VLC at $VLC — set VLC_BIN" >&2; exit 1; }

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
OUT="$REPO/tmp/claude/repro/$CHANNEL-$STAMP"
mkdir -p "$OUT"

echo "channel   $CHANNEL"
echo "url       $URL"
echo "vlc       $("$VLC" --version 2>/dev/null | head -1)"
echo "output    $OUT"
echo

# --------------------------------------------------------------- the client ---
# --intf dummy: no window. --play-and-exit + --run-time: bounded.
# -vvv: the HLS demuxer narrates segment selection, which is the whole point.
# Audio and video are decoded and discarded, so the pacing is a real player's
# pacing rather than a curl loop's guess at it.
"$VLC" -I dummy -vvv \
    --no-video-title-show \
    --run-time="$(( MINUTES * 60 ))" \
    --play-and-exit \
    --aout=dummy --vout=dummy \
    "$URL" > "$OUT/vlc.log" 2>&1 &
VLC_PID=$!

# ------------------------------------------------------------- the playlist ---
# Once a second, and only when it changed — a byte-identical playlist second
# after second is noise, and the interesting moments are the transitions.
(
    prev=""
    while kill -0 "$VLC_PID" 2>/dev/null; do
        now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
        body=$(curl -s --max-time 3 "http://$HOST_IP:$PORT/session/$CHANNEL/live.m3u8")
        if [ "$body" != "$prev" ]; then
            {
                printf '===== %s =====\n' "$now"
                printf '%s\n' "$body"
            } >> "$OUT/playlist.log"
            prev="$body"
        fi
        sleep 1
    done
) &
SNAP_PID=$!

trap 'kill "$VLC_PID" "$SNAP_PID" 2>/dev/null' INT TERM

echo "running for $MINUTES minutes (vlc pid $VLC_PID)..."
wait "$VLC_PID" 2>/dev/null
kill "$SNAP_PID" 2>/dev/null

# ----------------------------------------------------------------- verdict ---
echo
echo "=== segment requests VLC made ==="
grep -oE "live[0-9]+\.ts" "$OUT/vlc.log" | sort -u | wc -l | tr -d ' ' | sed 's/^/  distinct segments: /'
grep -oE "live[0-9]+\.ts" "$OUT/vlc.log" | tail -1 | sed 's/^/  last segment:      /'

echo
echo "=== what VLC complained about ==="
grep -iE "error|cannot|failed|discontinuity|reload|dead|timeout|out of range|skipping" "$OUT/vlc.log" \
    | grep -viE "^$" | tail -15 | sed 's/^/  /'

echo
echo "=== playlist window over the run ==="
grep -E "^#EXT-X-MEDIA-SEQUENCE|^===== " "$OUT/playlist.log" 2>/dev/null | tail -12 | sed 's/^/  /'

echo
echo "logs: $OUT/vlc.log  $OUT/playlist.log"
