#!/usr/bin/env bash
# Validate every channel served by a running etv-station + etv-next pair.
#
# For each channel in /channels.m3u this:
#   1. Triggers the channel session and waits for live.m3u8.
#   2. Fetches a fresh segment, probes its codec, and runs ffmpeg blackdetect
#      to catch the "black/silence fallback" mode etv-next enters when its
#      transcode pipeline fails.
#   3. Runs a short ffmpeg ingest end-to-end against the master playlist.
#   4. Scans the integration log (default tmp/devrun.log) for ffmpeg
#      pipeline failures, attributing each to a channel via the
#      `tmp/hls/<N>/` path that appears in the DEBUG pipeline line just
#      before the ERROR.
#
# Exits non-zero if any channel fails. Run while `./tools/dev-run.sh` is active.
#
# Usage:
#   tools/validate-streams.sh
#   BASE_URL=http://localhost:8409 LOG_FILE=tmp/devrun.log tools/validate-streams.sh
set -u

BASE_URL="${BASE_URL:-http://localhost:8409}"
LOG_FILE="${LOG_FILE:-tmp/devrun.log}"
INGEST_SECS="${INGEST_SECS:-10}"
SESSION_WAIT_SECS="${SESSION_WAIT_SECS:-30}"
LOG_RECENT_SECS="${LOG_RECENT_SECS:-180}"

red()    { printf '\033[31m%s\033[0m' "$1"; }
green()  { printf '\033[32m%s\033[0m' "$1"; }
yellow() { printf '\033[33m%s\033[0m' "$1"; }
bold()   { printf '\033[1m%s\033[0m' "$1"; }

failures=0
declare -a failure_msgs=()

fail() {
  local channel="$1"; shift
  local msg="$*"
  failures=$((failures + 1))
  failure_msgs+=("[ch $channel] $msg")
  printf '  %s %s\n' "$(red 'FAIL')" "$msg"
}

pass() { printf '  %s %s\n' "$(green 'PASS')" "$*"; }
warn() { printf '  %s %s\n' "$(yellow 'WARN')" "$*"; }

require() {
  command -v "$1" >/dev/null 2>&1 || {
    printf '%s required tool not found: %s\n' "$(red 'fatal:')" "$1" >&2
    exit 2
  }
}

require curl
require ffmpeg
require ffprobe
require awk

