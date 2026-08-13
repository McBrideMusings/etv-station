#!/bin/sh
# ffmpeg wrapper that instruments an ETV-next channel transcode.
#
# ETV-next runs whatever `ffmpeg.ffmpeg_path` in a channel config points at, so
# pointing one channel at this script instruments that channel and nothing else.
# It changes no argument ETV-next chose: it records the argv, appends
# `-progress`, and execs the real binary.
#
# `-progress` is the whole point. When a channel stops emitting segments, the
# container log says only "no segments produced for 76s" — which cannot tell you
# whether ffmpeg stopped encoding, or kept encoding while its output failed to
# land. The progress stream answers that directly: ffmpeg writes a block every
# second carrying `frame`, `out_time_ms` and `speed`, so a gap with a live
# progress stream and a gap with a dead one are different bugs.
#
# Install: mount at /config/ffmpeg-probe.sh (chmod +x) and set
#   "ffmpeg": { "ffmpeg_path": "/config/ffmpeg-probe.sh" }
# for one channel via presentation.json. Removing that key removes the probe;
# nothing else has to be undone.

REAL="${ETV_PROBE_REAL_FFMPEG:-/usr/local/bin/ffmpeg}"
DIAG="${ETV_PROBE_DIAG_DIR:-/data/diag}"

[ -d "$DIAG" ] || mkdir -p "$DIAG" 2>/dev/null

# Which channel is this? ETV-next names the HLS output folder after the channel
# number, so the segment path is the only place the number appears in argv.
channel=""
for a in "$@"; do
    case "$a" in
        */hls/*/live%06d.ts|*/hls/*/ffmpeg.m3u8)
            channel=$(printf '%s\n' "$a" | sed -n 's#.*/hls/\([0-9][0-9]*\)/.*#\1#p')
            break
            ;;
    esac
done

stamp=$(date -u +%Y%m%dT%H%M%S)

# Capability probes (`-hwaccels`, `-filters`, …) run constantly and are not
# transcodes. Instrumenting them would bury the run we care about, and adding
# -progress to them would be meaningless.
if [ -z "$channel" ]; then
    exec "$REAL" "$@"
fi

argv_log="$DIAG/ffmpeg-argv-ch${channel}.log"
progress="$DIAG/ffmpeg-progress-ch${channel}-${stamp}-$$.log"

# One argument per line, not a single space-joined line. Media paths routinely
# contain spaces — "Hidalgo (2004) {imdb-tt0317648}/…" — so a space-joined
# record cannot be split back into the original argv, and anything replaying it
# opens the wrong file.
{
    printf '=== %s pid=%s channel=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$$" "$channel"
    printf 'progress=%s\n' "$progress"
    printf 'argv_begin\n'
    for a in "$@"; do printf '%s\n' "$a"; done
    printf 'argv_end\n'
} >> "$argv_log" 2>/dev/null

# Keep only the last few runs per channel so a respawn loop cannot fill the
# volume — the interesting run is always the most recent one.
# shellcheck disable=SC2012
ls -1t "$DIAG"/ffmpeg-progress-ch${channel}-*.log 2>/dev/null | tail -n +9 | while read -r old; do
    rm -f "$old" 2>/dev/null
done

# `exec`, and no pipeline. ETV-next stops a session by signalling the process it
# spawned; if this script stayed alive as a pipeline parent, the signal would
# land on the wrapper and ffmpeg would be orphaned rather than stopped. That
# rules out teeing stderr here — ETV-next already captures ffmpeg's stderr into
# the container log, so nothing is lost by leaving it alone.
#
# -progress goes last so it cannot land between an option and its value.
exec "$REAL" "$@" -progress "$progress"
