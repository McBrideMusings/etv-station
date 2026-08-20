#!/usr/bin/env bash
# Run etv-station + etv-next together for the integration test.
# Output from each process is prefixed with [station] / [etv].
# Ctrl-C stops both. HLS + EPG endpoints listed below.
set -u

# Job control: each backgrounded subshell becomes its own process-group leader,
# so the EXIT/INT trap can signal the whole tree (including ffmpeg grandchildren
# spawned by ersatztv-channel) instead of only the direct children.
set -m

# shellcheck source=tools/dev-procs.sh
. "$(dirname "$0")/dev-procs.sh"

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

: "${ETV_BIND_ADDRESS:=0.0.0.0}"
: "${ETV_PORT:=8409}"
export ETV_BIND_ADDRESS ETV_PORT

: "${STATION_CONFIG:=examples/station.yaml}"

mkdir -p tmp/hls

# Pre-flight: never stack a second dev stack on top of an existing one.
#
# No exit trap can be complete. SIGKILL runs no handler at all, and closing the
# terminal delivers SIGHUP, which the station daemon deliberately treats as
# "reload the config" rather than "shut down" (see spawn_signal_listener in
# crates/etv-station/src/daemon.rs) — so on a window close bash, cargo and
# ersatztv all die on the default HUP action while etv-station reloads and keeps
# running, reparented to PID 1. Every such leak used to go unnoticed until the
# next run silently added a second daemon writing the same playout folders.
# Teardown is therefore best-effort by construction; the guarantee has to come
# from an idempotent startup, which is this check.
#
# Reparenting to PID 1 is what distinguishes the two cases: a leftover has no
# living parent, while a stack running in another terminal is still a child of
# its own dev-run. Orphans get killed, a live stack aborts this run — we will
# not tear down a session the user is actually watching.
preflight_stale_procs() {
  local orphans=() live=() entry label kind pattern pid ppid
  for entry in "${DEV_PROCS[@]}"; do
    IFS='|' read -r label kind pattern <<< "$entry"
    while IFS= read -r pid; do
      [ -n "$pid" ] || continue
      ppid=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')
      if [ "$ppid" = "1" ]; then
        orphans+=("$pid ($label)")
      else
        live+=("$pid ($label)")
      fi
    done <<< "$(dev_proc_pids "$kind" "$pattern")"
  done

  if [ "${#live[@]}" -gt 0 ]; then
    echo "[dev] a dev stack is already running in another terminal:" >&2
    printf '[dev]   pid %s\n' "${live[@]}" >&2
    echo "[dev] stop it there (Ctrl-C), or run ./tools/kill-dev.sh, then retry" >&2
    exit 1
  fi

  [ "${#orphans[@]}" -eq 0 ] && return 0
  echo "[dev] cleaning up ${#orphans[@]} orphaned process(es) from a previous run:"
  printf '[dev]   pid %s\n' "${orphans[@]}"
  # Kill by PID, not by process group: an orphan's group id belonged to a shell
  # that is long gone and the kernel may have since handed it to an unrelated
  # process. DEV_PROCS already enumerates the children (ffmpeg, ffprobe, the
  # overlay renderers) individually, so per-PID kills lose nothing.
  for entry in "${orphans[@]}"; do kill -TERM "${entry%% *}" 2>/dev/null; done
  sleep 1
  for entry in "${orphans[@]}"; do kill -KILL "${entry%% *}" 2>/dev/null; done
}
preflight_stale_procs

# Ask the station binary for each channel's resolved output_folder, for the
# readiness poll below. Going through the daemon's own config loader (rather
# than parsing TOML here) means the folders we poll can never disagree with
# where the daemon actually writes — nested tables, single-quoted strings, or a
# reformat can't drift the two apart (#35). `-q` keeps cargo's build chatter off
# stdout; the daemon build it triggers is needed a moment later anyway. A
# non-zero exit means the config won't load — the daemon would choke on it too,
# so fail fast instead of booting a doomed stack.
if ! folders_output="$(cargo run -q -p etv-station --bin etv-station -- --config "$STATION_CONFIG" --list-folders)"; then
  echo "[dev] station --list-folders failed — $STATION_CONFIG won't load; aborting" >&2
  exit 1
