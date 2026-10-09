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
# (tmp/claude/scratchpad/taste-debug/, kept in step with the host by
# tools/taste-cache.sh) rather than a second one, and exports
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

bash tools/taste-cache.sh catalog.db plexdb.snapshot.db || exit 1

export PLEXDB_SNAPSHOT_PATH="$DEFAULT_PLEXDB"

# No `--quiet`: on a cold target/ the release build compiles ~480 crates
# (the vello/wgpu stack included) for minutes, and cargo's per-crate
# "Compiling ..." lines are the only sign in the task log that it isn't hung.
echo "taste-lab: building release binary (minutes on a cold target/, seconds after)..." >&2
exec cargo run --release --bin taste-lab -- --catalog "$DEFAULT_CATALOG" "$@"
