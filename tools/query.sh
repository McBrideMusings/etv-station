#!/usr/bin/env bash
set -euo pipefail

if [ $# -lt 1 ]; then
  cat <<'EOF'
Usage: tools/query.sh <CEL_EXPR> --source <SPEC> [--order KEYS] [--limit N] [--format table|json]

Sources:
  plex-section:ID       Plex library section
  plex-show:RATING_KEY  Plex TV show (returns all leaves / episodes)
  plex-collection:ID    Plex collection
  fs:PATH               Local filesystem catalog (glob + ffprobe)

Examples:
  tools/query.sh 'title == "Star Trek"' --source plex-show:1234
  tools/query.sh 'season_in(3, 5)' --source plex-show:1234 --order season,episode --limit 25
  tools/query.sh 'shorter_than(30.0)' --source fs:/path/to/bumpers --format json
EOF
  exit 1
fi

cd "$(dirname "$0")/.."
if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
fi
exec cargo run --quiet -p etv-query-test -- query "$@"