fi
output_folders=()
while IFS= read -r folder; do
  [ -n "$folder" ] && output_folders+=("$folder")
done <<< "$folders_output"

# Create every channel's output folder before either process starts.
#
# ETV-next resolves each channel's playout folder once, at boot, and silently
# drops any channel whose folder is missing ("unable to resolve playout folder
# …: No such file or directory") — the channel then stays out of the lineup for
# the whole session even after the station creates the folder moments later.
# Whether a channel appears therefore used to depend on a race: the station
# creates a folder only when it first writes to it, and anything that delays
# that first write past ETV-next's boot (a cold catalog ingest, a slow query, a
# plugin ranking a large library) silently cost us channels. A channel existing
# is a fact of the config, not of how fast its first generation ran, so the
# folders are made here, up front, for all of them. An empty folder resolves
# fine; ETV-next just reports no playout for the current time until the station
# fills it, which is what wait_for_first_emit below is for.
if [ "${#output_folders[@]}" -gt 0 ]; then
  for folder in "${output_folders[@]}"; do mkdir -p "$folder"; done
fi

# Teardown: TERM the whole process tree, then escalate to KILL after a 1s grace
# for any group (e.g. an ffmpeg child stuck on a flush) that ignored TERM, so a
# misbehaving child can't leave the script hanging on Ctrl-C. The trap is
# disarmed on entry so an INT doesn't also re-run this via EXIT (which would
# double the sleep), and we return early when nothing is running so a clean exit
# doesn't pause.
#
# HUP is trapped alongside INT/TERM because closing the terminal is a routine
# way to end a dev session, and bash runs the EXIT trap only for signals it
# actually traps — an untrapped HUP kills the script with no cleanup at all,
# which is exactly how orphaned station daemons accumulated.
cleanup() {
  trap - EXIT INT TERM HUP
  local pids
  pids=$(jobs -p)
  [ -z "$pids" ] && return
  for pid in $pids; do kill -TERM -- "-$pid" 2>/dev/null; done
  sleep 1
  for pid in $pids; do kill -KILL -- "-$pid" 2>/dev/null; done
}
trap cleanup EXIT INT TERM HUP

# Pre-build etv-overlay so the station daemon can spawn it as a sibling binary
# the moment a channel becomes "watched". Without this the supervisor logs a
# spawn failure on the first few heartbeats.
echo "[dev] building etv-overlay..."
cargo build -p etv-overlay 2>&1 \
  | while IFS= read -r l; do printf '[station] %s\n' "$l"; done

# Generate ETV-next's lineup.json + channelN.json from the station config, so the
# playout folders it reads are derived from where the station writes (never
# hand-authored to match). Same binary, same flag the container entrypoint runs,
# so dev and prod render through one code path.
cargo run -q -p etv-station --bin etv-station -- \
  --config "$STATION_CONFIG" --render-etv-next "${ETV_NEXT_DIR:-examples/etv-next}" \
  | while IFS= read -r l; do printf '[dev] %s\n' "$l"; done

cat <<EOF
[dev] streams will appear at (point your IPTV app at the .m3u lineup):
[dev]   http://localhost:${ETV_PORT}/channels.m3u
[dev]   http://127.0.0.1:${ETV_PORT}/channels.m3u
[dev]   http://127.0.0.1:${ETV_PORT}/channel/1.m3u8
[dev]   http://127.0.0.1:${ETV_PORT}/xmltv.xml
EOF

# The station's output is teed to a log so the readiness wait below can read its
# structured events (#38) without stealing the stream from the console. The exit
# sentinel is appended to the log only — never printed — and is what turns "the
# station died" into an immediate failure instead of a 180s timeout.
station_log="tmp/dev-run-station.log"
station_exit_marker="__station-exited__"
: > "$station_log"

(
  ETV_STATION_TZ="${ETV_STATION_TZ:-UTC}" \
    cargo run -p etv-station --bin etv-station -- --config "$STATION_CONFIG" 2>&1 \
    | tee -a "$station_log" \
    | while IFS= read -r l; do printf '[station] %s\n' "$l"; done
  printf '%s %s\n' "$station_exit_marker" "${PIPESTATUS[0]}" >> "$station_log"
) &

