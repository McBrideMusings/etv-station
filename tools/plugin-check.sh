#!/usr/bin/env bash
# Check every plugin script the deploy host actually loads against THIS
# checkout's plugin hook contract, before shipping a binary that would refuse
# to load them (#396, #397).
#
# WHY THIS EXISTS. Contract enforcement lives at config load, so it reaches
# exactly the scripts some pool names, inside the container, at startup. #389
# made `audit(ctx, picks, workspace)` mandatory for every `pool_provider` and
# updated `examples/plugins/`. The host loads `deploy/appdata/plugins/`, a
# separate gitignored copy nothing updated. New binary, old scripts: config load
# refused, the entrypoint died, and every one of 64 channels was dark for 27
# minutes across 35 restarts, 13:48-14:15 UTC (9:48-10:15am ET) on 2026-08-31.
# `tools/verify-all.sh` passed the whole time, because it only ever sees
# `examples/`.
#
# WHY IT COPIES THE HOST'S SCRIPTS RATHER THAN COMPARING LOCAL COPIES. The host
# carries scripts with no counterpart in this checkout at all — it held
# `taste-engine.rhai`, a `pool_provider` with no `audit()` that exists nowhere in
# this repo, armed and invisible because its only reference sat inside a YAML
# comment. `admin deploy files` will never remove it either: `delete = false` is
# correct there, since the same directory holds catalog.db, history.db and the
# HLS working set. An orphan is the case that bites, and no check that compares
# local files to their counterparts can express it. So this pulls the host's
# whole plugin directory down and walks it.
#
# WHAT DOES THE CHECKING. `etv-station --check-plugins <DIR>`, built from this
# checkout — the binary you are about to ship, not the one already deployed.
# That is the point: it answers "would the code on my branch load the scripts on
# the host", which is the question the outage asked and nothing could answer.
#
# READ-ONLY. It copies files down, touches no container and changes nothing on
# the host.
#
# Usage:
#   tools/plugin-check.sh              # host scripts + both local directories
#   tools/plugin-check.sh --local      # local directories only, no ssh
#
# Env (from .env, gitignored — these name a specific machine, no fallbacks):
#   UNRAID_HOST, ETV_STATION_APPDATA, and UNRAID_USER (defaults to root)
#
# Exits 0 when every script passes, 1 on any failing script, 2 on a setup
# problem.
set -uo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)

LOCAL_ONLY=0
[ "${1:-}" = "--local" ] && LOCAL_ONLY=1

# The same binary the image runs, built from this checkout. Debug, not release:
# this compiles a Rhai script and reads its function table, so the build is the
# slow half and the run is milliseconds either way.
echo "==> building etv-station"
if ! cargo build -q --manifest-path "$repo_root/Cargo.toml" -p etv-station --bin etv-station; then
  echo "fatal: cargo build failed — fix the build before checking plugins" >&2
  exit 2
fi
STATION="$repo_root/target/debug/etv-station"
[ -x "$STATION" ] || { echo "fatal: no binary at $STATION" >&2; exit 2; }

rc=0

check_dir() {
  local label="$1" dir="$2"
  echo
  echo "==> $label  ($dir)"
  if [ ! -d "$dir" ]; then
    echo "    absent — skipped"
    return 0
  fi
  "$STATION" --check-plugins "$dir" || rc=1
}

# The repo's own scripts, then the copy a deploy ships. The second is gitignored,
# so it is absent in a worktree and in a fresh clone; absence is a skip.
check_dir "examples/plugins (what the repo tests)" "$repo_root/examples/plugins"
check_dir "deploy/appdata/plugins (what a deploy ships)" "$repo_root/deploy/appdata/plugins"

if [ "$LOCAL_ONLY" = "1" ]; then
  echo
  [ "$rc" = "0" ] && echo "PASS (local only — the host was not checked)" || echo "FAIL"
  exit "$rc"
fi

host="${UNRAID_HOST:-}"
appdata="${ETV_STATION_APPDATA:-}"
if [ -z "$host" ] || [ -z "$appdata" ]; then
  echo >&2
  echo "fatal: UNRAID_HOST and ETV_STATION_APPDATA must be set to reach the host." >&2
  echo "       They live in .env (gitignored). Run with --local to skip the host." >&2
  exit 2
fi
target="${UNRAID_USER:-root}@$host"

# tar over ssh rather than scp/rsync: one round trip, and it brings down every
# *.rhai in the directory including ones this repo has never heard of, which is
# the entire reason for checking the host at all.
staged=$(mktemp -d "${TMPDIR:-/tmp}/etv-plugin-check.XXXXXX") || exit 2
trap 'rm -rf "$staged"' EXIT

echo
echo "==> fetching $target:$appdata/plugins"
# ssh's stderr is left alone on purpose: an unreachable host, a refused key and a
# missing plugins/ directory all land here, and the message it prints is the only
# thing that separates them.
if ! ssh -o BatchMode=yes "$target" "tar -C '$appdata' -cf - plugins" | tar -xf - -C "$staged"; then
  echo "fatal: could not read $target:$appdata/plugins over ssh" >&2
  exit 2
fi

check_dir "HOST plugins (what production loads)" "$staged/plugins"

# Name what is on the host and not here. Not a failure on its own — an
# intentionally host-only script is legitimate — but it is the thing nothing
# else in this repo can tell you, so it always prints.
echo
echo "==> scripts on the host with no copy in this checkout"
orphans=0
for f in "$staged"/plugins/*.rhai; do
  [ -e "$f" ] || continue
  name=$(basename "$f")
  if [ ! -f "$repo_root/deploy/appdata/plugins/$name" ] && [ ! -f "$repo_root/examples/plugins/$name" ]; then
    echo "    ORPHAN $name  (exists only on $host)"
    orphans=$((orphans + 1))
  fi
done
[ "$orphans" = "0" ] && echo "    none"

echo
if [ "$rc" = "0" ]; then
  echo "PASS — every script on $host loads under this checkout's contract"
else
  echo "FAIL — deploying this binary would take the station off the air" >&2
fi
exit "$rc"
