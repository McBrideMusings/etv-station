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
#
# A `--serve-ui` daemon is the web dashboard (tools/station-ui.sh): it binds its
# own port and shares no folder or process with the dev stack, so it is not
# listed. Dropping it here keeps it from blocking dev-run's pre-flight, from
# failing verify-integration's orphan check, and from being killed by kill-dev.
dev_proc_pids() {
  local kind="$1" pattern="$2" pid pids
  if [ "$kind" = "exact" ]; then
    pids=$(pgrep -x "$pattern" 2>/dev/null || true)
  else
    pids=$(pgrep -f "$pattern" 2>/dev/null || true)
  fi
  for pid in $pids; do
    case "$(ps -o command= -p "$pid" 2>/dev/null)" in
      *" --serve-ui"*) continue ;;
    esac
    echo "$pid"
  done
}
