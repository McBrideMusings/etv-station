#!/bin/bash
# Verify soak-probe.sh's rotation detection (#355).
#
# Why this exists: the original detection inferred a rotation from the live
# file's line count SHRINKING between probes. entrypoint.sh's rotate_log()
# runs on its own interval (default 60s), independent of soak-probe.sh's
# schedule (default hourly) — a rotation followed by enough writes to regrow
# the live file past the old saved line count BEFORE the next probe runs
# makes the shrink invisible, and the next probe then resumes its
# zero-tolerance scan at an offset pointing into unrelated post-rotation
# content. That is exactly the skipped-lines gap the check exists to close.
#
# This drives the real sequence end to end — write, probe, rotate, regrow
# PAST the old line count, probe again — and asserts the second probe still
# scans the post-rotation lines. A check that rotates and immediately
# re-probes (no regrowth) passes under the old, broken code too and proves
# nothing; the regrowth step is the whole bug.
#
#   ./tools/soak-rotation-check.sh
#
# Exit 0 when rotation is still detected after regrowth, 1 otherwise.

set -u

W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
fails=0

DIAG_DIR="$W/diag"
mkdir -p "$DIAG_DIR"
STATION_LOG="$DIAG_DIR/station-etv.log"
STATE_FILE="$DIAG_DIR/soak-probe.state"
RESULT_LOG="$DIAG_DIR/soak-probe.log"

# Mirror entrypoint.sh's rotate_log() exactly (as tools/progress-split-check.sh
# already does) — copy-then-truncate, NOT rename, since a long-lived writer
# holds the live path open for the container's whole life.
STATION_LOG_KEEP=2
rotate_log() {
    local f="$1" keep="$2" i
    [ -f "$f" ] || return 0
    for (( i = keep; i > 1; i-- )); do
        mv -f "$f.$(( i - 1 ))" "$f.$i" 2>/dev/null
    done
    cp -f "$f" "$f.1" 2>/dev/null && : > "$f"
}

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

run_probe() {
    ETV_PORT=1 \
    ETV_DIAG_DIR="$DIAG_DIR" \
    SOAK_PROBE_CHANNEL=1 \
    SOAK_PROBE_MIN_COVERAGE_DAYS=7 \
    SOAK_PROBE_KEEP=8 \
        bash "$SCRIPT_DIR/soak-probe.sh" >/dev/null 2>&1 || true
}

last_log_scan() {
    tail -n 1 "$RESULT_LOG" 2>/dev/null | tr '\t' '\n' | grep '^log_scan=' | cut -d= -f2
}

echo "--- scenario ---"

# 1. Seed a clean log and run the first probe — no station running, so the
#    HTTP checks fail regardless; only log_scan is asserted below.
for i in $(seq 1 20); do
    printf '[2026-08-23T00:00:%02dZ INFO station] tick %d\n' "$i" "$i" >>"$STATION_LOG"
done
pre_rotation_lines=$(wc -l <"$STATION_LOG" | tr -d ' ')
run_probe

first_scan="$(last_log_scan)"
if [ "$first_scan" = "pass" ]; then
    echo "  ok   first probe scanned the seeded lines cleanly (log_scan=pass)"
else
    echo "  FAIL first probe expected log_scan=pass, got '$first_scan'"; fails=$((fails+1))
fi

# 2. No rotation between runs: a second probe with nothing new written must
#    resume from its saved offset, not rescan the whole log. Prove it by
#    corrupting the ALREADY-SCANNED region with a failing line: if the probe
#    rescanned from line 1, it would catch this and fail; if it correctly
#    resumes past it, this stays invisible.
sed -i.bak '1s/.*/[2026-08-23T00:00:01Z ERROR station] unable to find playout JSON file for time old-and-scanned/' "$STATION_LOG"
rm -f "$STATION_LOG.bak"
run_probe
no_rotation_scan="$(last_log_scan)"
if [ "$no_rotation_scan" = "pass" ]; then
    echo "  ok   probe with no rotation resumed from saved offset (didn't rescan line 1)"
else
    echo "  FAIL probe with no rotation re-scanned from line 1 (log_scan='$no_rotation_scan') — offset not honored"; fails=$((fails+1))
fi
# Restore the corrupted line so it doesn't confuse the next stage's counts.
sed -i.bak '1s/.*/[2026-08-23T00:00:01Z INFO station] tick 1/' "$STATION_LOG"
rm -f "$STATION_LOG.bak"

# 3. THE BUG: rotate, then regrow PAST the pre-rotation line count, then
#    probe. A post-rotation failing line must still be caught.
rotate_log "$STATION_LOG" "$STATION_LOG_KEEP"

regrow_count=$((pre_rotation_lines + 5))
for i in $(seq 1 "$regrow_count"); do
    if [ "$i" -eq 3 ]; then
        printf '[2026-08-23T01:00:%02dZ ERROR station] unable to find playout JSON file for time post-rotation\n' "$i" >>"$STATION_LOG"
    else
        printf '[2026-08-23T01:00:%02dZ INFO station] post-rotation tick %d\n' "$i" "$i" >>"$STATION_LOG"
    fi
done
post_rotation_lines=$(wc -l <"$STATION_LOG" | tr -d ' ')

if [ "$post_rotation_lines" -le "$pre_rotation_lines" ]; then
    echo "  FAIL test setup bug: regrown log ($post_rotation_lines lines) did not exceed pre-rotation count ($pre_rotation_lines)"
    fails=$((fails+1))
else
    echo "  ok   regrown log ($post_rotation_lines lines) exceeds pre-rotation count ($pre_rotation_lines) — shrink is invisible"
fi

run_probe
second_scan="$(last_log_scan)"
if [ "$second_scan" = "fail" ]; then
    echo "  ok   post-rotation probe caught the injected failure (log_scan=fail) — rotation detected despite regrowth"
else
    echo "  FAIL post-rotation probe did not detect the failure (log_scan='$second_scan') — rotation was missed, lines silently skipped"
    fails=$((fails+1))
fi

echo
[ "$fails" -eq 0 ] && { echo "RESULT: PASS"; exit 0; }
echo "RESULT: $fails assertion(s) FAILED"
exit 1
