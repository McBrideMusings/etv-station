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
#   SOAK_PROBE_KEEP                max files kept in xmltv-samples/ (default
#                                  8). Sized for ONE FILE PER UTC DAY: the
#                                  7-day soak (#20) plus one day in flight =
#                                  8. Also the same count tools/ffmpeg-probe.sh
#                                  uses for its argv log.
#   SOAK_PROBE_FAILURE_KEEP        max files kept in soak-probe-failures/
#                                  (default 192). Sized for ONE FILE PER
#                                  FAILING PROBE at the docker/entrypoint.sh
#                                  default hourly interval (24/day): 8 days x
#                                  24 = 192, mirroring xmltv's 7-day-soak +
#                                  1-day-headroom shape. Re-derive this if
#                                  SOAK_PROBE_INTERVAL_SECS ever changes —
#                                  a shorter interval means more failures/day
#                                  and needs a bigger count to still cover a
#                                  full 7-day soak.
set -u

# shellcheck source=tools/probe-checks.sh
. "$(dirname "$0")/probe-checks.sh"

: "${ETV_PORT:=8409}"
: "${ETV_DIAG_DIR:=/data/diag}"
: "${SOAK_PROBE_CHANNEL:=1}"
: "${SOAK_PROBE_MIN_COVERAGE_DAYS:=7}"
: "${SOAK_PROBE_KEEP:=8}"
: "${SOAK_PROBE_FAILURE_KEEP:=192}"

BASE_URL="http://127.0.0.1:${ETV_PORT}"
STATION_LOG="${ETV_DIAG_DIR}/station-etv.log"
ROLLED_LOG="${STATION_LOG}.1"
RESULT_LOG="${ETV_DIAG_DIR}/soak-probe.log"
STATE_FILE="${ETV_DIAG_DIR}/soak-probe.state"
FAILURE_DIR="${ETV_DIAG_DIR}/soak-probe-failures"
XMLTV_SAMPLE_DIR="${ETV_DIAG_DIR}/xmltv-samples"

mkdir -p "$ETV_DIAG_DIR" "$FAILURE_DIR" "$XMLTV_SAMPLE_DIR" 2>/dev/null || {
  echo "soak-probe: cannot create $ETV_DIAG_DIR — nowhere to record a result" >&2
  exit 2
}

now_iso="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"

# Trim a directory to its newest $2 entries by filename (#299), same "keep
# last N" shape as tools/ffmpeg-probe.sh's argv-log cap. Both
# soak-probe-failures/ and xmltv-samples/ name files with an ISO-8601
# timestamp (xmltv-YYYY-MM-DD.xml, <RFC3339 with `:` swapped for `-`>.log),
# so plain filename sort is chronological — no need for `ls -t`/`stat`
# mtime, which is a portability trap between the container's GNU coreutils
# and a macOS dev shell. Best-effort and silent: a soak probe must never fail
# because its own housekeeping hit a permission error or a `head`/`tail`
# quirk, so every failure mode here is swallowed and this always returns 0.
# rotate_log() in docker/entrypoint.sh is copy-then-truncate, not rename — a
# long-lived awk writer holds station-etv.log open for the container's whole
# life, so the live path's inode never changes at rotation and cannot be used
# to detect one. What DOES change at every rotation is station-etv.log.1: it
# is (re)written by `cp -f f f.1` each time rotate_log fires. This prints an
# identity token for that file — mtime:size:inode — or "none" when it does
# not exist yet. With ETV_STATION_LOG_KEEP=1 the .1 inode can be reused
# across rotations (mv shuffles .1 -> .2 first only when keep > 1), so mtime
# and size ride along precisely so identity still changes when inode alone
# would not. Never aborts the probe: any stat failure (missing file, a
# platform whose stat flags differ) yields "none" under `set -u` alone (no
# -e), which the caller treats as "not yet armed" rather than a rotation.
rolled_marker() {
  local f="$1" marker
  [ -f "$f" ] || { printf 'none'; return 0; }
  marker="$(stat -c '%Y:%s:%i' "$f" 2>/dev/null || stat -f '%m:%z:%i' "$f" 2>/dev/null || true)"
  [ -n "$marker" ] && printf '%s' "$marker" || printf 'none'
}

