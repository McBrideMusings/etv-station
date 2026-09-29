#!/usr/bin/env bash
# Wrapper around `cargo run --release --bin taste-lab`
# (crates/etv-station/src/bin/taste-lab.rs) — a local web UI for iteratively
# tuning a plugin pool's taste profile against the real catalog + plexdb
# snapshot (etv-station-sctf.8).
#
# `--release` is not optional: a debug build's score::pick() takes 15-22s
# against the real ~11,634-movie catalog, which makes a live-tuning loop
# unusable. See the tool's own module doc for the rest of that story.
#
# Reuses tools/taste-debug.sh's local catalog/plexdb cache
# (tmp/claude/scratchpad/taste-debug/) rather than a second one, and exports
# PLEXDB_SNAPSHOT_PATH so every pool's own `datastores: [{ path:
# "${PLEXDB_SNAPSHOT_PATH}" }]` resolves to that local copy instead of the
# production mount .env normally points it at.
set -u

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

CACHE_DIR="tmp/claude/scratchpad/taste-debug"
DEFAULT_CATALOG="$CACHE_DIR/catalog.db"
DEFAULT_PLEXDB="$CACHE_DIR/plexdb.snapshot.db"

missing=()
[ -f "$DEFAULT_CATALOG" ] || missing+=("catalog.db")
[ -f "$DEFAULT_PLEXDB" ] || missing+=("plexdb.snapshot.db")

if [ "${#missing[@]}" -gt 0 ]; then
  echo "Missing local copy: ${missing[*]}" >&2
  echo "Fetch from the Unraid host first:" >&2
  echo "  mkdir -p $CACHE_DIR" >&2
  for name in "${missing[@]}"; do
    echo "  scp ${UNRAID_USER:-root}@${UNRAID_HOST:?set UNRAID_HOST in .env}:/mnt/user/appdata/etv-station/data/$name $CACHE_DIR/$name" >&2
  done
  exit 1
fi

export PLEXDB_SNAPSHOT_PATH="$DEFAULT_PLEXDB"

exec cargo run --quiet --release --bin taste-lab -- --catalog "$DEFAULT_CATALOG" "$@"
