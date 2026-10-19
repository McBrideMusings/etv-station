#!/usr/bin/env bash
# Print one web dashboard endpoint's JSON — the same answer the page shows.
#
# A simulation-backed endpoint (a channel's review) answers 202
# {"pending": true} (with the real review) while the UI's worker projects; this
# asks again every 15s until the projection arrives, never returning the 202.
#
# Usage:
#   tools/ui-get.sh [-X METHOD] [-d JSON] <path> [--prod]
#   tools/ui-get.sh /api/lineup
#   tools/ui-get.sh '/api/channels/034-syfy/review?project=3d'
#   tools/ui-get.sh /api/health --prod
#   tools/ui-get.sh -X POST -d '{}' /api/channels/001-for-you/drafts
#
# Local is http://127.0.0.1:${ETV_UI_PORT:-4747} (tools/station-ui.sh). --prod
# is ${ETV_UI_PROD_URL}, else http://${UNRAID_HOST}:4747, read from .env.
# ETV_UI_WAIT caps the wait for a pending answer (default 900s).
#
# The body goes to stdout. Exits 0 on a 2xx, 1 on a 4xx/5xx (body still on
# stdout, status on stderr), 2 when the server cannot be reached or the wait
# runs out.
set -euo pipefail

usage() { echo "usage: tools/ui-get.sh [-X METHOD] [-d JSON] <path> [--prod]" >&2; exit 2; }

path=""
prod=0
method="GET"
data=""
while [ $# -gt 0 ]; do
  case "$1" in
    --prod) prod=1 ;;
    -X) [ $# -ge 2 ] || usage; method="$2"; shift ;;
    -d) [ $# -ge 2 ] || usage; data="$2"; shift ;;
    -h|--help) usage ;;
    *) [ -z "$path" ] || usage; path="$1" ;;
  esac
  shift
done
[ -n "$path" ] || usage
case "$path" in /*) ;; *) path="/$path" ;; esac

# A value the caller set wins over .env's.
caller_port="${ETV_UI_PORT:-}"
caller_prod_url="${ETV_UI_PROD_URL:-}"
caller_wait="${ETV_UI_WAIT:-}"
repo="$(cd "$(dirname "$0")/.." && pwd)"
if [ -f "$repo/.env" ]; then
  set -a
  # shellcheck disable=SC1091
  . "$repo/.env"
  set +a
fi
ETV_UI_PORT="${caller_port:-${ETV_UI_PORT:-}}"
ETV_UI_PROD_URL="${caller_prod_url:-${ETV_UI_PROD_URL:-}}"
ETV_UI_WAIT="${caller_wait:-${ETV_UI_WAIT:-}}"

if [ "$prod" = 1 ]; then
  if [ -n "${ETV_UI_PROD_URL:-}" ]; then
    base="$ETV_UI_PROD_URL"
  else
    : "${UNRAID_HOST:?set UNRAID_HOST (or ETV_UI_PROD_URL) in .env}"
    base="http://$UNRAID_HOST:4747"
  fi
else
  base="http://127.0.0.1:${ETV_UI_PORT:-4747}"
fi

body="$(mktemp)"
trap 'rm -f "$body"' EXIT
deadline=$(( $(date +%s) + ${ETV_UI_WAIT:-900} ))
while :; do
  curl_args=(-sS --max-time 600 -X "$method" -o "$body" -w '%{http_code}')
  [ -z "$data" ] || curl_args+=(-H 'Content-Type: application/json' -d "$data")
  if ! status="$(curl "${curl_args[@]}" "$base$path")"; then
    echo "ui-get: cannot reach $base$path" >&2
    exit 2
  fi
  if [ "$status" != 202 ]; then
    break
  fi
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "ui-get: $base$path still pending after ${ETV_UI_WAIT:-900}s" >&2
    exit 2
  fi
  echo "ui-get: $path pending; asking again in 15s" >&2
  sleep 15
done

cat "$body"
echo
case "$status" in
  2??) exit 0 ;;
  *) echo "ui-get: $base$path answered HTTP $status" >&2; exit 1 ;;
esac
