#!/usr/bin/env bash
# Print the facts the catalog holds about one entry as JSON (its shot list), from
# the dashboard's /api/entries/<entry_id>/facts.
#
# Usage:
#   tools/facts.sh <entry_id> [--prod]
#   tools/facts.sh imdb:tt0095016
#
# Local is the dashboard tools/station-ui.sh serves; --prod is the deployed one
# (see tools/ui-get.sh). `etv-station facts <entry_id>` reads the same row
# straight from a catalog file when no dashboard is running.
set -euo pipefail

[ $# -ge 1 ] || { echo "usage: tools/facts.sh <entry_id> [--prod]" >&2; exit 2; }
entry="$1"
shift
encoded=$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$entry")
exec "$(dirname "$0")/ui-get.sh" "/api/entries/${encoded}/facts" "$@"
