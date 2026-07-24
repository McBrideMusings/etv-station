#!/usr/bin/env bash
# Canonical list of every process ./tools/dev-run.sh can leave behind, sourced
# by both tools/kill-dev.sh and tools/dev-run.sh.
#
# One list, two consumers, on purpose: a process missing from here is a process
# that survives `kill-dev` AND slips past dev-run's pre-flight stale check, so
# two hand-maintained copies would silently reintroduce orphans the moment they
# drift. Add new dev-stack processes here, nowhere else.
#
# Each entry is "label|kind|pattern"; kind is `exact` (pgrep -x, matches the
# process name) or `pattern` (pgrep -f, matches the full command line).
DEV_PROCS=(
  "etv-station|exact|etv-station"
  "ersatztv|exact|ersatztv"
  "ersatztv-channel|exact|ersatztv-channel"
  "etv-overlay|exact|etv-overlay"
  "ffmpeg (hls)|pattern|ffmpeg.*tmp/hls"
  "ffprobe (lavfi)|pattern|ffprobe.*lavfi"
)

# Echo the PIDs matching one DEV_PROCS entry, one per line; empty output when
# nothing matches (pgrep exits non-zero in that case, which `|| true` absorbs so
# callers running under `set -e` aren't killed by a normal "none found").
dev_proc_pids() {
  local kind="$1" pattern="$2"
  if [ "$kind" = "exact" ]; then
    pgrep -x "$pattern" 2>/dev/null || true
  else
    pgrep -f "$pattern" 2>/dev/null || true
  fi
}
