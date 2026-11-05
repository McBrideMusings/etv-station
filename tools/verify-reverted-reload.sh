#!/usr/bin/env bash
# Drive a real reverted reload against a live daemon and read the outcome back.
#
# A SIGHUP reload has an outcome the daemon records in `.etv-station.reload.json`
# beside its pid file (crates/etv-station/src/reload_status.rs). "reverted" is
# the one only unit tests had covered: the new config PARSES (so it is neither
# `failed` nor refused by the dashboard's loader check) but `prepare_generation`
# cannot set it up, and the daemon puts the last-known-good config back. The
# generation that respawns that config must not overwrite the outcome with
# "applied" (`reverted_just_now`, crates/etv-station/src/daemon.rs).
#
# This script copies examples/station-test.yaml and its channel to a scratch
# folder, starts `etv-station` on the copy, renames the channel to a name whose
# output folder is a regular file (create_dir_all then fails), SIGHUPs the
# daemon, and asserts:
#   1. the status file says outcome=reverted with a non-empty error, seq > 0
#   2. the daemon is still running and logged config.reload_reverted, never
#      config.reload_failed
#   3. a few seconds later the status is still reverted, same seq (the respawn
#      did not rewrite it as applied)
#   4. the last-known-good channel respawns and publishes (playout.first_emit)
#   5. restoring the config and SIGHUPing again records applied with a higher seq
#
# Usage: tools/verify-reverted-reload.sh     (run from the repo root)
# Exit 0 on pass. Leaves no process it started; the scratch folder stays under
# /private/tmp/claude/etv-station/revert for inspection.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
S=/private/tmp/claude/etv-station/revert
BIN="$ROOT/target/debug/etv-station"
STATUS="$S/out/.etv-station.reload.json"
LOG="$S/daemon.log"
DAEMON_PID=""
failures=0

pass() { printf 'PASS %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; failures=$((failures + 1)); }

cleanup() {
  if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID" 2>/dev/null
    for _ in $(seq 1 60); do
      kill -0 "$DAEMON_PID" 2>/dev/null || break
      sleep 1
    done
    pkill -9 -P "$DAEMON_PID" 2>/dev/null
    kill -9 "$DAEMON_PID" 2>/dev/null
  fi
}
trap cleanup EXIT

strip() { perl -pe 's/\e\[[0-9;]*m//g' "$LOG"; }

field() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2]))' "$STATUS" "$1" 2>/dev/null; }

wait_status() { # wait_status <seq greater than> <seconds>
  for _ in $(seq 1 "$2"); do
    if [ -f "$STATUS" ] && [ "$(field seq)" -gt "$1" ] 2>/dev/null; then return 0; fi
    sleep 1
  done
  return 1
}

# The binary under test is the one built from this checkout, so build it here.
( cd "$ROOT" && CARGO_BUILD_JOBS="${VERIFY_JOBS:-4}" nice -n 10 cargo build -p etv-station 2>&1 | tail -1 ) || exit 2
[ -x "$BIN" ] || { echo "no $BIN after the build" >&2; exit 2; }

rm -rf "$S"
mkdir -p "$S/channels" "$S/out"
sed -e "s|^output_base: .*|output_base: $S/out|" \
    -e "s|^catalog_path: .*|catalog_path: $S/catalog.db|" \
    "$ROOT/examples/station-test.yaml" > "$S/station.yaml"
sed -e "s|\"../../crates/etv-overlay|\"$ROOT/crates/etv-overlay|" \
    "$ROOT/examples/channels/lavfi-test.yaml" > "$S/channels/lavfi-test.yaml"

env -u PLEX_URL -u PLEX_TOKEN -u TAUTULLI_URL -u TAUTULLI_API_KEY "$BIN" --config "$S/station.yaml" > "$LOG" 2>&1 &
DAEMON_PID=$!

for _ in $(seq 1 60); do
  [ -f "$S/out/.etv-station.pid" ] && [ -d "$S/out/test" ] && break
  sleep 1
done
if [ -f "$S/out/.etv-station.pid" ] && [ -d "$S/out/test" ]; then
  pass "daemon is up (pid $DAEMON_PID), channel folder $S/out/test exists"
else
  fail "daemon did not come up in 60s; see $LOG"
  exit 1
fi
sleep 3

# A name whose output folder is a regular file: parses, cannot be prepared.
: > "$S/out/blocker"
cp "$S/channels/lavfi-test.yaml" "$S/channels/lavfi-test.yaml.good"
perl -pi -e 's/^name: test$/name: blocker/' "$S/channels/lavfi-test.yaml"
grep -q '^name: blocker$' "$S/channels/lavfi-test.yaml" || { fail "could not edit the channel name"; exit 1; }

before=0
[ -f "$STATUS" ] && before="$(field seq)"
kill -HUP "$DAEMON_PID"
if wait_status "$before" 120; then
  outcome="$(field outcome)"; seq1="$(field seq)"; err="$(field error)"
  if [ "$outcome" = reverted ] && printf '%s' "$err" | grep -q "$S/out/blocker"; then
    pass "status is outcome=reverted seq=$seq1 error=\"$err\""
  else
    fail "expected outcome=reverted with an error naming $S/out/blocker, got outcome=$outcome error=$err"
  fi
else
  fail "no status written within 120s of SIGHUP; see $LOG"
  seq1=0
fi

# The respawned last-known-good channel publishes its window after the revert line.
respawned=0
for _ in $(seq 1 120); do
  if strip | sed -n '/config.reload_reverted/,$p' | grep -q 'event="playout.first_emit" channel=test'; then respawned=1; break; fi
  sleep 1
done
sleep 2
if kill -0 "$DAEMON_PID" 2>/dev/null; then pass "daemon still running after the revert"; else fail "daemon exited"; fi
if grep -q 'config.reload_reverted' "$LOG"; then pass "log has config.reload_reverted"; else fail "no config.reload_reverted in $LOG"; fi
if grep -q 'config.reload_failed' "$LOG"; then fail "log has config.reload_failed (parse failure, not a revert)"; else pass "no config.reload_failed in the log"; fi
if [ "$(field outcome)" = reverted ] && [ "$(field seq)" = "$seq1" ]; then
  pass "status still reverted at seq $seq1 after the respawn (not overwritten by applied)"
else
  fail "status changed after the respawn: outcome=$(field outcome) seq=$(field seq), wanted reverted seq=$seq1"
fi
# The last-known-good channel respawns right after the revert line and writes its window.
if [ "$respawned" = 1 ]; then
  pass "last-known-good channel 'test' respawned and published after the revert"
else
  fail "no playout.first_emit for channel test after config.reload_reverted"
fi

cp "$S/channels/lavfi-test.yaml.good" "$S/channels/lavfi-test.yaml"
kill -HUP "$DAEMON_PID"
if wait_status "$seq1" 120 && [ "$(field outcome)" = applied ]; then
  pass "restored config recorded outcome=applied seq=$(field seq)"
else
  fail "restoring the config did not record applied: outcome=$(field outcome) seq=$(field seq)"
fi

echo
if [ "$failures" -eq 0 ]; then echo "reverted reload verified"; else echo "$failures check(s) failed; log: $LOG"; fi
exit "$failures"
