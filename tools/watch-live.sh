#!/usr/bin/env bash
# Open a stream URL in VLC. Trivial on purpose — the one place that knows how
# to launch VLC (which binary, which flags) so admin.toml's `watch` menu and
# ./tools/overlay-watch.sh don't each duplicate that knowledge.
#
# Usage:
#   ./tools/watch-live.sh http://127.0.0.1:8409/channel/10.m3u8
set -u

URL="${1:?usage: watch-live.sh <stream URL>}"
VLC="${VLC:-/Applications/VLC.app/Contents/MacOS/vlc}"

printf '\033[1m==>\033[0m opening %s in VLC\n' "$URL"
"$VLC" -q "$URL"
