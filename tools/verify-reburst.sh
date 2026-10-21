#!/usr/bin/env bash
# Drive one reburst end to end on the no-media dev stack and read its log line back.
#
# A reburst is ETV-next restarting a channel's ffmpeg mid-item because the segment
# lead (frontier minus wall clock) fell to REBURST_AT_LEAD (20s) after having
# reached REBURST_ARM_AT_LEAD (24s); vendor/etv-next/crates/ersatztv-channel/src/
# channel_session.rs:reburst_decision. Nothing in normal local playback drains the
# lead that far, so this script does it: it starts the stack on examples/station-
# reburst.yaml (one looping 300s lavfi item), watches channel 1, waits for the
# channel worker's "reburst check" debug line to report armed, then freezes the
# worker's ffmpeg with SIGSTOP until the lead runs down and the worker kills and
# respawns it.
#
# ETV_INITIAL_BURST_SECS=60 is what makes the arming dependable. At the 28s
# default the lead peaks at 28 minus the seconds ffmpeg takes to emit its first
# segment, which on a busy machine is under the 24s arm point, so no freeze
# schedule can fire a reburst.
#
# Passes (exit 0) when the ETV-next log holds `reburst: lead exhausted` and the
# live playlist kept advancing afterward. Leaves no process it started.
#
# Usage:
#   tools/verify-reburst.sh              # ETV_PORT defaults to 8409
#   ETV_PORT=23448 tools/verify-reburst.sh
set -u
set -m

# shellcheck source=tools/dev-procs.sh
. "$(dirname "$0")/dev-procs.sh"

: "${ETV_PORT:=8409}"
: "${ETV_BIND_ADDRESS:=0.0.0.0}"
export ETV_PORT ETV_BIND_ADDRESS

# The channel worker reads this in channel_session.rs:initial_burst.
: "${ETV_INITIAL_BURST_SECS:=60}"
export ETV_INITIAL_BURST_SECS
export STATION_CONFIG=examples/station-reburst.yaml
export ETV_NEXT_DIR=tmp/reburst-etv-next
export ETV_DEV_NO_EXTERNAL=1
export OPEN_IINA=0

BASE_URL="http://127.0.0.1:${ETV_PORT}"
LOG_FILE="tmp/verify-reburst.log"
LAUNCH_WAIT_SECS=900
LINEUP_WAIT_SECS=120
MASTER_WAIT_SECS=120
# How long to wait for a transcode that is armed with enough of the item left.
: "${ARMED_WAIT_SECS:=240}"
# Unarmed lead must reach this many seconds past the fire point for a freeze to
# be worth starting, and the item must have this many seconds left to restart
# into (the worker holds fire under 20s remaining).
MIN_REMAINING_SECS=100
# The worker tears the session down after 60s without a segment.
FREEZE_MAX_SECS=50
SETTLE_SECS=15
CLEANUP_GRACE_SECS=10

failures=0
pass() { printf 'PASS %s\n' "$*"; }
fail() { failures=$((failures + 1)); printf 'FAIL %s\n' "$*" >&2; }

viewer_pid=""
frozen_pid=""

cleanup() {
  trap - EXIT INT TERM HUP
  local pids pid waited=0
  [ -n "$frozen_pid" ] && kill -CONT "$frozen_pid" 2>/dev/null
  [ -n "$viewer_pid" ] && kill -TERM "$viewer_pid" 2>/dev/null
  pids=$(jobs -p)
  [ -z "$pids" ] && return
  for pid in $pids; do kill -TERM -- "-$pid" 2>/dev/null || true; done
  while [ "$waited" -lt "$CLEANUP_GRACE_SECS" ]; do
    local alive=0
    for pid in $pids; do kill -0 "$pid" 2>/dev/null && alive=1; done
    [ "$alive" -eq 0 ] && break
    sleep 1
    waited=$((waited + 1))
  done
  for pid in $pids; do kill -KILL -- "-$pid" 2>/dev/null || true; done
}
trap cleanup EXIT INT TERM HUP

command -v curl >/dev/null 2>&1 || { echo "fatal: curl not found" >&2; exit 2; }

mkdir -p tmp
# A fresh output folder, so the first playout window starts from this run.
rm -rf examples/output-reburst "$ETV_NEXT_DIR" tmp/catalog-reburst.db

printf '==> driving a reburst at %s\n' "$BASE_URL"

OPEN_IINA=0 ./tools/dev-run.sh >"$LOG_FILE" 2>&1 &
dev_run_pid=$!
launched_at=$SECONDS

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

launched_at=$SECONDS
while ! curl -fs -o /dev/null --max-time 2 "$BASE_URL/channels.m3u"; do
  if ! kill -0 "$dev_run_pid" 2>/dev/null || [ $((SECONDS - launched_at)) -ge "$LINEUP_WAIT_SECS" ]; then
    fail "lineup did not come up; last lines of $LOG_FILE:"
    tail -20 "$LOG_FILE" >&2
    exit 1
  fi
  sleep 1
done
pass "lineup ready"

# Tuning in starts the channel worker; ETV-next answers 503 until its first
# ffmpeg run is up, so retry rather than judge the first attempt.
master_body=""
master_started=$SECONDS
while [ -z "$master_body" ] && [ $((SECONDS - master_started)) -lt "$MASTER_WAIT_SECS" ]; do
  master_body="$(curl -fs --max-time 40 "$BASE_URL/channel/1.m3u8" || true)"
  [ -n "$master_body" ] || sleep 1
done
if [ -z "$master_body" ]; then
  fail "channel 1 did not answer within ${MASTER_WAIT_SECS}s"
  exit 1
fi
pass "channel 1 answered"

