#!/usr/bin/env bash
# Play one bumper script through the overlay-watch pipeline into VLC.
#
# Usage:
#   ./tools/bumper-watch.sh <channel> <bumper.rhai> [--gap 45s] [--next PATH|ENTRY_ID] [--channel-number N]
#   ./tools/bumper-watch.sh 054-dragon-ball examples/plugins/text-card-bumper.rhai --gap 30s
#
# <channel> picks the overlay canvas (size, framerate) the bumper airs on, the
# same way overlay-watch does. The bumper itself is resolved by
# `etv-station bumper-preview`; this is overlay-watch.sh in its --bumper mode.
set -u
CHANNEL="${1:?usage: bumper-watch.sh <channel> <bumper.rhai> [--gap 45s] [--next PATH|ENTRY_ID]}"
SCRIPT="${2:?usage: bumper-watch.sh <channel> <bumper.rhai> [--gap 45s] [--next PATH|ENTRY_ID]}"
shift 2
exec "$(dirname "$0")/overlay-watch.sh" "$CHANNEL" --bumper "$SCRIPT" "$@"
