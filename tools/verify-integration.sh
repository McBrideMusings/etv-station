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

# `etv-station --serve-ui` port. Defaults to 4747, the container's; a swarm
# worker sets its own beside ETV_PORT.
: "${ETV_UI_PORT:=4747}"
UI_URL="http://127.0.0.1:${ETV_UI_PORT}"
UI_LOG_FILE="tmp/verify-integration-ui.log"

# The test config needs nothing external. dev-run.sh re-sources .env itself, so
# this flag, not an unset here, keeps a Plex URL from starting a full library
# ingest before the station rolls its first channel.
export ETV_DEV_NO_EXTERNAL=1

BASE_URL="http://127.0.0.1:${ETV_PORT}"
LOG_FILE="tmp/verify-integration.log"
BUILD_LOG="tmp/verify-integration-build.log"
LINEUP_WAIT_SECS=120
# Backstop for a dev-run that hangs; dev-run bounds its own waits well inside it.
LAUNCH_WAIT_SECS=900
CLEANUP_GRACE_SECS=10
MASTER_WAIT_SECS=120
SEGMENT_WAIT_SECS=10
# The web UI answers one request at a time, so a queued review can hold a health
# probe for as long as the review takes; --max-time sits well above that.
UI_WAIT_SECS=60
UI_REQUEST_SECS=30

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

# Cleanup: TERM the dev-run process group and give dev-run its own teardown
# (it TERMs, waits 1s, then KILLs each child group) time to finish before
# escalating. Killing sooner would take dev-run down mid-teardown and leave
# its children, such as a station that ignores TERM, running.
cleanup() {
  trap - EXIT INT TERM HUP
  local pids pid waited=0
  pids=$(jobs -p)
  [ -z "$pids" ] && return
  for pid in $pids; do
    kill -TERM -- "-$pid" 2>/dev/null || true
  done
  while [ "$waited" -lt "$CLEANUP_GRACE_SECS" ]; do
    local alive=0
    for pid in $pids; do
      kill -0 "$pid" 2>/dev/null && alive=1
    done
    [ "$alive" -eq 0 ] && break
    sleep 1
    waited=$((waited + 1))
  done
  for pid in $pids; do
    kill -KILL -- "-$pid" 2>/dev/null || true
  done
}

trap cleanup EXIT INT TERM HUP

require curl
require xmllint

mkdir -p tmp

printf '%s verifying integration at %s\n' "$(bold '==>')" "$BASE_URL"

# Build every binary the stack runs before the clock starts. A cold cargo build,
# or a wait on cargo's package-cache lock, can take minutes; none of it belongs
# in the lineup deadline.
printf 'building dev stack binaries...\n'
# One cargo invocation per package, as dev-run.sh runs them: a combined
# invocation unifies features differently and leaves dev-run rebuilding.
if ! cargo build -q -p etv-overlay >"$BUILD_LOG" 2>&1 \
  || ! cargo build -q -p etv-station --bin etv-station >>"$BUILD_LOG" 2>&1 \
  || ! cargo build -q --manifest-path vendor/etv-next/Cargo.toml \
    --bin ersatztv --bin ersatztv-channel >>"$BUILD_LOG" 2>&1; then
  fail "building the dev stack binaries failed; last lines of $BUILD_LOG:"
  tail -20 "$BUILD_LOG" >&2
  exit 1
fi

# Start dev-run.sh in background with OPEN_IINA=0 to suppress window open.
# Capture combined output to a log file for diagnostics.
printf 'launching dev stack...\n'
OPEN_IINA=0 ./tools/dev-run.sh >"$LOG_FILE" 2>&1 &
dev_run_pid=$!
launched_at=$SECONDS

# The lineup cannot answer until dev-run.sh launches etv-next, which it does
# after the station's startup catch-up (dev-run.sh wait_for_first_emit, bounded
# there). That catch-up scales with machine load, so the lineup deadline counts
# from dev-run's "launching etv-next" line, not from launch. A dev-run that
# exited fails at once instead of running out a deadline. A process-group
# failure is bash's setpgid refusal; it is harmless while the stack comes up, so
# it is named as the likely cause only when the lineup does not.
printf 'waiting for dev-run to launch etv-next...\n'
while ! grep -qF '[dev] launching etv-next' "$LOG_FILE" 2>/dev/null; do
  if ! kill -0 "$dev_run_pid" 2>/dev/null; then
    fail "dev-run exited before launching etv-next; last lines of $LOG_FILE:"
    tail -20 "$LOG_FILE" >&2
    exit 1
  fi
  if [ $((SECONDS - launched_at)) -ge "$LAUNCH_WAIT_SECS" ]; then
    fail "dev-run did not launch etv-next within ${LAUNCH_WAIT_SECS}s"
    exit 1
  fi
  sleep 1
