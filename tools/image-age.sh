#!/usr/bin/env bash
# How far behind origin/main is the deployed container? (#362)
#
# Reads the commit the running container was built from off its /build.json
# (stamped by tools/build-image.sh) and counts the commits on origin/main that
# it lacks. A merged fix that is not running reads "N commits behind" instead
# of looking like a fix that did not work.
#
# Usage: tools/image-age.sh [base-url]     (default: $PROD_URL)
#
# Exits 0 when the container is built from origin/main's tip, 1 when it is
# behind, unstamped, dirty, or built from a commit origin/main does not
# contain, 2 on a setup problem (no URL, unreachable, fetch failed).
set -uo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
base=${1:-${PROD_URL:-}}
[ -n "$base" ] || { echo "fatal: pass a base URL or set PROD_URL" >&2; exit 2; }

stamp=$(curl -fsS --max-time 15 "${base%/}/build.json") \
  || { echo "fatal: could not read ${base%/}/build.json (a container built before #362 has no such route)" >&2; exit 2; }
command -v python3 >/dev/null || { echo "fatal: python3 not found on PATH" >&2; exit 2; }
fields=$(printf %s "$stamp" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["revision"], d["created"])') \
  || { echo "fatal: unparseable stamp: $stamp" >&2; exit 2; }
read -r revision created <<<"$fields"

git -C "$repo_root" fetch -q origin main || { echo "fatal: git fetch origin main failed" >&2; exit 2; }
tip=$(git -C "$repo_root" rev-parse origin/main)

if [ "$revision" = "unknown" ]; then
  echo "UNSTAMPED: the container was built without tools/build-image.sh (created $created)"
  exit 1
fi
sha=${revision%-dirty}
suffix=""
[ "$revision" != "$sha" ] && suffix=" (built from a dirty tree)"
if ! git -C "$repo_root" merge-base --is-ancestor "$sha" origin/main 2>/dev/null; then
  echo "DIVERGED: container built from $revision (created $created), which is not on origin/main (${tip:0:12})"
  exit 1
fi

behind=$(git -C "$repo_root" rev-list --count "$sha..origin/main")
if [ "$behind" -eq 0 ] && [ -z "$suffix" ]; then
  echo "CURRENT: container built from origin/main tip ${sha:0:12}, created $created"
  exit 0
fi
echo "BEHIND: container built from ${sha:0:12} (created $created) is $behind commits behind origin/main (${tip:0:12})$suffix"
exit 1
