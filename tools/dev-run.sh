#!/usr/bin/env bash
# Run etv-station + etv-next together for the integration test.
# Output from each process is prefixed with [station] / [etv].
# Ctrl-C stops both. HLS + EPG endpoints listed below.
set -u

# Job control: each backgrounded subshell becomes its own process-group leader,
# so the EXIT/INT trap can signal the whole tree (including ffmpeg grandchildren
# spawned by ersatztv-channel) instead of only the direct children.
set -m

# The etv-next submodule supplies the `ersatztv-playout` crate every build here
# depends on. A fresh clone or a new git worktree has the directory but not the
# contents, and cargo's failure ("failed to read etv-next/crates/.../Cargo.toml")
# says nothing about submodules — so check it explicitly and fix it in place.
if [ ! -f etv-next/crates/ersatztv-playout/Cargo.toml ]; then
  echo "[dev] etv-next submodule is not checked out; running git submodule update --init --recursive"
  if ! git submodule update --init --recursive; then
    echo "[dev] submodule checkout failed; cannot build without etv-next" >&2
    exit 1
  fi
fi

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

: "${ETV_BIND_ADDRESS:=0.0.0.0}"
: "${ETV_PORT:=8409}"
export ETV_BIND_ADDRESS ETV_PORT

STATION_CONFIG="examples/station.yaml"

mkdir -p tmp/hls

# Ask the station binary for each channel's resolved output_folder, for the
# readiness poll below. Going through the daemon's own config loader (rather
# than parsing TOML here) means the folders we poll can never disagree with
# where the daemon actually writes — nested tables, single-quoted strings, or a
# reformat can't drift the two apart (#35). `-q` keeps cargo's build chatter off
# stdout; the daemon build it triggers is needed a moment later anyway. A
# non-zero exit means the config won't load — the daemon would choke on it too,
# so fail fast instead of booting a doomed stack.
if ! folders_output="$(cargo run -q -p etv-station -- --config "$STATION_CONFIG" --list-folders)"; then
  echo "[dev] station --list-folders failed — $STATION_CONFIG won't load; aborting" >&2
  exit 1
fi
output_folders=()
while IFS= read -r folder; do
  [ -n "$folder" ] && output_folders+=("$folder")
done <<< "$folders_output"

# Teardown: TERM the whole process tree, then escalate to KILL after a 1s grace
# for any group (e.g. an ffmpeg child stuck on a flush) that ignored TERM, so a
# misbehaving child can't leave the script hanging on Ctrl-C. The trap is
# disarmed on entry so an INT doesn't also re-run this via EXIT (which would
# double the sleep), and we return early when nothing is running so a clean exit
# doesn't pause.
cleanup() {
  trap - EXIT INT TERM
  local pids
  pids=$(jobs -p)
  [ -z "$pids" ] && return
  for pid in $pids; do kill -TERM -- "-$pid" 2>/dev/null; done
  sleep 1
  for pid in $pids; do kill -KILL -- "-$pid" 2>/dev/null; done
}
trap cleanup EXIT INT TERM

# Pre-build etv-overlay so the station daemon can spawn it as a sibling binary
# the moment a channel becomes "watched". Without this the supervisor logs a
# spawn failure on the first few heartbeats.
echo "[dev] building etv-overlay..."
cargo build -p etv-overlay 2>&1 \
  | while IFS= read -r l; do printf '[station] %s\n' "$l"; done

# Generate ETV-next's lineup.json + channelN.json from the station config, so the
# playout folders it reads are derived from where the station writes (never
# hand-authored to match). The station binary was just built by --list-folders
# above, so this reuses it.
STATION_CONFIG="$STATION_CONFIG" python3 "$(dirname "$0")/render-etv-next.py" \
  | while IFS= read -r l; do printf '[dev] %s\n' "$l"; done

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

# Wait (up to 60s) for the station to emit its first playout JSON in every
# channel folder. Otherwise etv-next's loader spams "unable to find playout JSON
# file for time …" until station catches up on cold builds.
#
# A single foreground loop polls all folders each tick and drops them as they
# become ready, so the readiness window is max(per-folder), not sum — one slow
# channel (e.g. a cold ffprobe cache fill) no longer blocks the others. This
# deliberately avoids backgrounded per-folder jobs: under `set -m` (load-bearing
# for the teardown trap) each finishing poll emits a job-control "[n]+ Done"
# notice, cluttering the otherwise-clean prefixed output (#89). The glob is a
# cheap filesystem check with no per-folder blocking work, so folding the polls
# into one process costs nothing.
wait_for_folders() {
  local deadline=$((SECONDS + 60))
  local pending=("$@")
  local still folder
  echo "[dev] waiting for station to emit first playout JSON in ${#pending[@]} folder(s)..."
  while [ "${#pending[@]}" -gt 0 ] && [ "$SECONDS" -lt "$deadline" ]; do
    still=()
    for folder in "${pending[@]}"; do
      compgen -G "$folder/*.json" >/dev/null 2>&1 || still+=("$folder")
    done
    if [ "${#still[@]}" -eq 0 ]; then
      return 0
    fi
    pending=("${still[@]}")
    sleep 0.5
  done
  [ "${#pending[@]}" -eq 0 ] && return 0
  echo "[dev] WARNING: timed out waiting for ${pending[*]} — launching etv-next anyway" >&2
}

if [ "${#output_folders[@]}" -gt 0 ]; then
  wait_for_folders "${output_folders[@]}"
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
