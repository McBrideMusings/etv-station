#!/usr/bin/env bash
# Build the etv-station image stamped with the commit it is built from (#362).
#
# Passes the checkout's HEAD sha and the build time as the build args the
# Dockerfile turns into the org.opencontainers.image.revision/.created labels
# and the ETV_STATION_REVISION/ETV_STATION_CREATED env that ersatztv serves at
# /build.json. A dirty tree gets a `-dirty` suffix, so a stamp never claims a
# commit the image does not match.
#
# Usage: tools/build-image.sh <image-tag> [extra docker build args...]
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
tag=${1:?usage: tools/build-image.sh <image-tag> [docker build args...]}
shift

revision=$(git -C "$repo_root" rev-parse HEAD)
if [ -n "$(git -C "$repo_root" status --porcelain --untracked-files=no)" ]; then
  revision="$revision-dirty"
fi
created=$(date -u +%Y-%m-%dT%H:%M:%SZ)

exec docker build --platform linux/amd64 \
  --build-arg "ETV_STATION_REVISION=$revision" \
  --build-arg "ETV_STATION_CREATED=$created" \
  -t "$tag" "$@" "$repo_root"
