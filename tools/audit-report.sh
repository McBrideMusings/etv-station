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
#   tools/audit-report.sh                        # list channels, exit 0
#   tools/audit-report.sh for-pierce             # report for one channel
#   tools/audit-report.sh for-pierce --next 25   # extra flags reach the binary
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

# No channel named: list what exists and stop. The station writes one directory
# per channel under `output_base` (/data/playout in the container, which is
# $ETV_STATION_DATA/playout on the host), so the directory listing IS the
# channel list — no need to start the binary to ask.
if [[ $# -eq 0 ]]; then
  channels=$(ssh "$target" "ls -1 '$ETV_STATION_DATA/playout' 2>/dev/null") || {
    echo "audit: cannot read $ETV_STATION_DATA/playout on $target" >&2
    exit 2
  }
  if [[ -z $channels ]]; then
    echo "No channels have written playout yet."
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
