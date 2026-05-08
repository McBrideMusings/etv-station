#!/usr/bin/env bash
# Kill all processes started by ./tools/dev-run.sh.
# Usage:
#   ./tools/kill-dev.sh          # SIGTERM
#   FORCE=1 ./tools/kill-dev.sh  # SIGKILL (for stuck processes)
set -u

FORCE="${FORCE:-0}"
SIG=$( [ "$FORCE" -eq 1 ] && echo "-KILL" || echo "-TERM" )

bold()   { printf '\033[1m%s\033[0m' "$1"; }
green()  { printf '\033[32m%s\033[0m' "$1"; }
yellow() { printf '\033[33m%s\033[0m' "$1"; }

killed=0

kill_exact() {
  local label="$1" name="$2"
  local pids
  pids=$(pgrep -x "$name" 2>/dev/null) || true
  if [ -n "$pids" ]; then
    # shellcheck disable=SC2086
    echo "$pids" | xargs kill $SIG 2>/dev/null || true
    printf '  %s %s (pid %s)\n' "$(yellow 'killed')" "$label" "$(echo "$pids" | tr '\n' ' ' | sed 's/ $//')"
    killed=$((killed + $(echo "$pids" | wc -l | tr -d ' ')))
  fi
}

kill_pattern() {
  local label="$1" pattern="$2"
  local pids
  pids=$(pgrep -f "$pattern" 2>/dev/null) || true
  if [ -n "$pids" ]; then
    # shellcheck disable=SC2086
    echo "$pids" | xargs kill $SIG 2>/dev/null || true
    printf '  %s %s (pid %s)\n' "$(yellow 'killed')" "$label" "$(echo "$pids" | tr '\n' ' ' | sed 's/ $//')"
    killed=$((killed + $(echo "$pids" | wc -l | tr -d ' ')))
  fi
}

printf '%s stopping dev processes (signal: %s)\n' "$(bold '==>')" "$SIG"

kill_exact  "etv-station"       "etv-station"
kill_exact  "ersatztv"          "ersatztv"
kill_exact  "ersatztv-channel"  "ersatztv-channel"
kill_pattern "ffmpeg (hls)"     "ffmpeg.*tmp/hls"
kill_pattern "ffprobe (lavfi)"  "ffprobe.*lavfi"

if [ "$killed" -eq 0 ]; then
  printf '%s no dev processes found\n' "$(green 'ok')"
else
  printf '%s sent %s to %d process(es)\n' "$(green 'ok')" "$SIG" "$killed"
fi