# Build both etv-next binaries up-front so the channel subprocess exists when
# the server's `ChannelSession::spawn` looks for it as a sibling executable.
echo "[dev] building etv-next binaries..."
cargo build --manifest-path vendor/etv-next/Cargo.toml --bin ersatztv --bin ersatztv-channel 2>&1 \
  | while IFS= read -r l; do printf '[etv] %s\n' "$l"; done

# Wait for the station to report that every channel's playout window is on disk,
# by reading the `playout.first_emit` event it logs at the end of each channel's
# startup catch-up (#38). Without this wait, etv-next's loader spams "unable to
# find playout JSON file for time …" until the station catches up on cold builds.
#
# This reads the station's own log rather than polling the folders, so the three
# things a filesystem glob could not tell apart are now distinguishable: ready
# (the event arrived), still working (no event yet, station alive), and dead (the
# exit sentinel arrived) — the last of which fails immediately instead of sitting
# out the deadline.
#
# Matching is on the `folder` field, which the daemon logs as the same resolved
# path `--list-folders` printed, so the two cannot drift. The pattern tolerates
# both `--log-format` renderings: `folder=path` in pretty, `"folder":"path"` in
# json. A single foreground loop checks all folders each tick and drops them as
# they land, so the readiness window is max(per-channel), not sum. It
# deliberately avoids backgrounded per-folder jobs: under `set -m` (load-bearing
# for the teardown trap) each finishing job emits a "[n]+ Done" notice that
# clutters the otherwise-clean prefixed output (#89).
#
# 180s: with `catalog_path` set the daemon ingests the whole catalog before any
# channel loop starts, and a Plex library of ~85k entries takes well over a
# minute. A timeout is not fatal to a channel (the folders already exist, so
# ETV-next keeps it in the lineup either way), so it warns rather than aborting —
# unlike a dead station, which has nothing left to wait for.
wait_for_first_emit() {
  local deadline=$((SECONDS + 180))
  local pending=("$@")
  local still folder pattern status esc seen
  # The pretty formatter wraps every field name in SGR colour codes, so the raw
  # line reads `<esc>[3mfolder<esc>[0m<esc>[2m=<esc>[0mexamples/output/test` —
  # `folder=` never appears adjacent. Strip the escapes before matching rather
  # than dropping the field-name anchor, which is what makes the match exact.
  esc=$(printf '\033')
  echo "[dev] waiting for station to report first playout in ${#pending[@]} folder(s)..."
  while [ "${#pending[@]}" -gt 0 ] && [ "$SECONDS" -lt "$deadline" ]; do
    seen=$(grep -F 'playout.first_emit' "$station_log" 2>/dev/null | sed "s,${esc}\[[0-9;]*m,,g")
    still=()
    for folder in "${pending[@]}"; do
      # A trailing boundary is what keeps one folder from matching another it is
      # a prefix of — `examples/output/lotr` must not answer for
      # `examples/output/lotr-theatrical`.
      pattern="folder\"?[=:]\"?$(printf '%s' "$folder" | sed 's,[][\.*^$(){}?+|],\\&,g')(\"|[[:space:]]|\$)"
      printf '%s\n' "$seen" | grep -qE "$pattern" || still+=("$folder")
    done
    if [ "${#still[@]}" -eq 0 ]; then
      return 0
    fi
    if status=$(grep -F "$station_exit_marker" "$station_log" 2>/dev/null); then
      echo "[dev] the station exited (${status##* }) before reporting playout for ${still[*]}" >&2
      echo "[dev] see the [station] output above for why; aborting" >&2
      exit 1
    fi
    pending=("${still[@]}")
    sleep 0.5
  done
  [ "${#pending[@]}" -eq 0 ] && return 0
  echo "[dev] WARNING: timed out waiting for ${pending[*]} — launching etv-next anyway" >&2
}

if [ "${#output_folders[@]}" -gt 0 ]; then
  wait_for_first_emit "${output_folders[@]}"
fi

(
  vendor/etv-next/target/debug/ersatztv examples/etv-next/lineup.json 2>&1 \
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