done
pass "dev-run launched etv-next ($((SECONDS - launched_at))s after launch)"
launched_at=$SECONDS

printf 'waiting for lineup...\n'
while ! curl -fs -o /dev/null --max-time 2 "$BASE_URL/channels.m3u"; do
  if ! kill -0 "$dev_run_pid" 2>/dev/null; then
    fail "dev-run exited before the lineup came up; last lines of $LOG_FILE:"
    if grep -q 'setpgid.*Operation not permitted' "$LOG_FILE"; then
      fail "launch error: $(grep -m1 'setpgid.*Operation not permitted' "$LOG_FILE")"
    fi
    tail -20 "$LOG_FILE" >&2
    exit 1
  fi
  if [ $((SECONDS - launched_at)) -ge "$LINEUP_WAIT_SECS" ]; then
    fail "lineup endpoint did not return 200 within ${LINEUP_WAIT_SECS}s of etv-next's launch"
    if grep -q 'setpgid.*Operation not permitted' "$LOG_FILE"; then
      fail "launch error: $(grep -m1 'setpgid.*Operation not permitted' "$LOG_FILE")"
    fi
    exit 1
  fi
  sleep 1
done
pass "lineup endpoint ready ($((SECONDS - launched_at))s after etv-next launch)"

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
# ETV-next answers 503 until the channel's first ffmpeg run is up; seeking into
# a lavfi source on a busy machine takes longer than its 30s answer window, so
# retry until a deadline rather than judging the first attempt.
master_body=""
master_started=$SECONDS
while [ -z "$master_body" ] && [ $((SECONDS - master_started)) -lt "$MASTER_WAIT_SECS" ]; do
  if ! kill -0 "$dev_run_pid" 2>/dev/null; then
    fail "dev-run exited before channel 1 answered; last lines of $LOG_FILE:"
    tail -20 "$LOG_FILE" >&2
    exit 1
  fi
  master_body="$(curl -fs --max-time 40 "$BASE_URL/channel/1.m3u8" || true)"
  [ -n "$master_body" ] || sleep 1
done
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

# The web UI is not part of dev-run.sh's stack, so start it here against the same
# config and probe /api/health: 200 with "ok":true and a tunable_channels count.
printf 'verifying web UI health...\n'
cargo run -q -p etv-station --bin etv-station -- \
  --config "$STATION_CONFIG" --serve-ui --ui-port "$ETV_UI_PORT" >"$UI_LOG_FILE" 2>&1 &
ui_pid=$!
ui_body=""
ui_started=$SECONDS
while [ -z "$ui_body" ] && [ $((SECONDS - ui_started)) -lt "$UI_WAIT_SECS" ]; do
  if ! kill -0 "$ui_pid" 2>/dev/null; then
    fail "web UI exited before answering /api/health; last lines of $UI_LOG_FILE:"
    tail -20 "$UI_LOG_FILE" >&2
    exit 1
  fi
  ui_body="$(curl -fs --max-time "$UI_REQUEST_SECS" "$UI_URL/api/health" || true)"
  [ -n "$ui_body" ] || sleep 1
done
check_or_die "web UI /api/health is ok and reports tunable_channels" \
  probe_check_ui_health "$ui_body"

# Stop the UI. The orphan check below skips --serve-ui processes (a dashboard on
# another port is not part of the dev stack), so this UI is checked by its own
# port instead.
kill -TERM -- "-$ui_pid" 2>/dev/null || true
wait $ui_pid 2>/dev/null || true
if [ -n "$(pgrep -f -- "--serve-ui.*--ui-port $ETV_UI_PORT" 2>/dev/null || true)" ]; then
  fail "web UI on port $ETV_UI_PORT still running after SIGTERM"
fi

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