# A viewer: the worker ends its session when nothing touches the playlist. Its
# own process, so the freeze below does not stop it.
(
  while :; do
    curl -fs -o /dev/null --max-time 5 "$BASE_URL/session/1/live.m3u8"
    sleep 2
  done
) &
viewer_pid=$!

# The worker logs "reburst check: lead Ns, remaining Ns, armed B, fire B" at debug
# when a transcode arms (and when it fires). Take the newest arming line that has
# enough of the item left.
armed_lead=""
armed_started=$SECONDS
printf 'waiting for an armed lead...\n'
while [ -z "$armed_lead" ] && [ $((SECONDS - armed_started)) -lt "$ARMED_WAIT_SECS" ]; do
  armed_lead="$(grep -a 'reburst check:' "$LOG_FILE" | tail -1 \
    | sed -n 's/.*lead \(-\{0,1\}[0-9]*\)s, remaining \([0-9]*\)s, armed true.*/\1 \2/p' \
    | awk -v min="$MIN_REMAINING_SECS" '$2 >= min { print $1 }')"
  [ -n "$armed_lead" ] || sleep 1
done
if [ -z "$armed_lead" ]; then
  fail "no armed reburst check with >= ${MIN_REMAINING_SECS}s remaining within ${ARMED_WAIT_SECS}s; newest checks:"
  grep -a 'reburst check:' "$LOG_FILE" | tail -5 >&2
  exit 1
fi
pass "armed at lead ${armed_lead}s"

ffmpeg_pid="$(pgrep -f "ffmpeg.*${ETV_NEXT_DIR}/tmp/hls" | head -1)"
if [ -z "$ffmpeg_pid" ]; then
  fail "no ffmpeg process writing under ${ETV_NEXT_DIR}/tmp/hls"
  exit 1
fi

# Freeze until the lead has run down past the 20s fire point, plus a margin.
freeze_secs=$((armed_lead - 20 + 8))
[ "$freeze_secs" -gt "$FREEZE_MAX_SECS" ] && freeze_secs=$FREEZE_MAX_SECS
[ "$freeze_secs" -lt 10 ] && freeze_secs=10
printf 'freezing ffmpeg pid %s with SIGSTOP for up to %ss...\n' "$ffmpeg_pid" "$freeze_secs"
# Only a reburst logged after the freeze counts as caused by it.
log_lines_before=$(wc -l < "$LOG_FILE")
kill -STOP "$ffmpeg_pid"
frozen_pid="$ffmpeg_pid"
froze_at=$SECONDS
fired=0
while [ $((SECONDS - froze_at)) -lt "$freeze_secs" ]; do
  if tail -n +"$((log_lines_before + 1))" "$LOG_FILE" | grep -aqF 'reburst: lead exhausted'; then fired=1; break; fi
  sleep 1
done
kill -CONT "$ffmpeg_pid" 2>/dev/null
frozen_pid=""

if [ "$fired" -eq 0 ] && tail -n +"$((log_lines_before + 1))" "$LOG_FILE" | grep -aqF 'reburst: lead exhausted'; then fired=1; fi
if [ "$fired" -eq 0 ]; then
  fail "no 'reburst: lead exhausted' line after a ${freeze_secs}s freeze; newest checks:"
  grep -a 'reburst check:' "$LOG_FILE" | tail -5 >&2
  exit 1
fi
reburst_line="$(tail -n +"$((log_lines_before + 1))" "$LOG_FILE" | grep -a 'reburst: lead exhausted' | head -1)"
pass "reburst logged: ${reburst_line#*] }"

# The channel must keep serving: the live playlist's newest segment advances.
newest_segment() {
  curl -fs --max-time 5 "$BASE_URL/session/1/live.m3u8" \
    | awk '/^[^#].+\.ts$/ { seg=$0 } END { print seg }'
}
sleep 5
before="$(newest_segment)"
sleep "$SETTLE_SECS"
after="$(newest_segment)"
if [ -z "$after" ] || [ "$after" = "$before" ]; then
  fail "live playlist did not advance after the reburst (before: ${before:-none}, after: ${after:-none})"
else
  seg_bytes="$(curl -fs --max-time 10 "$BASE_URL/session/1/$after" | wc -c | tr -d ' ')"
  if [ "${seg_bytes:-0}" -gt 0 ]; then
    pass "live playlist advanced ${before:-none} -> ${after} (${seg_bytes}B)"
  else
    fail "segment $after is empty"
  fi
fi
if grep -aq 'channel 1 terminated\|channel 1 exited' "$LOG_FILE"; then
  fail "channel 1's worker ended during the run: $(grep -a 'channel 1 terminated\|channel 1 exited' "$LOG_FILE" | head -1)"
fi

printf 'shutting down dev stack...\n'
kill -TERM "$viewer_pid" 2>/dev/null
viewer_pid=""
kill -TERM -- "-$dev_run_pid" 2>/dev/null || true
wait "$dev_run_pid" 2>/dev/null || true

orphans=()
for entry in "${DEV_PROCS[@]}"; do
  IFS='|' read -r label kind pattern <<< "$entry"
  while IFS= read -r pid; do
    [ -n "$pid" ] || continue
    kill -0 "$pid" 2>/dev/null && orphans+=("$pid ($label)")
  done <<< "$(dev_proc_pids "$kind" "$pattern")"
done
if [ "${#orphans[@]}" -gt 0 ]; then
  fail "${#orphans[@]} process(es) remain after shutdown: ${orphans[*]}"
else
  pass "no processes left behind"
fi

if [ "$failures" -eq 0 ]; then
  printf '\nOK reburst verification passed\n'
  exit 0
fi
printf '\nFAIL %d failure(s)\n' "$failures" >&2
exit 1
