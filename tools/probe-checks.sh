#!/usr/bin/env bash
# Shared HTTP-check assertion library for etv-station's served surface.
#
# Extracted from tools/verify-integration.sh (#27) so its master-playlist and
# XMLTV checks have exactly one home, sourced by both the dev integration
# smoke test (tools/verify-integration.sh) and the soak-test probe harness
# (tools/soak-probe.sh, #297) rather than each hand-maintaining its own copy
# of the same assertions. Same "one file, multiple consumers" shape as
# tools/dev-procs.sh.
#
# Every check below takes already-fetched content (a curl body, a log file
# path) rather than fetching it itself, so a caller controls its own HTTP
# timing/retries and can reuse one fetch across multiple assertions. On
# failure a check echoes a one-line human-readable message to stdout and
# returns 1; on success it returns 0 and prints nothing. Callers decide how
# to render that (verify-integration.sh's colored PASS/FAIL lines,
# soak-probe.sh's machine-readable result log) — this file has no opinion on
# presentation.
#
# Functions:
#   probe_check_master_playlist <body> <channel>
#       Fails if <body> is empty, doesn't start with #EXTM3U, or lacks a
#       /session/<channel>/live.m3u8 reference.
#   probe_check_xmltv_wellformed <body>
#       Fails if <body> is empty or is not valid XML (xmllint --noout).
#   probe_check_xmltv_channel_present <body>
#       Fails if <body> lacks a <channel id= element.
#   probe_check_xmltv_coverage_all <body> <min_days>
#       Fails unless every <channel id=…> in <body> has at least one
#       <programme> whose stop time is >= <min_days> days from now.
#   probe_check_log_window <log_file> <start_line> <end_line>
#       Fails if any "unable to find playout JSON file for time …" line
#       appears in <log_file> within [<start_line>, <end_line>]. Zero-tolerance:
#       a playout gap that already happened is never something to average
#       away, so any match at all is a failure. <end_line> is a caller-chosen
#       upper bound, not "end of file" — see soak-probe.sh, which snapshots
#       the line count once and reuses it both to bound this scan and as the
#       next probe's start_line, so a line appended between the scan and the
#       state-file write is neither skipped nor scanned twice.
set -u

probe_check_master_playlist() {
  local body="$1" channel="$2"
  if [ -z "$body" ]; then
    echo "channel $channel master playlist returned no body"
    return 1
  fi
  if ! printf '%s\n' "$body" | head -1 | grep -q '#EXTM3U'; then
    echo "channel $channel master playlist does not start with #EXTM3U"
    return 1
  fi
  if ! printf '%s\n' "$body" | grep -q "session/${channel}/live.m3u8"; then
    echo "channel $channel master playlist does not contain /session/${channel}/live.m3u8"
    return 1
  fi
  return 0
}

probe_check_xmltv_wellformed() {
  local body="$1"
  if [ -z "$body" ]; then
    echo "xmltv.xml returned no body"
    return 1
  fi
  if ! printf '%s\n' "$body" | xmllint --noout - 2>/dev/null; then
    echo "xmltv.xml is not valid XML"
    return 1
  fi
  return 0
}

probe_check_xmltv_channel_present() {
  local body="$1"
  if ! printf '%s\n' "$body" | grep -q '<channel id='; then
    echo "xmltv.xml does not contain a <channel id= element"
    return 1
  fi
  return 0
}

# Portable "now + N days" as a 14-digit UTC YYYYMMDDHHMMSS string. Tries GNU
# date's -d first and falls back to BSD date's -v — same GNU-then-BSD
# fallback shape as entrypoint.sh's require_writable, and for the same
# reason: this library is sourced both inside the Linux container and from a
# Mac dev checkout running tools/verify-integration.sh.
_probe_utc_plus_days() {
  local days="$1"
  date -u -d "+${days} days" '+%Y%m%d%H%M%S' 2>/dev/null \
    || date -u -v"+${days}d" '+%Y%m%d%H%M%S' 2>/dev/null
}

probe_check_xmltv_coverage_all() {
  local body="$1" min_days="$2"
  local threshold
  threshold="$(_probe_utc_plus_days "$min_days")"
  if [ -z "$threshold" ]; then
    echo "could not compute a +${min_days}d coverage threshold (no usable date command)"
    return 1
  fi

  local channel_ids
  channel_ids="$(printf '%s\n' "$body" \
    | xmllint --xpath '//channel/@id' - 2>/dev/null \
    | grep -o 'id="[^"]*"' | sed 's/^id="//; s/"$//')"
  if [ -z "$channel_ids" ]; then
    echo "xmltv.xml has no <channel id= entries to check coverage for"
    return 1
  fi

  local short_ids=()
  local cid stops max_stop max_digits
  while IFS= read -r cid; do
    [ -n "$cid" ] || continue
    stops="$(printf '%s\n' "$body" \
      | xmllint --xpath "//programme[@channel='${cid}']/@stop" - 2>/dev/null \
      | grep -o 'stop="[^"]*"' | sed 's/^stop="//; s/"$//')"
    max_stop="$(printf '%s\n' "$stops" | sort | tail -1)"
    max_digits="$(printf '%s' "$max_stop" | tr -cd '0-9' | cut -c1-14)"
    if [ -z "$max_digits" ] || [[ "$max_digits" < "$threshold" ]]; then
      short_ids+=("$cid")
    fi
  done <<< "$channel_ids"

  if [ "${#short_ids[@]}" -gt 0 ]; then
    echo "xmltv.xml programme coverage is short of +${min_days}d for channel(s): ${short_ids[*]}"
    return 1
  fi
  return 0
}

probe_check_log_window() {
  local log_file="$1" start_line="$2" end_line="$3"
  [ -f "$log_file" ] || return 0
  [ "$end_line" -ge "$start_line" ] || return 0
  local matches
  matches="$(sed -n "${start_line},${end_line}p" "$log_file" 2>/dev/null \
    | grep 'unable to find playout JSON file for time' || true)"
  if [ -n "$matches" ]; then
    printf '%s\n' "$matches"
    return 1
  fi
  return 0
}
