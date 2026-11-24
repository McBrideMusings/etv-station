#!/usr/bin/env bash
# shellcheck disable=SC2029 # every ssh command here is built on this side on purpose
# Build and deploy etv-station to the Unraid host. `just build` and `just deploy` call this.
#
#   tools/deploy.sh build                    # build the image on the host, deploy nothing
#   tools/deploy.sh image [--dry-run] [--force]
#   tools/deploy.sh files [--dry-run]        # sync deploy/appdata only, then SIGHUP
#   tools/deploy.sh all   [--dry-run] [--force]
#
# The image builds on the host, never here: the git-tracked files go to
# BUILD_CONTEXT on the host, and the host's default builder is the capped
# buildbox-bk BuildKit container (~/Projects/buildbox). tools/deploy-container.sh
# then recreates the container from values in REMOTE_ENV_FILE on the host.
#
# Reads from .env (just loads it): UNRAID_HOST, UNRAID_USER, APP_IMAGE,
# ETV_REMOTE_ENV_FILE, ETV_BUILD_CONTEXT, ETV_STATION_BACKUP_DIR,
# ETV_STATION_BACKUP_KEEP.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
target=${1:-}
shift || true
dry=0 force=0
for a in "$@"; do
  case $a in
    --dry-run) dry=1 ;;
    --force) force=1 ;;
    *) echo "deploy: unknown flag '$a' (expected --dry-run or --force)" >&2; exit 2 ;;
  esac
done
case $target in
  build|backup|image|files|all) ;;
  *) echo "usage: tools/deploy.sh build|backup|image|files|all [--dry-run] [--force]" >&2; exit 2 ;;
esac

: "${UNRAID_HOST:?UNRAID_HOST is not set (.env)}"
: "${APP_IMAGE:?APP_IMAGE is not set (.env)}"
: "${ETV_REMOTE_ENV_FILE:?ETV_REMOTE_ENV_FILE is not set (.env): the env file on the host the container is built from}"
: "${ETV_BUILD_CONTEXT:?ETV_BUILD_CONTEXT is not set (.env): the directory on the host the image builds from}"
host=${UNRAID_USER:-root}@$UNRAID_HOST

# One value from the host's env file, so the mount paths are read where they apply.
remote_var() {
  ssh "$host" "set -a; . $(printf %q "$ETV_REMOTE_ENV_FILE"); printf %s \"\${$1:-}\""
}

backup() {
  local if_stale=$1 appdata data
  appdata=$(remote_var ETV_STATION_APPDATA)
  data=$(remote_var ETV_STATION_DATA)
  # A deploy passes 1: a verified snapshot from the last 10 minutes is enough,
  # so a retried deploy does not churn the retention window. `just backup` passes 0.
  ssh "$host" ETV_STATION_BACKUP_IF_STALE="$if_stale" \
    ETV_STATION_APPDATA="$(printf %q "$appdata")" ETV_STATION_DATA="$(printf %q "$data")" \
    ETV_STATION_BACKUP_DIR="$(printf %q "${ETV_STATION_BACKUP_DIR:?ETV_STATION_BACKUP_DIR is not set (.env)}")" \
    ETV_STATION_BACKUP_KEEP="${ETV_STATION_BACKUP_KEEP:-10}" \
    bash -s <"$repo/tools/backup-station.sh"
}

build() {
  local revision created ctx
  revision=$(git -C "$repo" rev-parse HEAD)
  [ -z "$(git -C "$repo" status --porcelain --untracked-files=no)" ] || revision=$revision-dirty
  created=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  ctx=$(printf %q "$ETV_BUILD_CONTEXT")
  # CARGO_BUILD_JOBS: docker/cargo-jobs.sh sizes jobs from /proc/meminfo, which
  # inside the capped builder shows the host's memory, not the builder's 6 GB.
  local build_cmd
  build_cmd="docker build --build-arg ETV_STATION_REVISION=$revision --build-arg ETV_STATION_CREATED=$created --build-arg CARGO_BUILD_JOBS=4 -t $(printf %q "$APP_IMAGE") $ctx"
  if [ $dry = 1 ]; then
    echo "== sync git-tracked files to $host:$ETV_BUILD_CONTEXT"
    echo "== build: ssh $host $build_cmd"
    return
  fi
  echo "deploy: syncing git-tracked files to $host:$ETV_BUILD_CONTEXT"
  ssh "$host" "rm -rf $ctx && mkdir -p $ctx"
  git -C "$repo" ls-files -z | rsync -a --from0 --ignore-missing-args --files-from=- "$repo/" "$host:$ETV_BUILD_CONTEXT/"
  echo "deploy: building $APP_IMAGE at $revision on $host"
  ssh "$host" "$build_cmd"
}

