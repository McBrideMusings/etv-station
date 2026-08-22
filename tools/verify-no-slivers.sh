#!/usr/bin/env bash
# Prove no channel is spawning a whole ffmpeg transcode session to produce less
# than one HLS segment (#339).
#
# A "sliver" is an invocation whose `-t` is shorter than its own `-hls_time`.
# Measured in production on 2026-08-22 (11:20pm ET 2026-08-21): channel 2 was
# handed `-t 153ms` against `-hls_time 4` — 3.8% of one segment — channel 1
# `-t 1336ms`, channel 9 `-t 3619ms`. Each one is a full process spawn, a deep
# seek, a hwaccel decode init and an overlay composite to emit a fraction of a
# segment nobody can play on its own.
#
# It is not a scheduling defect: every playout item the station emits is a whole
# catalog item, and an item straddling a chunk boundary is re-emitted whole in
# both chunks. The sliver is made by `reburst_decision` in
# vendor/etv-next/crates/ersatztv-channel/src/channel_session.rs, which restarts
# a still-playing item when its buffer lead drains — and, before #339, did so
# without checking whether any of the item was left to transcode. The container
# log names it:
#
#   lead down to 18s while still playing; restarting the item to rebuild the buffer
#   resuming the same item from 2026-08-21 20:31:18.460403686 +00:00:00
#
# #339 gated that on `remaining > REBURST_AT_LEAD`. This script is the read-back:
# a sliver reappearing means the gate regressed.
#
# THE SEGMENT LENGTH COMES OUT OF THE ARGV LOG, not from a config file. It is a
# compile-time constant (`SEGMENT_SECONDS` in ffpipeline/src/pipeline.rs), so the
# only honest source for what a given invocation was held to is the `-hls_time`
# that invocation was actually given.
#
# Unlike tools/verify-accel.sh this reads EVERY invocation in each log, not just
# the newest: a sliver is a rare event, and checking only the last spawn would
# miss all three of the ones above.
#
# Usage:
#   tools/verify-no-slivers.sh                 # against the deployed host over ssh
#   tools/verify-no-slivers.sh --local DIR     # against a local diag dir (dev-run)
#
# Env (from .env, gitignored — no fallbacks, these name a specific machine):
#   UNRAID_HOST, UNRAID_USER, ETV_STATION_DATA
#
# Exits 0 when no channel has a sliver, 1 when any does, 2 on a setup problem.
set -uo pipefail

LOCAL_DIAG=""
if [ "${1:-}" = "--local" ]; then
  LOCAL_DIAG="${2:-}"
  [ -n "$LOCAL_DIAG" ] || { echo "fatal: --local needs a directory" >&2; exit 2; }
fi

red()    { printf '\033[31m%s\033[0m' "$1"; }
green()  { printf '\033[32m%s\033[0m' "$1"; }
dim()    { printf '\033[2m%s\033[0m' "$1"; }

# Whole logs, not a tail: tools/ffmpeg-probe.sh caps each at ~2000 lines, so the
# file is already bounded and a sliver can sit anywhere in it. One ssh round trip
# for every channel, as in verify-accel.sh, and for the same reason.
#
# shellcheck disable=SC2016  # $DIAG is expanded by the remote/local sh, not here
collect_argv() {
  local script='for f in "$DIAG"/ffmpeg-argv-ch*.log; do [ -e "$f" ] || continue; echo "##FILE $f"; cat "$f"; done'
  if [ -n "$LOCAL_DIAG" ]; then
    DIAG="$LOCAL_DIAG" sh -c "$script"
  else
    : "${UNRAID_HOST:?set UNRAID_HOST in .env}"
    : "${ETV_STATION_DATA:?set ETV_STATION_DATA in .env}"
    # shellcheck disable=SC2029  # ETV_STATION_DATA must expand locally
    ssh "${UNRAID_USER:-root}@${UNRAID_HOST}" "DIAG='${ETV_STATION_DATA}/diag' sh -c '$script'"
  fi
}

