#!/usr/bin/env bash
# Recreate the etv-station container from IMAGE. Runs ON the Unraid host, piped
# over ssh by tools/deploy.sh (`ssh host 'bash -s' < tools/deploy-container.sh`),
# so no copy of it lives on the host to go stale.
#
# Every value comes from ENV_FILE on the host, never from the deploy machine:
# the mount paths name host directories, and the tokens stay on the host.
#
# Inputs (environment): IMAGE, ENV_FILE, DRY_RUN=0|1, FORCE=0|1.
#
# Order, each step guarding the next:
#   drift gate  — refuse when the live container differs from the last deploy's
#                 snapshot on /boot (a hand edit, a GUI Apply), or when there is
#                 no snapshot to compare with, unless FORCE=1
#   preflight   — `etv-station --check-config` from the NEW image, mounts
#                 read-only; a refusal leaves the running container untouched
#   rollback    — tag the outgoing image :previous only while it is healthy, so
#                 two failed deploys in a row still roll back to a good build;
#                 write /tmp/etv-station-restore.sh
#   recreate    — stop, remove, chown the mount roots, docker run
#   health      — wait for /channels.m3u, then prune dangling images
set -euo pipefail

CONTAINER=etv-station
SNAPSHOT=/boot/config/$CONTAINER-lastdeploy.json
RESTORE=/tmp/$CONTAINER-restore.sh
HEALTH_TIMEOUT=240
HEALTH_INTERVAL=5

: "${IMAGE:?IMAGE is not set}"
: "${ENV_FILE:?ENV_FILE is not set}"
DRY_RUN=${DRY_RUN:-0}
FORCE=${FORCE:-0}

[ -r "$ENV_FILE" ] || { echo "deploy: cannot read $ENV_FILE on $(hostname)" >&2; exit 1; }
set -a
# shellcheck disable=SC1090
. "$ENV_FILE"
set +a

ETV_STATION_UID=${ETV_STATION_UID:-99}
ETV_STATION_GID=${ETV_STATION_GID:-100}
ETV_STATION_TZ=${ETV_STATION_TZ:-America/Chicago}

missing=()
for v in ETV_STATION_APPDATA ETV_STATION_DATA ETV_STATION_MEDIA ETV_PORT_HOST \
         ETV_IMAGES_DOMAIN ETV_IMAGES_TARGET_HOST; do
  [ -n "${!v:-}" ] || missing+=("$v")
done
[ ${#missing[@]} -eq 0 ] || { echo "deploy: $ENV_FILE does not set: ${missing[*]}" >&2; exit 1; }

# Passed through as NAME=value; an unset one goes in empty, which every reader
# treats as off (the diag switches) or reports at load (the URLs).
ENV_NAMES=(ETV_STATION_TZ PLEX_URL PLEX_TOKEN TAUTULLI_URL TAUTULLI_API_KEY
  RADARR_URL RADARR_API_KEY SONARR_URL SONARR_API_KEY PLEXDB_EX_URL
  ETV_ACCESS_LOG ETV_DIAG_CAPTURE ETV_NEXT_URL
  ETV_ACCEL ETV_VAAPI_DEVICE ETV_VAAPI_DRIVER LIBVA_DRIVER_NAME)

# The Pangolin resource that serves guide images publicly (Plex will not fetch
# an image from a private address): /logo and /artwork answer, everything else
# is denied. Change a rule here and redeploy, never in the Pangolin dashboard.
p=pangolin.public-resources.etv-images
LABELS=(
  "$p.name=etv-station guide images"
  "$p.full-domain=$ETV_IMAGES_DOMAIN"
  "$p.protocol=http"
  "$p.auth.sso-enabled=false"
  "$p.targets[0].hostname=$ETV_IMAGES_TARGET_HOST"
  "$p.targets[0].method=http"
  "$p.targets[0].port=$ETV_PORT_HOST"
  "$p.rules[0].action=allow" "$p.rules[0].match=path" "$p.rules[0].priority=1" "$p.rules[0].value=/logo/*"
  "$p.rules[1].action=allow" "$p.rules[1].match=path" "$p.rules[1].priority=2" "$p.rules[1].value=/artwork/*"
  "$p.rules[2].action=deny"  "$p.rules[2].match=path" "$p.rules[2].priority=3" "$p.rules[2].value=*"
)

# Everything `docker run` and the preflight share: the user, the Intel iGPU's
# render node (renderD129 is the discrete card, kept for model work), the three
# mounts and the env.
COMMON=(--user "$ETV_STATION_UID:$ETV_STATION_GID"
  --device /dev/dri/renderD128:/dev/dri/renderD128)
MOUNTS=("$ETV_STATION_APPDATA:/config" "$ETV_STATION_DATA:/data" "$ETV_STATION_MEDIA:/media:ro")
for n in "${ENV_NAMES[@]}"; do COMMON+=(-e "$n=${!n:-}"); done

RUN=(--restart unless-stopped "${COMMON[@]}" -p "$ETV_PORT_HOST:8409" -p 4747:4747)
for m in "${MOUNTS[@]}"; do RUN+=(-v "$m"); done
for l in "${LABELS[@]}"; do RUN+=(--label "$l"); done

# The preflight mounts everything read-only and carries no ports and no labels:
# it must not collide with the live container's ports or register a second
# Pangolin target.
PREFLIGHT=(docker run --rm --entrypoint etv-station "${COMMON[@]}")
for m in "${MOUNTS[@]}"; do
  case $m in *:ro) PREFLIGHT+=(-v "$m") ;; *) PREFLIGHT+=(-v "$m:ro") ;; esac
done
PREFLIGHT+=("$IMAGE" --config /config/station.yaml --check-config)

