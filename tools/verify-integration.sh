#!/usr/bin/env bash
# HTTP smoke test for etv-station + etv-next integration.
#
# Starts ./tools/dev-run.sh in the background, probes its HTTP endpoints,
# verifies HLS playout, XMLTV, and process cleanup. Exits non-zero on any failure.
#
# Usage:
#   tools/verify-integration.sh
set -u
set -m

# shellcheck source=tools/dev-procs.sh
. "$(dirname "$0")/dev-procs.sh"
# shellcheck source=tools/probe-checks.sh
. "$(dirname "$0")/probe-checks.sh"

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

: "${ETV_PORT:=8409}"
: "${ETV_BIND_ADDRESS:=0.0.0.0}"
export ETV_BIND_ADDRESS ETV_PORT

BASE_URL="http://127.0.0.1:${ETV_PORT}"
LOG_FILE="tmp/verify-integration.log"
SESSION_WAIT_SECS=30
SEGMENT_WAIT_SECS=10

# Use a minimal test configuration that doesn't require external resources
: "${STATION_CONFIG:=examples/station-test.yaml}"
export STATION_CONFIG

red()    { printf '\033[31m%s\033[0m' "$1"; }
green()  { printf '\033[32m%s\033[0m' "$1"; }
yellow() { printf '\033[33m%s\033[0m' "$1"; }
bold()   { printf '\033[1m%s\033[0m' "$1"; }

failures=0
declare -a failure_msgs=()

fail() {
  local msg="$*"
  failures=$((failures + 1))
  failure_msgs+=("$msg")
  printf '%s %s\n' "$(red 'FAIL')" "$msg" >&2
}

pass() { printf '%s %s\n' "$(green 'PASS')" "$*"; }
warn() { printf '%s %s\n' "$(yellow 'WARN')" "$*"; }

# Run a probe-checks.sh assertion: PASS with <label>, or FAIL with the check's
# own message and abort. Every probe_check_* call in this script goes through
# here so the pass/fail/exit shape is written once.
check_or_die() {
  local label="$1"
  shift
  local msg
  if msg="$("$@")"; then
    pass "$label"
  else
    fail "$msg"
    exit 1
  fi
}

require() {
  command -v "$1" >/dev/null 2>&1 || {
    printf '%s required tool not found: %s\n' "$(red 'fatal:')" "$1" >&2
    exit 2
  }
}

# Cleanup: kill the dev-run process group and check for orphans.
cleanup() {
  trap - EXIT INT TERM HUP
  local pids
  pids=$(jobs -p)
  if [ -n "$pids" ]; then
    for pid in $pids; do
      kill -TERM -- "-$pid" 2>/dev/null || true
    done
    sleep 1
    for pid in $pids; do
      kill -KILL -- "-$pid" 2>/dev/null || true
    done
  fi
}

trap cleanup EXIT INT TERM HUP

require curl
require xmllint

mkdir -p tmp

printf '%s verifying integration at %s\n' "$(bold '==>')" "$BASE_URL"

# Start dev-run.sh in background with OPEN_IINA=0 to suppress window open.
# Capture combined output to a log file for diagnostics.
printf 'launching dev stack...\n'
OPEN_IINA=0 ./tools/dev-run.sh >"$LOG_FILE" 2>&1 &
dev_run_pid=$!

# Wait for /channels.m3u to return 200 (bounded, up to SESSION_WAIT_SECS).
printf 'waiting for lineup...\n'
waited=0
while ! curl -fsS -o /dev/null --max-time 2 "$BASE_URL/channels.m3u"; do
  sleep 1
  waited=$((waited + 1))
  if [ "$waited" -ge "$SESSION_WAIT_SECS" ]; then
    fail "lineup endpoint did not return 200 within ${SESSION_WAIT_SECS}s"
    exit 1
  fi
done
pass "lineup endpoint ready (${waited}s)"

# Fetch lineup and assert it contains channel/1.m3u8.
printf 'verifying lineup content...\n'
lineup="$(curl -fsS "$BASE_URL/channels.m3u" || true)"
if [ -z "$lineup" ]; then
  fail "could not fetch $BASE_URL/channels.m3u"
  exit 1
fi

if ! printf '%s\n' "$lineup" | grep -q "channel/1.m3u8"; then
  fail "lineup does not contain channel/1.m3u8"
  exit 1
fi
pass "lineup contains channel/1.m3u8"

