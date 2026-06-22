#!/usr/bin/env bash
# Run etv-station + etv-next together for the integration test.
# Output from each process is prefixed with [station] / [etv].
# Ctrl-C stops both. HLS + EPG endpoints listed below.
set -u

# Job control: each backgrounded subshell becomes its own process-group leader,
# so the EXIT/INT trap can signal the whole tree (including ffmpeg grandchildren
# spawned by ersatztv-channel) instead of only the direct children.
set -m

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

: "${ETV_BIND_ADDRESS:=0.0.0.0}"
: "${ETV_PORT:=8409}"
export ETV_BIND_ADDRESS ETV_PORT

STATION_CONFIG="examples/station.toml"

# Pre-create every channel's output_folder referenced by the station config so
# etv-next's startup canonicalize doesn't hard-error on a fresh checkout.
mkdir -p tmp/hls

station_dir="$(dirname "$STATION_CONFIG")"
output_folders=()
while IFS= read -r rel; do
  channel_file="$station_dir/$rel"
  folder="$(awk -F '"' '/^output_folder/ {print $2; exit}' "$channel_file")"
  if [ -n "$folder" ]; then
    mkdir -p "$folder"
    output_folders+=("$folder")
  fi
done < <(awk -F '"' '/^path/ {print $2}' "$STATION_CONFIG")

trap 'for pid in $(jobs -p); do kill -TERM -- "-$pid" 2>/dev/null; done' EXIT INT TERM

# Pre-build etv-overlay so the station daemon can spawn it as a sibling binary
# the moment a channel becomes "watched". Without this the supervisor logs a
# spawn failure on the first few heartbeats.
echo "[dev] building etv-overlay..."
cargo build -p etv-overlay 2>&1 \
  | while IFS= read -r l; do printf '[station] %s\n' "$l"; done

bash "$(dirname "$0")/render-lineup.sh"

cat <<EOF
[dev] streams will appear at (point your IPTV app at the .m3u lineup):
[dev]   http://localhost:${ETV_PORT}/channels.m3u
[dev]   http://127.0.0.1:${ETV_PORT}/channels.m3u
[dev]   http://127.0.0.1:${ETV_PORT}/channel/1.m3u8
[dev]   http://127.0.0.1:${ETV_PORT}/xmltv.xml
EOF

(
  ETV_STATION_TZ="${ETV_STATION_TZ:-UTC}" \
    cargo run -p etv-station -- --config "$STATION_CONFIG" 2>&1 \
    | while IFS= read -r l; do printf '[station] %s\n' "$l"; done
) &

# Build both etv-next binaries up-front so the channel subprocess exists when
# the server's `ChannelSession::spawn` looks for it as a sibling executable.
echo "[dev] building etv-next binaries..."
cargo build --manifest-path etv-next/Cargo.toml --bin ersatztv --bin ersatztv-channel 2>&1 \
  | while IFS= read -r l; do printf '[etv] %s\n' "$l"; done

# Wait for station to emit its first playout JSON in every channel folder
# before launching etv-next. Otherwise etv-next's loader spams "unable to find
# playout JSON file for time …" until station catches up on cold builds.
if [ "${#output_folders[@]}" -gt 0 ]; then
  for folder in "${output_folders[@]}"; do
    echo "[dev] waiting for station to emit first playout JSON in $folder..."
    deadline=$((SECONDS + 60))
    while [ "$SECONDS" -lt "$deadline" ]; do
      if compgen -G "$folder/*.json" >/dev/null 2>&1; then
        break
      fi
      sleep 0.5
    done
    if ! compgen -G "$folder/*.json" >/dev/null 2>&1; then
      echo "[dev] WARNING: timed out waiting for $folder/*.json — launching etv-next anyway" >&2
    fi
  done
fi

(
  etv-next/target/debug/ersatztv examples/etv-next/lineup.json 2>&1 \
    | while IFS= read -r l; do printf '[etv] %s\n' "$l"; done
) &

# Once etv-next is serving the lineup, point IINA at the channel list so the
# channels + overlays can be eyeballed live. IINA loads the .m3u as a playlist
# (one entry per channel). Set OPEN_IINA=0 to skip — e.g. for headless
# validation runs that only curl/ffprobe the endpoints.
#
# `open -a IINA <url>` goes through LaunchServices, which routes the open to an
# already-running IINA instead of starting a second copy — so repeated dev-runs
# reuse the one instance rather than stacking up duplicate apps (verified: pid
# stays the same across opens). Do NOT use iina-cli here: it execs the IINA
# binary directly and forks a fresh instance every time. We don't try to detect
# what IINA is currently playing — reuse just replaces it with our lineup.
if [ "${OPEN_IINA:-1}" != "0" ]; then
  (
    lineup_url="http://127.0.0.1:${ETV_PORT}/channels.m3u"
    for _ in $(seq 1 60); do
      if curl -fsS -o /dev/null --max-time 2 "$lineup_url"; then
        echo "[dev] opening lineup in IINA (reusing existing instance) -> $lineup_url"
        open -a IINA "$lineup_url"
        break
      fi
      sleep 1
    done
  ) &
fi

wait