HEALTH_URL="http://127.0.0.1:$ETV_PORT_HOST/channels.m3u"
healthy() { [ "$(curl -s -o /dev/null -w '%{http_code}' "$HEALTH_URL" 2>/dev/null || true)" = 200 ]; }

# The fields a hand edit or a GUI Apply changes, in a form two inspects compare by.
shape() {
  jq -S '.[0] | {image: .Config.Image, user: .Config.User, env: (.Config.Env | sort),
    labels: .Config.Labels, binds: .HostConfig.Binds, ports: .HostConfig.PortBindings,
    devices: .HostConfig.Devices, restart: .HostConfig.RestartPolicy.Name}'
}

masked() {
  local a out=()
  for a in "$@"; do
    case $a in *TOKEN=*|*API_KEY=*) out+=("${a%%=*}=***") ;; *) out+=("$a") ;; esac
  done
  printf '%q ' "${out[@]}"
}

exists=0
docker inspect --type container "$CONTAINER" >/dev/null 2>&1 && exists=1

command -v jq >/dev/null || { echo "deploy: jq is not installed on $(hostname); the drift gate needs it" >&2; exit 1; }

# No snapshot means nothing can say whether the live container was edited by
# hand, so the gate refuses rather than passes.
if [ $exists = 1 ] && [ ! -s "$SNAPSHOT" ]; then
  echo "deploy: no $SNAPSHOT, so a hand edit to '$CONTAINER' cannot be told apart from the last deploy." >&2
  if [ "$FORCE" != 1 ]; then
    echo "deploy: refusing. Check the live container against the dry run's docker run line, then pass --force." >&2
    exit 1
  fi
  echo "deploy: --force: replacing '$CONTAINER' without a drift check" >&2
elif [ $exists = 1 ]; then
  if ! diff_out=$(diff <(shape <"$SNAPSHOT") <(docker inspect --type container "$CONTAINER" | shape)); then
    echo "deploy: '$CONTAINER' was changed since the last deploy (< last deploy, > live):" >&2
    echo "$diff_out" | sed -E 's/("[A-Z_]*(TOKEN|API_KEY)=)[^"]*/\1***/' >&2
    if [ "$FORCE" != 1 ]; then
      echo "deploy: refusing. Put the change in the deploy config, or pass --force to discard it." >&2
      exit 1
    fi
    echo "deploy: --force: discarding the live changes above" >&2
  fi
fi

if [ "$DRY_RUN" = 1 ]; then
  echo "== preflight (from the new image, before '$CONTAINER' is touched)"
  echo "  $(masked "${PREFLIGHT[@]}")"
  [ $exists = 1 ] && echo "== outgoing image would become ${IMAGE%%:*}:previous if '$CONTAINER' is healthy"
  echo "== docker run"
  echo "  $(masked docker run -d --name "$CONTAINER" "${RUN[@]}" "$IMAGE")"
  echo "== health: $HEALTH_URL, up to ${HEALTH_TIMEOUT}s"
  exit 0
fi

echo "deploy: preflight: checking the config with the new image"
if ! "${PREFLIGHT[@]}"; then
  echo "deploy: preflight failed; '$CONTAINER' was not replaced and is still running" >&2
  exit 1
fi

ROLLBACK=${IMAGE%%:*}:previous
if [ $exists = 1 ]; then
  outgoing=$(docker inspect --type container "$CONTAINER" --format '{{.Image}}')
  if healthy || ! docker image inspect "$ROLLBACK" >/dev/null 2>&1; then
    docker tag "$outgoing" "$ROLLBACK"
    healthy || echo "deploy: '$CONTAINER' is not healthy, but it is the only rollback candidate; tagged $ROLLBACK" >&2
  else
    echo "deploy: '$CONTAINER' is not healthy; $ROLLBACK stays on the last healthy build" >&2
  fi
  # rm -f first: by the time anyone runs this, the failed new container holds the name.
  { echo '#!/bin/sh'
    echo "docker rm -f $CONTAINER >/dev/null 2>&1"
    printf '%q ' docker run -d --name "$CONTAINER" "${RUN[@]}" "$ROLLBACK"; echo
  } >"$RESTORE.tmp"
  chmod 700 "$RESTORE.tmp" && mv "$RESTORE.tmp" "$RESTORE"
  echo "deploy: restore script: $RESTORE (starts $ROLLBACK)"

  docker stop -t 10 "$CONTAINER" >/dev/null
  docker rm "$CONTAINER" >/dev/null
fi

mkdir -p "$ETV_STATION_APPDATA" "$ETV_STATION_DATA"
chown "$ETV_STATION_UID:$ETV_STATION_GID" "$ETV_STATION_APPDATA" "$ETV_STATION_DATA"

if ! docker run -d --name "$CONTAINER" "${RUN[@]}" "$IMAGE" >/dev/null; then
  echo "deploy: container start failed; to restore: bash $RESTORE" >&2
  exit 1
fi
(umask 077 && docker inspect --type container "$CONTAINER" >"$SNAPSHOT")
echo "deploy: started '$CONTAINER' from $IMAGE"

echo "deploy: waiting for $HEALTH_URL (up to ${HEALTH_TIMEOUT}s)"
for _ in $(seq 1 $((HEALTH_TIMEOUT / HEALTH_INTERVAL))); do
  if healthy; then
    echo "deploy: healthy"
    docker image prune -f >/dev/null
    exit 0
  fi
  sleep "$HEALTH_INTERVAL"
done
echo "deploy: '$CONTAINER' started but never answered $HEALTH_URL" >&2
echo "  check:     docker logs $CONTAINER" >&2
[ $exists = 1 ] && echo "  roll back: bash $RESTORE" >&2
exit 1
