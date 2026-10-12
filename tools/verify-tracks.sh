#!/usr/bin/env bash
# Prove the track selector reaches the playout JSON (#261).
#
# Builds one short Matroska file with a default-flagged commentary track listed first, a main
# track not flagged default, and an English text subtitle; boots the station daemon
# against it (no Plex, no Tautulli); and asserts that the first emitted item
#
#   * plays the main audio, not the commentary the station config excludes,
#   * carries the text subtitle, which is what makes ETV-next offer the track,
#   * lands in channel1.json with the right subtitle.default_on.
#
# Picture subtitles (PGS, VobSub) cannot be synthesized with ffmpeg, so the burn
# rule is covered by unit tests in crates/etv-station/src/tracks.rs instead.
#
# Usage: tools/verify-tracks.sh        (needs ffmpeg and ffprobe on PATH)
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

for tool in ffmpeg ffprobe python3; do
  command -v "$tool" >/dev/null || { echo "FAIL: $tool not on PATH" >&2; exit 1; }
done

WORK="$(mktemp -d "${TMPDIR:-/tmp}/verify-tracks.XXXXXX")"
STATION_PID=""
cleanup() {
  [ -n "$STATION_PID" ] && kill "$STATION_PID" 2>/dev/null && wait "$STATION_PID" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

mkdir -p "$WORK/media" "$WORK/channels/t" "$WORK/out" "$WORK/etv"

cat > "$WORK/en.srt" <<'SRT'
1
00:00:01,000 --> 00:00:05,000
Hello from the English text track.
SRT

ffmpeg -y -v error \
  -f lavfi -i testsrc=size=320x180:rate=25:duration=20 \
  -f lavfi -i sine=frequency=440:duration=20 \
  -f lavfi -i sine=frequency=880:duration=20 \
  -i "$WORK/en.srt" \
  -map 0:v -map 1:a -map 2:a -map 3:s \
  -c:v libx264 -preset ultrafast -c:a aac -c:s srt \
  -metadata:s:a:0 language=eng -metadata:s:a:0 title="Director Commentary" \
  -metadata:s:a:1 language=eng -metadata:s:a:1 title="Main" \
  -disposition:a:0 default -disposition:a:1 0 \
  -metadata:s:s:0 language=eng \
  "$WORK/media/film.mkv" || { echo "FAIL: could not build the fixture" >&2; exit 1; }

cat > "$WORK/station.yaml" <<YAML
tz: UTC
output_base: $WORK/out
channels: [channels/t/channel.yaml]
catalog_path: $WORK/catalog.db
catalog_refresh_secs: 86400
normalization:
  audio: { format: aac, bitrate_kbps: 128, buffer_kbps: 256, channels: 2, sample_rate_hz: 48000, normalize_loudness: false }
  video: { format: h264, bit_depth: 8, width: 320, height: 180, bitrate_kbps: 400, buffer_kbps: 800 }
  subtitle: { mode: convert }
tracks:
  audio:
    exclude:
      - { title: "commentary|director" }
YAML

cat > "$WORK/channels/t/channel.yaml" <<YAML
number: 1
name: t
window_days: 1
chunk_hours: 1
roll_interval: 60s
retention_days: 1
subtitle: { default_on: true }
rule:
  blocks:
    - mode: all
      order: manual
      entries:
        - kind: item
          source: { kind: local, path: "$WORK/media/film.mkv" }
          program: { title: "Fixture" }
YAML

cargo build -q -p etv-station --bin etv-station || { echo "FAIL: build" >&2; exit 1; }
BIN="$(cargo metadata --format-version 1 --no-deps 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/etv-station"

"$BIN" --config "$WORK/station.yaml" --render-etv-next "$WORK/etv" >/dev/null 2>&1 \
  || { echo "FAIL: --render-etv-next" >&2; exit 1; }

env -u PLEX_URL -u PLEX_TOKEN -u TAUTULLI_URL -u TAUTULLI_API_KEY \
  "$BIN" --config "$WORK/station.yaml" > "$WORK/station.log" 2>&1 &
STATION_PID=$!

for _ in $(seq 1 60); do
  grep -q 'playout.first_emit' "$WORK/station.log" 2>/dev/null && break
  kill -0 "$STATION_PID" 2>/dev/null || break
  sleep 1
done

python3 - "$WORK" <<'PY'
import glob, json, re, sys

work = sys.argv[1]
failures = []

def check(ok, what):
    print(("PASS " if ok else "FAIL ") + what)
    if not ok:
        failures.append(what)

items = []
for path in sorted(glob.glob(work + "/out/t/*.json")):
    items += json.load(open(path)).get("items", [])
check(bool(items), "the station emitted playout items")
if items:
    tracks = items[0].get("tracks") or {}
    audio = (tracks.get("audio") or {}).get("stream_index")
    subtitle = (tracks.get("subtitle") or {}).get("stream_index")
    check(audio == 2, f"audio is stream 2, the main track, not the excluded commentary (got {audio})")
    check(subtitle == 3, f"subtitle is stream 3, the text track (got {subtitle})")

channel = json.load(open(work + "/etv/channel1.json"))
default_on = channel["normalization"]["subtitle"].get("default_on")
check(default_on is True, f"channel1.json announces the subtitle track as default (got {default_on})")

log = re.sub(r"\x1b\[[0-9;]*m", "", open(work + "/station.log").read())
check('event="tracks.assigned"' in log, "the station logged tracks.assigned")

sys.exit(1 if failures else 0)
PY
