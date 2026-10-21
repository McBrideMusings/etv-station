#!/usr/bin/env bash
# Run the station's web UI (`etv-station --serve-ui`, crates/etv-station/src/web_ui/)
# locally, against the real catalog + plexdb snapshot and this checkout's
# in-development plugin scripts. The container runs the same mode on its own.
#
# A release build is not optional: a debug build's score::pick() takes 15-22s
# against the real ~11,634-movie catalog, which makes a live-tuning loop
# unusable. See the module's own doc for the rest of that story.
#
# Keeps a local catalog/plexdb cache
# (tmp/claude/scratchpad/station-cache/, kept in step with the host by
# tools/taste-cache.sh) rather than a second one. ETV_STATION_CATALOG points the
# station config's `catalog_path` at that copy, and PLEXDB_SNAPSHOT_PATH makes
# every pool's own `datastores: [{ path: "${PLEXDB_SNAPSHOT_PATH}" }]` resolve to
# it instead of the production mount .env normally points it at.
#
# --ui-private-catalog makes the UI serve a migrated copy (catalog.db.ui-copy)
# of that cache, so a checkout whose schema is ahead of prod's still answers;
# the cache itself stays at the host's schema, which is what taste-cache.sh
# compares.
#
# STATION_UI_CONFIG picks another lineup (default deploy/appdata/station.yaml,
# e.g. `STATION_UI_CONFIG=examples/station.yaml`). Extra arguments pass through,
# e.g. `--ui-port N` for another port.
set -u

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

CACHE_DIR="tmp/claude/scratchpad/station-cache"

bash tools/taste-cache.sh catalog.db plexdb.snapshot.db || exit 1

export ETV_STATION_CATALOG="$CACHE_DIR/catalog.db"
export PLEXDB_SNAPSHOT_PATH="$CACHE_DIR/plexdb.snapshot.db"

# No `--quiet`: on a cold target/ the release build compiles ~480 crates
# (the vello/wgpu stack included) for minutes, and cargo's per-crate
# "Compiling ..." lines are the only sign in the task log that it isn't hung.
echo "station-ui: building release binary (minutes on a cold target/, seconds after)..." >&2
exec cargo run --release --bin etv-station -- \
  --config "${STATION_UI_CONFIG:-deploy/appdata/station.yaml}" --serve-ui --ui-private-catalog \
  --ui-plugins-dir examples/plugins "$@"