install_template() {
  # The Unraid GUI's Edit form and "force update" rebuild the container from
  # this file. Non-fatal: the container is what serves, the template only describes it.
  local src=$repo/deploy/unraid-template.xml
  local dst=/boot/config/plugins/dockerMan/templates-user/my-etv-station.xml
  [ -f "$src" ] || { echo "deploy: no $src, skipping the Unraid template" >&2; return 0; }
  if [ $dry = 1 ]; then echo "== template: $src -> $host:$dst"; return; fi
  scp -q "$src" "$host:$dst.tmp" && ssh "$host" "chmod 600 $dst.tmp && mv $dst.tmp $dst" \
    || echo "deploy: installing the Unraid template failed (non-fatal)" >&2
}

container() {
  ssh "$host" IMAGE="$(printf %q "$APP_IMAGE")" ENV_FILE="$(printf %q "$ETV_REMOTE_ENV_FILE")" \
    DRY_RUN="$dry" FORCE="$force" bash -s <"$repo/tools/deploy-container.sh"
}

# deploy/appdata mirrors the container's /config. --delete reaches only the
# paths this repo owns end to end: /config also holds data/, etv-next/ and the
# rest of what the container writes, and the catch-all exclude keeps rsync
# from deleting any of it.
files() {
  local appdata uid gid
  # deploy/appdata is gitignored, so a worktree or fresh clone may lack any of
  # it, and --delete removes on the host whatever owned path is missing here:
  # a missing .device_id would drop Plex's mapping for every channel.
  local p
  for p in channels plugins shared station.yaml .device_id; do
    [ -e "$repo/deploy/appdata/$p" ] || {
      echo "deploy: $repo/deploy/appdata/$p is missing, and --delete would remove it on the host; run files deploys from the checkout that holds the prod config" >&2
      exit 1
    }
  done
  appdata=$(remote_var ETV_STATION_APPDATA)
  uid=$(remote_var ETV_STATION_UID); gid=$(remote_var ETV_STATION_GID)
  [ -n "$appdata" ] || { echo "deploy: $ETV_REMOTE_ENV_FILE on $host does not set ETV_STATION_APPDATA" >&2; exit 1; }
  local flags=(-az --delete --exclude README.md
    --include /channels --include '/channels/**'
    --include /plugins --include '/plugins/**'
    --include /shared --include '/shared/**'
    --include /station.yaml --include /.device_id
    --exclude '*')
  [ $dry = 1 ] && flags+=(--dry-run -v)
  echo "deploy: syncing deploy/appdata to $host:$appdata"
  rsync "${flags[@]}" "$repo/deploy/appdata/" "$host:$appdata/"
  [ $dry = 1 ] && return
  # rsync carries this Mac's uid onto the directories it creates, and the
  # container runs as 99:100. The daemon re-reads its config only on SIGHUP.
  ssh "$host" "chown -R ${uid:-99}:${gid:-100} $(printf %q "$appdata") && if docker ps -q -f name=^etv-station\$ | grep -q .; then docker exec etv-station pkill -HUP etv-station; fi" \
    || echo "deploy: the chown or SIGHUP after the sync failed; the files are on the host" >&2
}

case $target in
  build) build ;;
  backup) backup 0 ;;
  files) files ;;
  image|all)
    # The container step's dry run checks the host env file and the drift gate
    # before the backup and the build spend minutes on a deploy that would refuse.
    if [ $dry = 0 ]; then
      dry=1 container >/dev/null || { echo "deploy: the container step refuses (above); nothing was changed" >&2; exit 1; }
      backup 1
    fi
    [ "$target" = all ] && files
    build
    install_template
    container
    ;;
esac
