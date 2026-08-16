#!/usr/bin/env bash
# One-shot soak-test probe (#297): a single probe run against the *deployed*,
# already-running station — no dev-run.sh, no build, no process management of
# its own. docker/entrypoint.sh loops this once an hour
# (`while true; do soak-probe.sh; sleep 3600; done`); this script only knows
# how to run once and record the result, exactly the shape #297 asks for
# ("no second prober, no host cron").
#
# Reuses tools/probe-checks.sh (#27's HTTP-check logic, extracted) rather
# than growing a second copy of the assertions verify-integration.sh already
# has.
#
# Usage: tools/soak-probe.sh   (or, in the container, /usr/local/bin/soak-probe.sh)
#
# Env:
#   ETV_PORT                       station HTTP port (default 8409)
#   ETV_DIAG_DIR                   diagnostics dir (default /data/diag) — the
#                                   same mount ffmpeg-probe.sh's diagnostics
#                                   and entrypoint.sh's station/etv log tee
#                                   already use.
#   SOAK_PROBE_CHANNEL             sampled channel number (default 1)
#   SOAK_PROBE_MIN_COVERAGE_DAYS   required XMLTV lookahead, in days (default 7)
set -u

# shellcheck source=tools/probe-checks.sh
. "$(dirname "$0")/probe-checks.sh"

: "${ETV_PORT:=8409}"
: "${ETV_DIAG_DIR:=/data/diag}"
: "${SOAK_PROBE_CHANNEL:=1}"
: "${SOAK_PROBE_MIN_COVERAGE_DAYS:=7}"

BASE_URL="http://127.0.0.1:${ETV_PORT}"
STATION_LOG="${ETV_DIAG_DIR}/station-etv.log"
RESULT_LOG="${ETV_DIAG_DIR}/soak-probe.log"
STATE_FILE="${ETV_DIAG_DIR}/soak-probe.state"
FAILURE_DIR="${ETV_DIAG_DIR}/soak-probe-failures"
XMLTV_SAMPLE_DIR="${ETV_DIAG_DIR}/xmltv-samples"

mkdir -p "$ETV_DIAG_DIR" "$FAILURE_DIR" "$XMLTV_SAMPLE_DIR" 2>/dev/null || {
  echo "soak-probe: cannot create $ETV_DIAG_DIR — nowhere to record a result" >&2
  exit 2
}

now_iso="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"

# --- run each check, collecting name/result/message -------------------------
declare -a check_names=() check_results=() check_messages=()

record() {
  check_names+=("$1")
  check_results+=("$2")
  check_messages+=("$3")
}

# Run one probe-checks.sh assertion and record its verdict under <name>. A
# failing check's own message is what gets recorded, so this file never
# restates an assertion's wording.
run_check() {
  local name="$1"
  shift
  local msg
  if msg="$("$@")"; then
    record "$name" "pass" ""
  else
    record "$name" "fail" "$msg"
  fi
}

master_body="$(curl -fsS --max-time 10 "${BASE_URL}/channel/${SOAK_PROBE_CHANNEL}.m3u8" 2>/dev/null || true)"
run_check "master_playlist" probe_check_master_playlist "$master_body" "$SOAK_PROBE_CHANNEL"

xmltv_body="$(curl -fsS --max-time 10 "${BASE_URL}/xmltv.xml" 2>/dev/null || true)"
run_check "xmltv_wellformed" probe_check_xmltv_wellformed "$xmltv_body"

if [ -n "$xmltv_body" ]; then
  run_check "xmltv_coverage" probe_check_xmltv_coverage_all "$xmltv_body" "$SOAK_PROBE_MIN_COVERAGE_DAYS"
else
  record "xmltv_coverage" "fail" "skipped: no xmltv.xml body to check"
fi

# Zero-tolerance log scan since the previous probe. No state file yet (first
# run, or the state file was lost) means start_line=1 — the first probe scans
# the whole log rather than silently skipping whatever happened before it
# existed.
#
# end_line is snapshotted once, before the scan, and reused both to bound the
# scan and as the state file's new value. Reading "current length" a second
# time after the scan (e.g. a separate `wc -l` once done) would race the log
# still being appended between the two reads: a line written in that window
# is past what the scan covered but at-or-before the saved start_line, so it
# would be skipped by every future probe — silently defeating the
# "zero-tolerance" contract documented in probe-checks.sh. One snapshot means
# a line written after it is simply left for the next probe to see.
start_line=1
if [ -f "$STATE_FILE" ]; then
  saved="$(cat "$STATE_FILE" 2>/dev/null || true)"
  case "$saved" in
    '' | *[!0-9]*) start_line=1 ;;
    *) start_line=$((saved + 1)) ;;
  esac
fi

end_line=0
[ -f "$STATION_LOG" ] && end_line="$(wc -l <"$STATION_LOG" 2>/dev/null | tr -d ' ')"
[ -n "$end_line" ] || end_line=0

run_check "log_scan" probe_check_log_window "$STATION_LOG" "$start_line" "$end_line"

# Advance the state file to the snapshot taken above (not a fresh read),
# regardless of pass/fail, so a failure's lines are never re-reported as new
# on the next probe.
if [ "$end_line" -gt 0 ]; then
  printf '%s' "$end_line" >"$STATE_FILE"
fi

# One XMLTV sample per calendar day (UTC) — evidence for #20 without keeping
# every hourly fetch.
if [ -n "$xmltv_body" ]; then
  today="$(date -u '+%Y-%m-%d')"
  sample_path="${XMLTV_SAMPLE_DIR}/xmltv-${today}.xml"
  [ -f "$sample_path" ] || printf '%s' "$xmltv_body" >"$sample_path"
fi

# --- overall verdict + result line ------------------------------------------
overall="pass"
for r in "${check_results[@]}"; do
  [ "$r" = "fail" ] && overall="fail"
done

# One machine-readable line per probe: tab-separated timestamp, overall
# verdict, then "<check>=<pass|fail>" per check. Deliberately not JSON — a
# soak run's evidence needs to be greppable/cuttable with tools already in
# the runtime image, nothing more.
result_line="${now_iso}"$'\t'"overall=${overall}"
for i in "${!check_names[@]}"; do
  result_line="${result_line}"$'\t'"${check_names[$i]}=${check_results[$i]}"
done
printf '%s\n' "$result_line" >>"$RESULT_LOG"

if [ "$overall" = "fail" ]; then
  failure_file="${FAILURE_DIR}/${now_iso//:/-}.log"
  {
    printf 'soak-probe failure at %s\n\n' "$now_iso"
    for i in "${!check_names[@]}"; do
      [ "${check_results[$i]}" = "fail" ] || continue
      printf '[%s] %s\n' "${check_names[$i]}" "${check_messages[$i]}"
    done
    if [ -f "$STATION_LOG" ]; then
      printf '\n--- last 200 lines of %s ---\n' "$STATION_LOG"
      tail -n 200 "$STATION_LOG"
    fi
  } >"$failure_file"
fi

[ "$overall" = "pass" ]
