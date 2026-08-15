#!/usr/bin/env bash
# Wrapper around `cargo run --bin taste-debug` (crates/etv-station/src/bin/
# taste-debug.rs) — explains a scorer plugin pool's ranking (taste-cosine's
# For You channels, endless-distance's Endless walk) against the real catalog
# + plexdb snapshot.
#
# Subcommand form picks --channel for you, discovered live from whichever
# deploy/appdata/channels/*.yaml actually declare a `plugin:` pool — nothing
# hardcoded, so a new plugin-backed channel shows up here with no edit to
# this file. A single_user channel (For Pierce, For Madi) resolves its own
# account over Tautulli inside the Rust tool itself, same as a live
# generation — no name-to-id table here, and nothing personally-identifying
# in this committed file (deploy/appdata/ itself is gitignored).
#
#   ./tools/taste-debug.sh                          # list available channels
#   ./tools/taste-debug.sh for-pierce                # explain 002-for-pierce.yaml
#   ./tools/taste-debug.sh endless --top 10 --extended-target-count 500
#   ./tools/taste-debug.sh --channel path/to/other.yaml ...   # raw passthrough
#
# Neither database exists on a dev Mac; both live on the Unraid host at
# /mnt/user/appdata/etv-station/data/{catalog.db,plexdb.snapshot.db}. This
# defaults --catalog/--plexdb to a local read-only copy under
# tmp/claude/scratchpad/taste-debug/ and prints the exact scp commands to
# fetch one when it's missing, rather than handing a raw sqlite "unable to
# open database file" error to a scorer bug that has nothing to do with it.
set -u

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

CHANNELS_DIR="deploy/appdata/channels"
CACHE_DIR="tmp/claude/scratchpad/taste-debug"
DEFAULT_CATALOG="$CACHE_DIR/catalog.db"
DEFAULT_PLEXDB="$CACHE_DIR/plexdb.snapshot.db"

# name (file stem, numeric prefix stripped) -> path, one per plugin-backed
# channel. Discovered by grepping for `plugin:` rather than hand-listed, so
# 001-for-you.yaml becomes `for-you`, 045-endless.yaml becomes `endless`, and
# a future plugin channel needs no edit here.
plugin_channel_path() {
  local name="$1" f stem
  for f in "$CHANNELS_DIR"/*.yaml; do
    [ -f "$f" ] || continue
    grep -q "plugin:" "$f" || continue
    stem="$(basename "$f" .yaml | sed -E 's/^[0-9]+-//')"
    [ "$stem" = "$name" ] && { echo "$f"; return 0; }
  done
  return 1
}

list_plugin_channels() {
  local f stem
  for f in "$CHANNELS_DIR"/*.yaml; do
    [ -f "$f" ] || continue
    grep -q "plugin:" "$f" || continue
    stem="$(basename "$f" .yaml | sed -E 's/^[0-9]+-//')"
    echo "  $stem  ($f)"
  done
}

has_flag() {
  local needle="$1"
  shift
  for arg in "$@"; do
    [ "$arg" = "$needle" ] && return 0
  done
  return 1
}

if [ "$#" -eq 0 ]; then
  echo "Usage: admin taste-debug <channel> [-- taste-debug flags]" >&2
  echo "       admin taste-debug --channel <path> [flags]   (raw passthrough)" >&2
  echo >&2
  echo "Available channels:" >&2
  list_plugin_channels >&2
  exit 1
fi

args=()
if [[ "$1" != --* ]]; then
  channel_path="$(plugin_channel_path "$1")" || {
    echo "Unknown channel: $1" >&2
    echo "Available channels:" >&2
    list_plugin_channels >&2
    exit 1
  }
  shift
  args+=(--channel "$channel_path")
fi
args+=("$@")

missing=()
if ! has_flag --catalog "${args[@]}"; then
  [ -f "$DEFAULT_CATALOG" ] || missing+=("catalog.db")
  args+=(--catalog "$DEFAULT_CATALOG")
fi
if ! has_flag --plexdb "${args[@]}"; then
  [ -f "$DEFAULT_PLEXDB" ] || missing+=("plexdb.snapshot.db")
  args+=(--plexdb "$DEFAULT_PLEXDB")
fi

if [ "${#missing[@]}" -gt 0 ]; then
  echo "Missing local copy: ${missing[*]}" >&2
  echo "Fetch from the Unraid host first:" >&2
  echo "  mkdir -p $CACHE_DIR" >&2
  for name in "${missing[@]}"; do
    echo "  scp ${UNRAID_USER:-root}@${UNRAID_HOST:?set UNRAID_HOST in .env}:/mnt/user/appdata/etv-station/data/$name $CACHE_DIR/$name" >&2
  done
  echo "Or pass your own --catalog/--plexdb path." >&2
  exit 1
fi

exec cargo run --quiet --bin taste-debug -- "${args[@]}"