# Fetch /channel/1.m3u8 — assert 200, starts with #EXTM3U, contains /session/1/live.m3u8.
printf 'verifying channel master playlist...\n'
master_body="$(curl -fsS "$BASE_URL/channel/1.m3u8" || true)"
check_or_die "master playlist starts with #EXTM3U and contains /session/1/live.m3u8" \
  probe_check_master_playlist "$master_body" 1

# Wait for ffmpeg segment ramp-up, then fetch live.m3u8 and latest segment.
printf 'waiting for ffmpeg segment production...\n'
sleep "$SEGMENT_WAIT_SECS"

printf 'fetching live playlist...\n'
live_body="$(curl -fsS "$BASE_URL/session/1/live.m3u8" || true)"
if [ -z "$live_body" ]; then
  fail "live.m3u8 did not return a body"
  exit 1
fi

# Extract the latest .ts segment filename from the playlist.
latest_seg="$(printf '%s\n' "$live_body" | awk '/^[^#].+\.ts$/ { seg=$0 } END { print seg }')"
if [ -z "$latest_seg" ]; then
  fail "no .ts segment found in live playlist"
  exit 1
fi
pass "live playlist contains segment $latest_seg"

# Fetch the latest segment and check it is non-empty.
printf 'fetching ts segment...\n'
seg_url="$BASE_URL/session/1/$latest_seg"
tmpseg="$(mktemp -t etv-seg.XXXXXX.ts)"
if ! curl -fsS "$seg_url" -o "$tmpseg"; then
  fail "could not fetch segment $latest_seg"
  rm -f "$tmpseg"
  exit 1
fi

seg_bytes="$(wc -c < "$tmpseg" | tr -d ' ')"
if [ "$seg_bytes" -le 0 ]; then
  fail "segment $latest_seg is empty"
  rm -f "$tmpseg"
  exit 1
fi
pass "segment ${latest_seg} (${seg_bytes}B) is valid"
rm -f "$tmpseg"

# Fetch XMLTV and validate it is well-formed XML containing a channel element.
printf 'verifying XMLTV...\n'
xmltv_body="$(curl -fsS "$BASE_URL/xmltv.xml" || true)"
if [ -z "$xmltv_body" ]; then
  fail "xmltv.xml did not return a body"
  exit 1
fi

# Validate XML structure.
check_or_die "xmltv.xml is valid XML" probe_check_xmltv_wellformed "$xmltv_body"

# Check for <channel id= element.
check_or_die "xmltv.xml contains <channel id= element" \
  probe_check_xmltv_channel_present "$xmltv_body"

# Send SIGINT to dev-run process group and wait for clean exit.
printf 'shutting down dev stack...\n'
kill -TERM -- "-$dev_run_pid" 2>/dev/null || true
wait $dev_run_pid 2>/dev/null || true

# Check for orphaned dev processes.
printf 'verifying no orphaned processes...\n'
orphans=()
for entry in "${DEV_PROCS[@]}"; do
  IFS='|' read -r label kind pattern <<< "$entry"
  while IFS= read -r pid; do
    [ -n "$pid" ] || continue
    # Check if process is actually running (not a zombie that hasn't been reaped).
    if kill -0 "$pid" 2>/dev/null; then
      orphans+=("$pid ($label)")
    fi
  done <<< "$(dev_proc_pids "$kind" "$pattern")"
done

if [ "${#orphans[@]}" -gt 0 ]; then
  fail "${#orphans[@]} orphaned process(es) remain after shutdown:"
  printf '  pid %s\n' "${orphans[@]}" >&2
else
  pass "no orphaned processes"
fi

# Check for stale temp files in examples/output/test/
printf 'verifying no stale temp files...\n'
if [ -d "examples/output/test" ]; then
  stale_files=()
  while IFS= read -r f; do
    stale_files+=("$f")
  done < <(find "examples/output/test" -name '.*.tmp.*' 2>/dev/null || true)

  if [ "${#stale_files[@]}" -gt 0 ]; then
    fail "${#stale_files[@]} stale temp file(s) in examples/output/test/:"
    printf '  %s\n' "${stale_files[@]}" >&2
  else
    pass "no stale temp files in examples/output/test/"
  fi
fi

# Summary.
if [ "$failures" -eq 0 ]; then
  printf '\n%s integration verification passed\n' "$(green 'OK')"
  exit 0
fi

printf '\n%s %d failure(s):\n' "$(red 'FAIL')" "$failures"
for m in "${failure_msgs[@]}"; do
  printf '  - %s\n' "$m"
done
exit 1
