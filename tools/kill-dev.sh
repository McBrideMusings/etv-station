#!/usr/bin/env bash
# Kill all processes started by ./tools/dev-run.sh.
# Usage:
#   ./tools/kill-dev.sh          # SIGTERM
#   FORCE=1 ./tools/kill-dev.sh  # SIGKILL (for stuck processes)
set -u

# shellcheck source=tools/dev-procs.sh
. "$(dirname "$0")/dev-procs.sh"

FORCE="${FORCE:-0}"
SIG=$( [ "$FORCE" -eq 1 ] && echo "-KILL" || echo "-TERM" )

bold()   { printf '\033[1m%s\033[0m' "$1"; }
green()  { printf '\033[32m%s\033[0m' "$1"; }
yellow() { printf '\033[33m%s\033[0m' "$1"; }

killed=0

kill_procs() {
  local label="$1" kind="$2" pattern="$3"
  local pids
  pids=$(dev_proc_pids "$kind" "$pattern")
  if [ -n "$pids" ]; then
    # shellcheck disable=SC2086
    echo "$pids" | xargs kill $SIG 2>/dev/null || true
    printf '  %s %s (pid %s)\n' "$(yellow 'killed')" "$label" "$(echo "$pids" | tr '\n' ' ' | sed 's/ $//')"
    killed=$((killed + $(echo "$pids" | wc -l | tr -d ' ')))
  fi
}

printf '%s stopping dev processes (signal: %s)\n' "$(bold '==>')" "$SIG"

for entry in "${DEV_PROCS[@]}"; do
  IFS='|' read -r label kind pattern <<< "$entry"
  kill_procs "$label" "$kind" "$pattern"
done

if [ "$killed" -eq 0 ]; then
  printf '%s no dev processes found\n' "$(green 'ok')"
else
  printf '%s sent %s to %d process(es)\n' "$(green 'ok')" "$SIG" "$killed"
fi
