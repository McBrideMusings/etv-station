#!/usr/bin/env bash
# Pin the one invariant tools/ffmpeg-probe.sh exists to respect: it records the
# argv and changes NOTHING.
#
# The probe used to append its own `-progress <file>`. ffmpeg's `-progress` is a
# single AVIO target, so that silently REPLACED the `-progress pipe:1` ETV-next
# splices in at argv position 0 (channel_session.rs:866). ETV-next was then left
# reading a stdout that never received a byte, and the rotated daemon log at
# /data/diag/ffmpeg-progress.log went dead at 2026-08-21 15:57Z (11:57am ET) --
# one minute before the always-on probe first ran -- and stayed dead through 25
# channel stalls. Nothing failed loudly; the log just stopped.
#
# That is the whole class this file guards: an argv edit by the wrapper that
# looks additive and is actually destructive. Assert pass-through, not the
# absence of one specific flag.
#
#   tools/ffmpeg-probe-check.sh     # exits non-zero on any failure
#
# No container, no host, no ffmpeg. The real binary is stubbed with a script
# that dumps its argv, so this runs anywhere.

set -uo pipefail

PROBE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/ffmpeg-probe.sh"
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT

fails=0
ok()   { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1"; fails=$((fails + 1)); }

# Stub for the real ffmpeg: one argument per line, so an argv carrying spaces in
# a media path round-trips exactly.
cat > "$W/fake-ffmpeg" <<'STUB'
#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done > "$FAKE_FFMPEG_ARGV_OUT"
exit 0
STUB
chmod +x "$W/fake-ffmpeg"

run_probe() {
    FAKE_FFMPEG_ARGV_OUT="$W/seen.txt" \
    ETV_PROBE_REAL_FFMPEG="$W/fake-ffmpeg" \
    ETV_PROBE_DIAG_DIR="$W/diag" \
        sh "$PROBE" "$@"
}

# A realistic transcode argv: ETV-next's own -progress first, a media path with
# spaces and parentheses, and the HLS output url the channel number is read from.
MEDIA='/media/television/Joe Pera Talks with You (2018) {imdb-tt8199790}/S02E07.mkv'
TRANSCODE_ARGV=(
    -progress pipe:1
    -hwaccel vaapi
    -i "$MEDIA"
    -vcodec h264_vaapi
    -hls_segment_filename /data/hls/1/live%06d.ts
    /data/hls/1/ffmpeg.m3u8
)

printf '%s\n' "--- transcode argv ---"
run_probe "${TRANSCODE_ARGV[@]}"

if [ ! -s "$W/seen.txt" ]; then
    fail "the real binary was never reached"
else
    # 1. Pass-through, byte for byte. This subsumes every specific flag check:
    #    if argv is identical, no flag was added, dropped or reordered.
    printf '%s\n' "${TRANSCODE_ARGV[@]}" > "$W/want.txt"
    if diff -q "$W/want.txt" "$W/seen.txt" >/dev/null; then
        ok "argv reaches ffmpeg unchanged"
    else
        fail "argv was modified by the wrapper:"
        diff "$W/want.txt" "$W/seen.txt" | sed 's/^/       /'
    fi

    # 2. The specific regression, named so a failure reads as itself rather than
    #    as a generic diff.
    n_progress=$(grep -c -- '^-progress$' "$W/seen.txt")
    if [ "$n_progress" -eq 1 ]; then
        ok "exactly one -progress survives (ETV-next's own)"
    else
        fail "expected 1 -progress, found $n_progress — a second one replaces the first and kills /data/diag/ffmpeg-progress.log"
    fi

    if grep -qx 'pipe:1' "$W/seen.txt"; then
        ok "-progress still targets pipe:1, so log_ffmpeg_progress receives it"
    else
        fail "-progress no longer targets pipe:1"
    fi

    # 3. The media path must survive intact — this is why the argv log is written
    #    one argument per line rather than space-joined.
    if grep -qxF "$MEDIA" "$W/seen.txt"; then
        ok "media path with spaces and parens round-trips"
    else
        fail "media path was mangled"
    fi
fi

# 4. The argv record is the probe's actual job; assert it still does it.
argv_log="$W/diag/ffmpeg-argv-ch1.log"
if [ -s "$argv_log" ] && grep -q 'argv_begin' "$argv_log" && grep -qxF "$MEDIA" "$argv_log"; then
    ok "argv recorded to $(basename "$argv_log")"
else
    fail "argv log missing or incomplete at $argv_log"
fi

# 5. A capability probe has no HLS url, so no channel can be derived. It must
#    exec straight through and write no log — instrumenting these buries the run
#    that matters.
printf '%s\n' "--- capability probe argv ---"
rm -f "$W/seen.txt"
run_probe -hwaccels
if [ -s "$W/seen.txt" ] && [ "$(cat "$W/seen.txt")" = "-hwaccels" ]; then
    ok "capability probe passes through untouched"
else
    fail "capability probe was altered or never reached ffmpeg"
fi

printf '\n'
if [ "$fails" -eq 0 ]; then
    printf 'RESULT: PASS\n'
    exit 0
fi
printf 'RESULT: FAIL (%d)\n' "$fails"
exit 1
