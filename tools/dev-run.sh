#!/usr/bin/env bash
# Run etv-station + etv-next together for the integration test.
# Output from each process is prefixed with [station] / [etv].
# Ctrl-C stops both. HLS + EPG endpoints listed below.
set -u

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

: "${ETV_BIND_ADDRESS:=0.0.0.0}"
: "${ETV_PORT:=8409}"
export ETV_BIND_ADDRESS ETV_PORT

mkdir -p tmp/hls examples/output/test

trap 'jobs -p | xargs -r kill 2>/dev/null' EXIT INT TERM

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
    cargo run -p etv-station -- --config examples/station.toml 2>&1 \
    | while IFS= read -r l; do printf '[station] %s\n' "$l"; done
) &

# Give station a head start so it has playout files before etv-next looks for them.
sleep 2

# Build both etv-next binaries up-front so the channel subprocess exists when
# the server's `ChannelSession::spawn` looks for it as a sibling executable.
echo "[dev] building etv-next binaries..."
cargo build --manifest-path etv-next/Cargo.toml --bin ersatztv --bin ersatztv-channel 2>&1 \
  | while IFS= read -r l; do printf '[etv] %s\n' "$l"; done

(
  etv-next/target/debug/ersatztv examples/etv-next/lineup.json 2>&1 \
    | while IFS= read -r l; do printf '[etv] %s\n' "$l"; done
) &

wait