echo "ffmpeg argv — is any transcode shorter than one segment?"

argv_dump=$(collect_argv) || { echo "fatal: could not read argv logs" >&2; exit 2; }

if [ -z "$argv_dump" ]; then
  echo "  $(red 'FAIL') no ffmpeg-argv-ch*.log found." >&2
  echo "        The probe ships in the image and is named by ffmpeg.ffmpeg_path" >&2
  echo "        in deploy/appdata/station.yaml. An empty diag dir means it is" >&2
  echo "        unset there, or no channel has transcoded since the last start." >&2
  exit 2
fi

# One line per invocation: "<channel> <hls_time_ms> <t_ms> <source basename>".
# The probe writes one argument per line, so a flag's value is the line after it.
# `-t` and `-hls_time` are each emitted at most once per invocation; take the
# first of each so a filter graph mentioning a similar token cannot overwrite it.
report=$(printf '%s\n' "$argv_dump" | awk '
  function flush_block() {
    if (ch != "") printf "%s\t%s\t%s\t%s\n", ch, seg, dur, src
  }
  /^##FILE / {
    flush_block(); ch = ""; seg = ""; dur = ""; src = ""
    file = $2
    next
  }
  /^=== / {
    flush_block()
    seg = ""; dur = ""; src = ""
    ch = ""
    for (i = 1; i <= NF; i++) if ($i ~ /^channel=/) { ch = substr($i, 9) }
    next
  }
  ch == "" { next }
  prev == "-t"        && dur == "" && $0 ~ /^[0-9]+ms$/ { dur = substr($0, 1, length($0) - 2) }
  prev == "-hls_time" && seg == "" && $0 ~ /^[0-9.]+$/  { seg = $0 * 1000 }
  # The media input, not the overlay fifo or the filter graph.
  prev == "-i" && src == "" && $0 ~ /\// { n = split($0, p, "/"); src = p[n] }
  { prev = $0 }
  END { flush_block() }
')

failures=0
checked=0
channels=0
last_ch=""

while IFS=$'\t' read -r ch seg dur src; do
  [ -n "$ch" ] || continue
  # An invocation with no -t runs to the end of the item and cannot be a sliver.
  [ -n "$dur" ] || continue
  # No -hls_time means this was a capability probe, not a real transcode.
  [ -n "$seg" ] || continue
  checked=$((checked + 1))
  [ "$ch" = "$last_ch" ] || { channels=$((channels + 1)); last_ch="$ch"; }

  if [ "$dur" -lt "$seg" ]; then
    failures=$((failures + 1))
    pct=$(awk -v d="$dur" -v s="$seg" 'BEGIN { printf "%.1f", d * 100 / s }')
    printf '  %s channel %s spawned a transcode for %sms against -hls_time %sms (%s%% of one segment)\n' \
      "$(red FAIL)" "$ch" "$dur" "$seg" "$pct"
    printf '        %s\n' "$(dim "${src:-<no input>}")"
  fi
done <<EOF
$report
EOF

echo

if [ "$checked" -eq 0 ]; then
  echo "  $(red 'FAIL') found argv logs but no invocation carrying both -t and -hls_time." >&2
  echo "        Either the probe recorded only capability probes, or the argv" >&2
  echo "        format changed and this parser is reading nothing. Check" >&2
  echo "        tools/ffmpeg-probe.sh before trusting a pass here." >&2
  exit 2
fi

if [ "$failures" -eq 0 ]; then
  printf '%s no sliver in %d invocation(s) across %d channel(s)\n' \
    "$(green OK)" "$checked" "$channels"
  exit 0
fi

printf '%s %d of %d invocation(s) produced less than one segment\n' \
  "$(red FAILED)" "$failures" "$checked"
printf '%s\n' "$(dim 'Each one is a reburst that fired with nothing left to rebuild — see #339.')"
exit 1
