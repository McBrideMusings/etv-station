#!/usr/bin/env bash
# Sweep every channel's failure accounting from ETV-next's /health/channels.json.
#
# A channel that keeps dying under a viewer crosses ETV-next's failure threshold
# and /channel/{n}.m3u8 then answers 503 (Retry-After: 30) until its backoff
# runs out. This lists those channels, and any channel partway there, without
# tuning in to each one.
#
# Usage:
#   tools/channel-health.sh [base-url]      # default http://127.0.0.1:8409
#   tools/channel-health.sh --all [base-url] # every channel, not just unhealthy ones
#
# Exits 1 if any channel is failed and still inside its backoff (answering 503
# now), 2 if the endpoint cannot be read. A failed channel whose backoff has run
# out prints "failed, retry open": the next tune-in spawns it.
set -euo pipefail

all=0
if [ "${1:-}" = "--all" ]; then
  all=1
  shift
fi
base="${1:-http://127.0.0.1:8409}"

if ! report="$(curl -fsS --max-time 10 "$base/health/channels.json")"; then
  echo "channel-health: cannot read $base/health/channels.json" >&2
  exit 2
fi

jq -r --argjson all "$all" '
  .channels[]
  | select($all == 1 or .consecutive_failures > 0 or .failed)
  | [ .number, .name,
      (if .failed and .retry_in_secs > 0 then "FAILED"
       elif .failed then "failed, retry open"
       elif .consecutive_failures > 0 then "failing" else "ok" end),
      "\(.consecutive_failures) consecutive",
      (if .retry_in_secs > 0 then "503 for \(.retry_in_secs)s more" else "spawn allowed" end),
      (if .last_failure then "last: \(.last_failure.at) \(.last_failure.cause)" else "" end) ]
  | @tsv' <<<"$report"

failed="$(jq '[.channels[] | select(.failed and .retry_in_secs > 0)] | length' <<<"$report")"
total="$(jq '.channels | length' <<<"$report")"
echo "$failed of $total channels failed and answering 503" >&2
[ "$failed" -eq 0 ]
