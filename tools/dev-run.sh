#!/usr/bin/env bash
# Run etv-station + etv-next together for the integration test.
# Output from each process is prefixed with [station] / [etv].
# Ctrl-C stops both. HLS + EPG endpoints listed below.
set -u

mkdir -p tmp/hls examples/output/test

trap 'jobs -p | xargs -r kill 2>/dev/null' EXIT INT TERM

cat <<EOF
[dev] streams will appear at:
[dev]   http://127.0.0.1:8409/channels.m3u
[dev]   http://127.0.0.1:8409/channel/1.m3u8
[dev]   http://127.0.0.1:8409/xmltv.xml
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