# Filter the log file down to the recent window and the failure entries that
# reference a specific channel via `tmp/hls/<N>/`. Returns lines like:
#   <channel> <iso-timestamp> <message>
# A failure entry is one of:
#   - DEBUG line with `tmp/hls/N/` followed by an ERROR within ~5 lines
#   - ERROR line that itself contains `tmp/hls/N/`
recent_failures_for_channel() {
  local target_channel="$1"
  [ -f "$LOG_FILE" ] || return 0
  local cutoff
  cutoff="$(date -u -v "-${LOG_RECENT_SECS}S" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
    || date -u -d "${LOG_RECENT_SECS} seconds ago" +%Y-%m-%dT%H:%M:%SZ)"
  awk -v cutoff="$cutoff" -v target="$target_channel" '
    function ts_of(line,    m) {
      if (match(line, /[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z/)) {
        return substr(line, RSTART, RLENGTH)
      }
      return ""
    }
    function ch_of(line,    m, rest, slash) {
      if (match(line, /tmp\/hls\/[0-9]+\//)) {
        m = substr(line, RSTART, RLENGTH)
        sub(/tmp\/hls\//, "", m)
        sub(/\//, "", m)
        return m
      }
      return ""
    }
    {
      ts = ts_of($0)
      if (ts == "" || ts < cutoff) next
      ch = ch_of($0)
      if (ch != "") last_channel_for_pid = ch
      if ($0 ~ /ERROR.*ersatztv_channel/) {
        ec = (ch != "") ? ch : last_channel_for_pid
        if (ec == target) {
          msg = $0
          if (length(msg) > 200) msg = substr(msg, 1, 200) "..."
          print ts " " msg
        }
      }
    }
  ' "$LOG_FILE"
}

# Detect whether a downloaded TS segment is the "black/silence fallback" by
# running blackdetect for the segment's full duration. If essentially the
# whole segment is black, the channel is in fallback mode.
segment_is_black() {
  local seg_path="$1"
  local out
  out="$(ffmpeg -hide_banner -nostats -i "$seg_path" -vf blackdetect=d=0.5:pic_th=0.98 -an -f null - 2>&1 || true)"
  printf '%s\n' "$out" | grep -q "blackdetect.*black_start"
}

printf '%s validating %s\n' "$(bold '==>')" "$BASE_URL"

lineup="$(curl -fsS "$BASE_URL/channels.m3u" || true)"
if [ -z "$lineup" ]; then
  printf '%s could not fetch %s/channels.m3u\n' "$(red 'fatal:')" "$BASE_URL" >&2
  exit 2
fi

mapfile -t channel_lines < <(printf '%s\n' "$lineup" | awk -F 'tvg-id="' '
  /tvg-id=/ { split($2, a, "\""); n=a[1]; getline url; print n "\t" url }
')

if [ "${#channel_lines[@]}" -eq 0 ]; then
  printf '%s no channels found in lineup\n' "$(red 'fatal:')" >&2
  exit 2
fi

printf 'discovered %d channel(s)\n\n' "${#channel_lines[@]}"

for entry in "${channel_lines[@]}"; do
  channel="${entry%%$'\t'*}"
  master_url="${entry#*$'\t'}"
  printf '%s channel %s — %s\n' "$(bold '==>')" "$channel" "$master_url"

  # Master playlist
  master_body="$(curl -fsS "$master_url" || true)"
  if [ -z "$master_body" ]; then
    fail "$channel" "master playlist did not return a body"; echo; continue
  fi
  pass "master playlist served"

  # Wait for live session
  live_url="$BASE_URL/session/$channel/live.m3u8"
  waited=0
  while ! curl -fsS -o /dev/null "$live_url"; do
    sleep 2
    waited=$((waited + 2))
    if [ "$waited" -ge "$SESSION_WAIT_SECS" ]; then
      fail "$channel" "live.m3u8 did not appear within ${SESSION_WAIT_SECS}s"
      break
    fi
  done
  if [ "$waited" -ge "$SESSION_WAIT_SECS" ]; then echo; continue; fi
  pass "live session ready (${waited}s)"

  # Fetch latest segment
  live_body="$(curl -fsS "$live_url" || true)"
  latest_seg="$(printf '%s\n' "$live_body" | awk '/^[^#].+\.ts$/ { seg=$0 } END { print seg }')"
  if [ -z "$latest_seg" ]; then
    fail "$channel" "no .ts segment found in live playlist"; echo; continue
  fi
  seg_url="$BASE_URL/session/$channel/$latest_seg"
  tmpseg="$(mktemp -t etv-seg.XXXXXX.ts)"
  if ! curl -fsS "$seg_url" -o "$tmpseg"; then
    fail "$channel" "could not fetch segment $latest_seg"
    rm -f "$tmpseg"; echo; continue
  fi
  seg_bytes="$(wc -c < "$tmpseg" | tr -d ' ')"

  probe="$(ffprobe -v error -show_streams -of compact "$tmpseg" 2>&1 || true)"
  vcodec="$(printf '%s\n' "$probe" | awk -F'codec_name=' '/codec_type=video/ { split($2, a, "|"); print a[1]; exit }')"
  acodec="$(printf '%s\n' "$probe" | awk -F'codec_name=' '/codec_type=audio/ { split($2, a, "|"); print a[1]; exit }')"

  if [ -z "$vcodec" ] || [ -z "$acodec" ]; then
    fail "$channel" "ffprobe could not identify v/a streams"
  else
    pass "segment ${latest_seg} ${seg_bytes}B v=${vcodec} a=${acodec}"
  fi

  # Black/silence fallback detection
  if segment_is_black "$tmpseg"; then
    fail "$channel" "segment is solid black — channel is in black/silence fallback mode"
  else
    pass "segment has real video content (not the black fallback)"
  fi
  rm -f "$tmpseg"

  # Full-stream ingest
  ingest_log="$(mktemp -t etv-ingest.XXXXXX.log)"
  if ffmpeg -hide_banner -loglevel error -i "$master_url" -t "$INGEST_SECS" -f null - >"$ingest_log" 2>&1; then
    benign_re='non monotonically increasing dts|Application provided invalid'
    serious="$(grep -vE "$benign_re" "$ingest_log" || true)"
    if [ -n "$serious" ]; then
      warn "ffmpeg ingest succeeded with non-benign messages:"
      printf '%s\n' "$serious" | head -3 | sed 's/^/        /'
    else
      pass "ffmpeg ingested ${INGEST_SECS}s clean"
    fi
  else
    fail "$channel" "ffmpeg ingest failed"
    head -5 "$ingest_log" | sed 's/^/        /'
  fi
  rm -f "$ingest_log"

  # Channel-scoped server-log scan
  if [ -f "$LOG_FILE" ]; then
    failures_for_ch="$(recent_failures_for_channel "$channel")"
    if [ -n "$failures_for_ch" ]; then
      count="$(printf '%s\n' "$failures_for_ch" | wc -l | tr -d ' ')"
      latest="$(printf '%s\n' "$failures_for_ch" | tail -1)"
      fail "$channel" "$count pipeline error(s) in last ${LOG_RECENT_SECS}s; latest: ${latest:0:160}"
    else
      pass "no recent pipeline errors in $LOG_FILE"
    fi
  else
    warn "log file $LOG_FILE not found; skipping log scan"
  fi

  echo
done

if [ "$failures" -eq 0 ]; then
  printf '%s all channels healthy\n' "$(green 'OK')"
  exit 0
fi

printf '%s %d failure(s):\n' "$(red 'FAIL')" "$failures"
for m in "${failure_msgs[@]}"; do
  printf '  - %s\n' "$m"
done
exit 1
