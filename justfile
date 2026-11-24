# etv-station tasks. Bare `just` lists them, each with the comment above it.
# Values (hosts, paths, tokens) come from .env, which `set dotenv-load` reads; see .env.example.
# Cargo builds and tests run on this Mac (cargo is Turnstile's shim); image builds run on Tower's capped builder.

set dotenv-load
set positional-arguments
set quiet

remote := "${UNRAID_USER:-root}@${UNRAID_HOST:?UNRAID_HOST is not set (.env)}"
exec := "docker exec etv-station etv-station --config ${ETV_STATION_CONFIG:-/config/station.yaml}"

_default:
    @just --list --unsorted

# --- lifecycle ---

# Build the image on Tower's capped builder, stamped with HEAD's sha
build:
    tools/deploy.sh build

# Run the station: `full` (daemon + ETV-next) or `station` (the daemon alone); extra args reach the script
dev what="full" *args:
    #!/usr/bin/env bash
    set -euo pipefail
    what=$1; shift
    case $what in
      full) exec ./tools/dev-run.sh "$@" ;;
      station) exec ./tools/dev-station.sh "$@" ;;
      *) echo "just dev full|station" >&2; exit 2 ;;
    esac

# Back up, build and recreate the container: image (default), files (deploy/appdata only) or all; --dry-run, --force
deploy target="image" *flags:
    tools/deploy.sh "$@"

# --- checks ---

# Tests in both cargo workspaces
test:
    ./tools/verify-all.sh test

# Clippy (deny warnings) and rustfmt --check in both workspaces
vet:
    ./tools/verify-all.sh lint

# Format both workspaces; it rewrites files
fmt:
    ./tools/verify-all.sh fmt

# docs/ anchor links resolve
docs-check:
    ./tools/verify-all.sh docs

# Serve the VitePress docs on http://localhost:5193
docs:
    bun run docs:dev

# Kill leftover dev processes: station, etv-next, overlays, ffmpeg
kill:
    ./tools/kill-dev.sh

# --- the deployed station ---

# Tail the container's log (extra args reach docker logs, e.g. --since 1h)
logs *args:
    ssh {{remote}} docker logs -f --tail 200 "$@" etv-station

# Tail the in-container stream diagnostics: the access log and the event watcher
diag:
    ssh {{remote}} tail -f "${ETV_STATION_DATA:?}/diag/access.log" "${ETV_STATION_DATA:?}/diag/stream-events.log"

# Snapshot history.db, catalog.db, .device_id and config on the host
backup:
    tools/deploy.sh backup

# Why each upcoming item is on a channel's schedule: bare lists channels; <channel> [--next N]
audit *args:
    tools/audit-report.sh "$@"

# The catalog facts for one entry as JSON: <entry_id> [--prod]
facts *args:
    tools/audio-facts.sh "$@"

# Open a channel's generated playout in a Wheel pane: <channel> [--days N] [--prod]
playout *args:
    tools/playout-view.sh "$@"

# Repair the play-history ledger from the playout files on disk: `history [--dry-run]`
repair what *args:
    #!/usr/bin/env bash
    set -euo pipefail
    [ "$1" = history ] || { echo "just repair history [--dry-run]" >&2; exit 2; }
    shift
    ssh {{remote}} {{exec}} --backfill-history "$@"

# Would this branch's binary load the plugin scripts prod has?
plugin-check:
    UNRAID_USER="${UNRAID_USER:-root}" tools/plugin-check.sh

# Which commit prod runs, and how far behind this checkout it is
image-age:
    tools/image-age.sh

# List the configured event hooks (station and channel hooks:)
hooks:
    ./tools/dev-station.sh examples/station.yaml --list-hooks

# Verify a production invariant: `accel` (GPU encode) or `slivers` (no sub-segment ffmpeg spawns)
verify what:
    #!/usr/bin/env bash
    set -euo pipefail
    case $1 in
      accel) tools/ui-get.sh /api/health/channels --prod | jq -e '.channels | map(select(.encoder.verdict == "FAIL") | {channel, key, reason: .encoder.reason}) | if length == 0 then "no channel FAILs" else (., false) end' ;;
      slivers) UNRAID_USER="${UNRAID_USER:-root}" tools/verify-no-slivers.sh ;;
      *) echo "just verify accel|slivers" >&2; exit 2 ;;
    esac

# Channels failing or failed on the deployed station (exit 1 if any failed)
channel-health:
    #!/usr/bin/env bash
    set -euo pipefail
    tools/ui-get.sh /api/health/channels --prod | jq -e '.channels | map(select(.failed == true) | {channel, key, failures, retry_in_secs, last_failure}) | if length == 0 then "no channel failed" else (., false) end'

# Tell the deployed daemon to redo something: `catalog` (full re-ingest), `channel <n|folder|all>`, `config` (SIGHUP)
refresh what *args:
    #!/usr/bin/env bash
    set -euo pipefail
    what=$1; shift
    # Signal the daemon, never PID 1: tini forwards to entrypoint.sh, which exits on a signal it does not trap.
    case $what in
      catalog) ssh {{remote}} docker exec etv-station pkill -USR1 etv-station ;;
      channel) ssh {{remote}} {{exec}} --refresh-channel "$@" ;;
      config) ssh {{remote}} docker exec etv-station pkill -HUP etv-station ;;
      *) echo "just refresh catalog|channel <n>|config" >&2; exit 2 ;;
    esac

# SIGHUP the deployed daemon so it re-reads config without restarting
reload:
    ssh {{remote}} docker exec etv-station pkill -HUP etv-station

# The station dashboard: `ui` runs it locally (:4747); `ui get <path> [--prod]` prints one endpoint's JSON
ui *args:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "${1:-}" = get ]; then shift; exec tools/ui-get.sh "$@"; fi
    exec ./tools/station-ui.sh "$@"
