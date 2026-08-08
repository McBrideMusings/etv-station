#!/usr/bin/env bash
# Run ONLY the station daemon — no etv-overlay build, no ETV-next binaries, no
# HTTP server, no IINA. For watching the daemon's own log (a tautulli.join
# event, a channel that keeps failing) or reproducing a config error, where the
# rest of the dev stack is dead weight.
#
# The one thing this shares with dev-run.sh is the reason it exists: the sample
# channels read ETV_TEST_MEDIA_DIR and friends at config-load time, so a bare
# `cargo run -p etv-station` exits 1 with a config error that never mentions
# .env. Sourcing .env is the whole job.
#
# Pass a different config as $1. Everything after it goes to the daemon, so
# flags like --list-folders work: ./tools/dev-station.sh examples/station.yaml --list-folders
set -u

STATION_CONFIG="${1:-examples/station.yaml}"
[ "$#" -gt 0 ] && shift

# The etv-next submodule supplies the `ersatztv-playout` crate this build needs.
# A fresh clone or new worktree has the directory but not the contents, and
# cargo's error never mentions submodules — same check dev-run.sh makes.
if [ ! -f etv-next/crates/ersatztv-playout/Cargo.toml ]; then
  echo "[station] etv-next submodule is not checked out; running git submodule update --init --recursive"
  if ! git submodule update --init --recursive; then
    echo "[station] submodule checkout failed; cannot build" >&2
    exit 1
  fi
fi

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

exec env ETV_STATION_TZ="${ETV_STATION_TZ:-UTC}" \
  cargo run -p etv-station -- --config "$STATION_CONFIG" "$@"
