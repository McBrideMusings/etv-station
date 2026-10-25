#!/usr/bin/env bash
# Print one catalog entry's audio facts as JSON.
#
# Usage: tools/audio-facts.sh <entry_id> [--prod]
#
# Reads the dashboard's GET /api/audio/{entry_id} through tools/ui-get.sh, so it
# needs the dashboard running (tools/station-ui.sh locally, the deployed one
# with --prod). `analyzed: false` means the analyzer has not run on the entry;
# `error` says why a run failed.
set -euo pipefail

[ $# -ge 1 ] || { echo "usage: tools/audio-facts.sh <entry_id> [--prod]" >&2; exit 2; }
entry="$1"
shift
here="$(cd "$(dirname "$0")" && pwd)"
encoded="$(printf %s "$entry" | python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.stdin.read(), safe=""))')"
exec "$here/ui-get.sh" "/api/audio/$encoded" "$@"
