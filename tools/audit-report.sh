#!/usr/bin/env bash
# Run the station's `--audit` mode against the deployed container and print the
# report it wrote (#390, #394).
#
# WHY THIS IS A SCRIPT AND NOT A ONE-LINE admin.toml ENTRY. `admin
# refresh-channel` and `admin backfill-history` are single `ssh … docker exec`
# lines because the station's own stdout is the whole answer. `--audit` is
# different in two ways:
#
#   1. It writes the report to a FILE inside the container and prints only that
#      path. Printing the report therefore needs a second remote call to read
#      the file back — two steps admin.toml's `run` string cannot sequence.
#   2. Bare `admin audit`, with no channel, has to answer "which channels have
#      one" and exit 0. A list is an answer, not a usage error.
#
# WHAT IT DELIBERATELY DOES NOT NEED. `taste-debug` is the counterexample this
# exists to avoid: it runs locally, so it needs a local copy of catalog.db and
# the plexdb snapshot, and its most common outcome on a dev Mac is a message
# telling you to scp two databases. `--audit` reads chunk JSON and nothing else
# — no catalog, no plexdb snapshot, no plugin evaluation (ADR 0011) — so this
# wrapper never copies a database anywhere and never needs one present.
#
# Usage:
#   tools/audit-report.sh                             # list channels, exit 0
#   tools/audit-report.sh --list                      # name<TAB>chunk-count list (admin.toml picker)
#   tools/audit-report.sh --list --format json        # name/number/display_name map, from the binary
#   tools/audit-report.sh for-pierce                  # report for one channel
#   tools/audit-report.sh for-pierce --next 25        # extra flags reach the binary
#   tools/audit-report.sh for-pierce --format json    # JSON report (per-item
#                                                      # audit trail + overlay_spec)
#
# Env (from .env, gitignored — no fallbacks, these name a specific machine):
#   UNRAID_HOST, UNRAID_USER, ETV_STATION_DATA
#
# Optional:
#   ETV_STATION_CONFIG   in-container config path (default /config/station.yaml)
#
# Exits 0 on a printed report or a channel listing, 1 when the station refuses
# (unknown channel, ambiguous match), 2 on a setup problem.
set -uo pipefail

: "${UNRAID_HOST:?UNRAID_HOST is unset — it names the deploy host and lives in .env}"
: "${ETV_STATION_DATA:?ETV_STATION_DATA is unset — it names the host data volume and lives in .env}"
user=${UNRAID_USER:-root}
config=${ETV_STATION_CONFIG:-/config/station.yaml}
target="$user@$UNRAID_HOST"

# No channel named: list what has actually written playout and stop.
#
# DIRECTORIES ONLY, and that is not a tidiness preference. The station writes
# one directory per channel under `output_base` (/data/playout in the
# container, $ETV_STATION_DATA/playout on the host) — but it also keeps
# history.db there, with its -wal and -shm siblings beside it. A plain `ls`
# listed all three as if they were channels you could audit.
#
# `--list` is the machine form: one `name<TAB>label` line per channel, which is
# what admin.toml's picker parses. Bare is the human form.
#
# `--list --format json` is a different machine form entirely: the number ->
# name map the EPG TUI needs, which only the binary's own `--audit --list
# --format json` can produce. Route that one straight to the container over
# the same docker-exec path the per-channel report uses below, and print its
# stdout as-is — unlike the per-channel report, list mode prints the JSON
# array directly instead of writing a file and printing its path, so this is
# one remote call, not the file-read-back pattern.
if [[ ${1:-} == "--list" && "$*" == *"--format"* ]]; then
  ssh "$target" "docker exec etv-station etv-station --config '$config' --audit $*" || exit 1
  exit 0
fi

if [[ $# -eq 0 || ${1:-} == "--list" ]]; then
  channels=$(ssh "$target" "find '$ETV_STATION_DATA/playout' -mindepth 1 -maxdepth 1 -type d -exec basename {} \; 2>/dev/null | sort") || {
    echo "audit: cannot read $ETV_STATION_DATA/playout on $target" >&2
    exit 2
  }
  if [[ -z $channels ]]; then
    [[ ${1:-} == "--list" ]] || echo "No channels have written playout yet."
    exit 0
  fi
  # The picker's second column is the chunk count, not a constant string: it is
  # the one cheap fact that tells a channel with a full window apart from one
  # holding a single file, and it costs no extra round trip.
  if [[ ${1:-} == "--list" ]]; then
    ssh "$target" "find '$ETV_STATION_DATA/playout' -mindepth 1 -maxdepth 1 -type d -exec sh -c 'printf \"%s\t%s chunk(s)\n\" \"\$(basename \"\$1\")\" \"\$(ls -1 \"\$1\"/*.json 2>/dev/null | wc -l | tr -d \" \")\"' _ {} \; 2>/dev/null | sort"
    exit 0
  fi
  echo "Channels with a schedule to audit:"
  echo "$channels" | sed 's/^/  /'
  echo
  echo "Run: admin audit <channel> [--next N]"
  exit 0
fi

# One remote invocation, then one remote read. The station prints the path it
# wrote and nothing else on stdout, so the path is the whole capture; anything
# it says on stderr (a refusal naming the channels that exist) passes through
# to the caller untouched.
path=$(ssh "$target" "docker exec etv-station etv-station --config '$config' --audit $*") || {
  # The station already explained itself on stderr — naming an unknown channel,
  # or naming every channel an ambiguous one matched. Do not restate it.
  exit 1
}

if [[ -z $path ]]; then
  echo "audit: the station printed no report path" >&2
  exit 2
fi

ssh "$target" "docker exec etv-station cat '$path'" || {
  echo "audit: the station reported $path but it could not be read back" >&2
  exit 2
}