trim_dir() {
  local dir="$1" keep="$2" count excess
  [ -d "$dir" ] || return 0
  count="$(ls -1 "$dir" 2>/dev/null | wc -l | tr -d ' ')"
  [ -n "$count" ] || return 0
  excess=$((count - keep))
  [ "$excess" -gt 0 ] || return 0
  ls -1 "$dir" 2>/dev/null | sort | head -n "$excess" | while IFS= read -r name; do
    [ -n "$name" ] && rm -f "$dir/$name" 2>/dev/null
  done
  return 0
}

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
#
# Rotation is detected from station-etv.log.1's identity (#355), not from the
# live file's line count shrinking. entrypoint.sh's rotate_log() runs on its
# own interval (default 60s), independent of this probe's schedule (default
# hourly) — a rotation followed by enough writes to regrow the live file past
# the old saved line count before the NEXT probe runs makes the shrink
# invisible, and start_line would then resume at an offset pointing into
# unrelated post-rotation content: exactly the skipped-lines gap this check
# exists to close. station-etv.log.1 is (re)written by rotate_log() at every
# rotation and never touched between rotations, so its mtime:size:inode
# marker changes if-and-only-if a rotation happened, regardless of how fast
# the live file regrows afterward.
start_line=1
saved_line=""
saved_marker=""
if [ -f "$STATE_FILE" ]; then
  read -r saved_line saved_marker <"$STATE_FILE" 2>/dev/null || true
  case "$saved_line" in
    '' | *[!0-9]*) saved_line="" ;;
    *) start_line=$((saved_line + 1)) ;;
  esac
fi

end_line=0
[ -f "$STATION_LOG" ] && end_line="$(wc -l <"$STATION_LOG" 2>/dev/null | tr -d ' ')"
[ -n "$end_line" ] || end_line=0

current_marker="$(rolled_marker "$ROLLED_LOG")"

if [ -n "$saved_marker" ]; then
  # Armed: a prior probe recorded a marker, so any change — including .1
  # appearing for the first time — is a rotation, however much the live file
  # has regrown since.
  if [ "$current_marker" != "$saved_marker" ]; then
    start_line=1
  fi
fi

# A manual truncate (no .1 written) still needs to reset the scan — belt and
# suspenders alongside the marker check above. This single unconditional
# check also covers the legacy/not-yet-armed case (an upgrade from a
# bare-integer state file, or the run right after one was written before .1
# existed yet, i.e. saved_marker=="" && saved_line!=""): falling back to the
# old shrink heuristic there is still correct unless a rotation actually
# happened, and the next probe onward is armed via the marker recorded below.
# That case used to have its own copy of this exact condition in an `elif`
# branch above — a strict subset of this unconditional one, which also
# reaches the armed case (a manual truncate that writes no .1, leaving the
# marker unchanged) that the elif copy could never reach — so only this copy
# is kept.
if [ -n "$saved_line" ] && [ "$end_line" -lt "$saved_line" ]; then
  start_line=1
fi

run_check "log_scan" probe_check_log_window "$STATION_LOG" "$start_line" "$end_line"

# Advance the state file to the snapshot taken above (not a fresh read),
# regardless of pass/fail, so a failure's lines are never re-reported as new
# on the next probe. The marker is always written (even when end_line is 0,
# e.g. probing right after a rotation truncated the live file to empty) so
# the NEXT probe is armed to detect a rotation from this run onward; only the
# line count is gated on end_line>0 to avoid recording a bogus "0" over a
# real prior count on a transient empty read.
if [ "$end_line" -gt 0 ]; then
  printf '%s %s\n' "$end_line" "$current_marker" >"$STATE_FILE"
else
  printf '%s %s\n' "${saved_line:-0}" "$current_marker" >"$STATE_FILE"
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

# Trim both retained-artifact dirs every run (#299), not only on a run that
# writes to them — an already-oversized dir (e.g. SOAK_PROBE_KEEP or
# SOAK_PROBE_FAILURE_KEEP lowered after the fact) still shrinks back down.
# The two dirs fill at very different rates (one xmltv sample per day vs. up
# to one failure per hourly probe, #356), so each gets its own count rather
# than sharing SOAK_PROBE_KEEP.
trim_dir "$FAILURE_DIR" "$SOAK_PROBE_FAILURE_KEEP"
trim_dir "$XMLTV_SAMPLE_DIR" "$SOAK_PROBE_KEEP"

[ "$overall" = "pass" ]
