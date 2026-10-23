#!/usr/bin/env bash
# Open a channel's generated playout in a Wheel artifact: a day-by-day timeline
# of every airing, the slot and rotation draw behind it, and the bumpers between
# programmes. The data is the dashboard's own Review JSON, so the page shows what
# the station generates.
#
# Usage:
#   tools/playout-view.sh <channel folder | channel.yaml> [--days N] [--prod]
#   tools/playout-view.sh 002-for-pierce --days 3
#
# --days is the projection length past the published frontier (1, 3, 7 or 14;
# default 7). --prod reads the deployed dashboard; without it a local dashboard
# answers (one already on ETV_UI_PORT is reused, else tools/station-ui.sh is
# started and stopped on exit).
#
# Run from a Wheel terminal or agent pane: the page opens in a pane split to the
# right of $WHEEL_PANE (or refreshes the pane already showing it). Exits 1 when
# the page's airing count differs from the Review JSON's.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
days=7
prod=0
target=""
while [ $# -gt 0 ]; do
  case "$1" in
    --days) [ $# -ge 2 ] || { echo "playout-view: --days needs a number" >&2; exit 2; }; days="$2"; shift ;;
    --prod) prod=1 ;;
    -h|--help) sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) [ -z "$target" ] || { echo "playout-view: one channel only" >&2; exit 2; }; target="$1" ;;
  esac
  shift
done
[ -n "$target" ] || { echo "usage: tools/playout-view.sh <channel folder | channel.yaml> [--days N] [--prod]" >&2; exit 2; }
case "$days" in 1|3|7|14) ;; *) echo "playout-view: --days must be 1, 3, 7 or 14" >&2; exit 2 ;; esac
: "${WHEEL_PANE:?playout-view: run inside a Wheel pane (WHEEL_PANE is unset)}"
: "${WHEEL_SPACE:?playout-view: run inside a Wheel pane (WHEEL_SPACE is unset)}"

case "$target" in
  *.yaml|*.yml) key="$(basename "$(dirname "$target")")" ;;
  *) key="$(basename "$target")" ;;
esac
artifact="playout-$key"

work="$(mktemp -d)"
ui_pid=""
cleanup() {
  if [ -n "$ui_pid" ]; then
    pkill -P "$ui_pid" 2>/dev/null || true
    kill "$ui_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

# 1. A dashboard that answers. At most one is started, and only if none is up.
get_args=()
if [ "$prod" = 1 ]; then
  get_args=(--prod)
else
  port="${ETV_UI_PORT:-4747}"
  if ! curl -sf -o /dev/null "http://127.0.0.1:$port/api/health"; then
    echo "playout-view: no dashboard on :$port; starting tools/station-ui.sh" >&2
    ( cd "$repo" && exec tools/station-ui.sh ) >"$work/ui.log" 2>&1 &
    ui_pid=$!
    deadline=$(( $(date +%s) + ${ETV_UI_START_WAIT:-900} ))
    until curl -sf -o /dev/null "http://127.0.0.1:$port/api/health"; do
      if ! kill -0 "$ui_pid" 2>/dev/null; then
        echo "playout-view: tools/station-ui.sh exited; last lines:" >&2
        tail -20 "$work/ui.log" >&2
        exit 1
      fi
      if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "playout-view: dashboard not up after ${ETV_UI_START_WAIT:-900}s" >&2
        exit 1
      fi
      sleep 3
    done
  fi
fi

# 2. The Review JSON, trimmed to what the page reads (the audit trails are large).
"$repo/tools/ui-get.sh" "/api/channels/$key/review?project=${days}d" ${get_args[@]+"${get_args[@]}"} >"$work/review.json"
if jq -e 'has("load_error")' "$work/review.json" >/dev/null; then
  echo "playout-view: $key failed to load: $(jq -r .load_error "$work/review.json")" >&2
  exit 1
fi
jq '{
  channel, number, name, display_name, tz, span, notices, day_starts, principles,
  blocks: [.blocks[] | {name, pools, placed}],
  rotations: (.rotations // []),
  airings: [.airings[] | {index, label, kind, pool, start, duration_secs, projected, slot, schedule, rotation, rotation_reason}],
  bumpers: (.bumpers // [])
}' "$work/review.json" >"$work/data.json"
expected="$(jq '.airings | length' "$work/data.json")"

# 3. The artifact, reused when it exists.
folder="$(wheel artifact ls | awk -v n="$artifact" '$2 == n { getline; print $1; exit }' | xargs -I{} dirname {})"
if [ -z "$folder" ]; then
  wheel artifact new "$artifact" --expires 7d >/dev/null
  folder="$(wheel artifact ls | awk -v n="$artifact" '$2 == n { getline; print $1; exit }' | xargs -I{} dirname {})"
fi
[ -n "$folder" ] || { echo "playout-view: could not find the folder of artifact $artifact" >&2; exit 1; }
# The data rides inside the page: a pushed value (`wheel artifact data`) never
# reached the page's onData in a web pane, and a page cannot fetch from the
# artifact folder.
jq -c . "$work/data.json" | sed 's#</#<\\/#g' >"$work/data.inline"
jq -Rrs --arg d "$(cat "$work/data.inline")" 'sub("/\\*PLAYOUT_DATA\\*/null"; $d)' "$repo/tools/playout-view.html" >"$folder/index.html"

# 4. A pane showing it: the one already open (reloaded), else a split right of this pane.
pane="$(wheel status | jq -r --arg s "$WHEEL_SPACE" --arg a "$artifact" '
  [.spaces[] | select(.id == $s) | .. | objects | select(.node? == "pane" and .kind == "web" and .name == $a) | .id][0] // empty')"
if [ -z "$pane" ]; then
  pane="$(wheel pane split "$WHEEL_PANE" right)"
fi
wheel pane kind "$pane" web --artifact "$artifact" >/dev/null

# 5. Check the page drew every airing: it stores its row count with wheel.set.
shown="$(wheel artifact state "$pane" airings --wait --timeout 30 2>/dev/null || true)"
echo "playout-view: $key ${days}d -> pane $pane, review airings $expected, page airings ${shown:-none}"
[ "$shown" = "$expected" ] || { echo "playout-view: airing count mismatch" >&2; exit 1; }
