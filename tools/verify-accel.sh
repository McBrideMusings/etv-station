#!/usr/bin/env bash
# Prove the hardware encoder is actually encoding — not silently falling back
# to software (#258).
#
# A channel that reverts to software looks identical to a working one in the
# container log: ETV-next's answer to a half-configured VAAPI channel is to use
# x264 and say so only at DEBUG. Codec name doesn't separate them either — both
# produce H.264, so `ffprobe` on a segment says `h264` for a hardware encode and
# a software one alike. The only signal that distinguishes them is the ffmpeg
# command line, which the host probe records verbatim.
#
# So this reads `ffmpeg-argv-ch<N>.log` (written by tools/ffmpeg-probe.sh, one
# argument per line) and asserts, for each channel's most recent transcode:
#
#   1. the expected hardware encoder is present  (e.g. h264_vaapi)
#   2. libx264 is ABSENT                          (the fallback fingerprint)
#   3. for VAAPI, -vaapi_device names the configured render node
#
# and, separately, that each rendered channel config carries a non-empty
# `normalization.video.accel`. Both layers matter: the config check catches a
# station that was never asked to use the GPU, the argv check catches a station
# that asked and didn't get it.
#
# EXPECTED VALUES COME FROM deploy/unraid-template.xml — the same source of
# truth the deploy uses. This script never asks the host what it is configured
# to do; it asserts the host matches what the repo says.
#
# Usage:
#   tools/verify-accel.sh                 # against the deployed host over ssh
#   tools/verify-accel.sh --local DIR     # against a local diag dir (dev-run)
#
# Env (from .env, gitignored — no fallbacks, these name a specific machine):
#   UNRAID_HOST, UNRAID_USER, ETV_STATION_DATA, ETV_STATION_APPDATA
#
# Overrides, for asserting something other than what the template says:
#   EXPECT_ACCEL, EXPECT_VAAPI_DEVICE
#
# Exits 0 when every channel passes, 1 on any failure, 2 on a setup problem.
set -uo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)

# deploy/unraid-template.xml is gitignored (.gitignore:64) — it names host paths,
# so it lives in the main checkout and NOT in any linked worktree. Fall back to
# the main checkout, which `--git-common-dir` points at from anywhere: without
# this, every run from a worktree dies on a missing file that is present on the
# machine.
TEMPLATE="$repo_root/deploy/unraid-template.xml"
if [ ! -f "$TEMPLATE" ]; then
  common=$(git -C "$repo_root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)
  if [ -n "$common" ]; then
    TEMPLATE="$(dirname "$common")/deploy/unraid-template.xml"
  fi
fi

LOCAL_DIAG=""
if [ "${1:-}" = "--local" ]; then
  LOCAL_DIAG="${2:-}"
  [ -n "$LOCAL_DIAG" ] || { echo "fatal: --local needs a directory" >&2; exit 2; }
fi

red()    { printf '\033[31m%s\033[0m' "$1"; }
green()  { printf '\033[32m%s\033[0m' "$1"; }
yellow() { printf '\033[33m%s\033[0m' "$1"; }
dim()    { printf '\033[2m%s\033[0m' "$1"; }

# A channel's argv log is only written when a transcode STARTS, and only a
# channel someone is watching transcodes at all. So a log can be days old and
# still say h264_vaapi — proving the GPU worked then, not now. That is a real
# false-confidence risk for a tool whose whole job is catching a silent
# regression, so an old pass is reported as STALE rather than PASS.
STALE_DAYS="${STALE_DAYS:-3}"

failures=0
checked=0
stale=0
fail() { failures=$((failures + 1)); printf '  %s %s\n' "$(red FAIL)" "$*"; }
pass() { printf '  %s %s\n' "$(green PASS)" "$*"; }
warn() { printf '  %s %s\n' "$(yellow STALE)" "$*"; }

# Read one <Config Name="X" ...>value</Config> out of the Unraid template. The
# element spans several lines, so the name and the value are never on the same
# one: latch on the name, then take the text off the line carrying the closing
# tag. Strip `</Config>` FIRST — otherwise the greedy `.*>` swallows through the
# closing tag and returns an empty string.
template_value() {
  awk -v name="$1" '
    index($0, "<Config Name=\"" name "\"") { inblk = 1 }
    inblk && /<\/Config>/ {
      sub(/<\/Config>.*/, "")
      sub(/.*>/, "")
      print
      exit
    }
  ' "$TEMPLATE"
}

[ -f "$TEMPLATE" ] || { echo "fatal: no $TEMPLATE" >&2; exit 2; }

ACCEL="${EXPECT_ACCEL:-$(template_value ETV_ACCEL)}"
VAAPI_DEVICE="${EXPECT_VAAPI_DEVICE:-$(template_value ETV_VAAPI_DEVICE)}"

if [ -z "$ACCEL" ]; then
  echo "fatal: ETV_ACCEL is empty in $TEMPLATE — that IS software x264." >&2
  echo "       Set it there and redeploy; this script asserts, it does not configure." >&2
  exit 2
fi

# What the encoder is called on the ffmpeg command line for each backend.
case "$ACCEL" in
  vaapi) ENCODER="h264_vaapi" ;;
  cuda)  ENCODER="h264_nvenc" ;;
  qsv)   ENCODER="h264_qsv" ;;
  amf)   ENCODER="h264_amf" ;;
  *) echo "fatal: don't know the encoder name for ETV_ACCEL=$ACCEL" >&2; exit 2 ;;
esac

printf '%s\n' "$(dim "expecting ETV_ACCEL=$ACCEL -> $ENCODER (from deploy/unraid-template.xml)")"

# Emit every channel's most recent argv block, prefixed by a file marker. One
# ssh round trip rather than 62: argv logs are append-only and never rotated, so
# the last `=== ` header to EOF is the newest invocation.
# Deliberately no awk here: this string is nested inside `sh -c '...'` inside a
# double-quoted ssh argument, and an awk program's quoting does not survive that
# without becoming unreadable. `tail -c` is quote-free and 64K is far more than
# one invocation's argv, so the newest block is always inside the tail. Trimming
# to that block happens locally, in check_block.
#
# shellcheck disable=SC2016  # $DIAG is expanded by the remote/local sh, not here
collect_argv() {
  local script='for f in "$DIAG"/ffmpeg-argv-ch*.log; do [ -e "$f" ] || continue; echo "##FILE $f"; tail -c 65536 "$f"; done'
  if [ -n "$LOCAL_DIAG" ]; then
    DIAG="$LOCAL_DIAG" sh -c "$script"
  else
    : "${UNRAID_HOST:?set UNRAID_HOST in .env}"
    : "${ETV_STATION_DATA:?set ETV_STATION_DATA in .env}"
    # shellcheck disable=SC2029  # ETV_STATION_DATA must expand locally
    ssh "${UNRAID_USER:-root}@${UNRAID_HOST}" "DIAG='${ETV_STATION_DATA}/diag' sh -c '$script'"
  fi
}

echo
echo "ffmpeg argv — what the encoder actually was"

argv_dump=$(collect_argv) || { echo "fatal: could not read argv logs" >&2; exit 2; }

if [ -z "$argv_dump" ]; then
  echo "  $(red 'FAIL') no ffmpeg-argv-ch*.log found." >&2
  echo "        The probe is host-only state and dies on every \`admin deploy files\`." >&2
  echo "        Re-apply it (see CLAUDE.local.md) and let a channel play, then re-run." >&2
  exit 1
fi

# Assert one channel's newest invocation. Defined before the loop that calls it:
# a bash function is only callable after its definition has been executed.
check_block() {
  local ch="$1" body="$2" when
  [ -n "$ch" ] || return 0
  checked=$((checked + 1))

  # Reduce the tail to the NEWEST invocation only: everything from the last
  # `=== ` header to the end. An older block in the same tail could still carry
  # the hardware encoder from before a regression and mask a current failure.
  body=$(printf '%s\n' "$body" |
    awk '/^=== /{buf = ""} {buf = buf $0 "\n"} END{printf "%s", buf}')

  if ! printf '%s\n' "$body" | grep -q '^argv_end$'; then
    fail "ch $ch: newest argv block is incomplete — channel may still be starting"
    return
  fi

  when=$(printf '%s\n' "$body" | sed -n 's/^=== \([^ ]*\) .*/\1/p' | head -1)

  # libx264 first: "it fell back to software" is a sharper diagnosis than "the
  # encoder I wanted is missing", and both are true of a fallback.
  if printf '%s\n' "$body" | grep -qx -- "libx264"; then
    fail "ch $ch: libx264 in the argv — this channel encoded in SOFTWARE (${when:-unknown time})"
    return
  fi
  if ! printf '%s\n' "$body" | grep -qx -- "$ENCODER"; then
    fail "ch $ch: no $ENCODER in the last transcode's argv (${when:-unknown time})"
    return
  fi
  if [ "$ACCEL" = "vaapi" ] && [ -n "$VAAPI_DEVICE" ]; then
    if ! printf '%s\n' "$body" | grep -qx -- "$VAAPI_DEVICE"; then
      fail "ch $ch: $ENCODER present but render node $VAAPI_DEVICE is not in the argv"
      return
    fi
  fi
  local age
  age=$(age_days "$when")
  if [ -n "$age" ] && [ "$age" -gt "$STALE_DAYS" ]; then
    stale=$((stale + 1))
    warn "ch $ch: $ENCODER, no libx264 — but newest transcode is ${age}d old ($when)"
    return
  fi
  pass "ch $ch: $ENCODER, no libx264 (${when:-unknown time})"
}

# Days between an ISO-8601 Z timestamp and now. Prints nothing if neither date
# dialect parses it — an unparseable stamp must not fail a channel that is
# otherwise fine. GNU date first, then BSD/macOS.
age_days() {
  local when="$1" then_s now_s
  [ -n "$when" ] || return 0
  then_s=$(date -u -d "$when" +%s 2>/dev/null ||
    date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$when" +%s 2>/dev/null) || return 0
  [ -n "$then_s" ] || return 0
  now_s=$(date -u +%s)
  echo $(((now_s - then_s) / 86400))
}

# Split the dump per file. The loop must run in THIS shell (a pipe would fork a
# subshell and lose every increment to `failures`), hence the here-string.
cur_ch=""
cur_body=""
while IFS= read -r line; do
  case "$line" in
    "##FILE "*)
      check_block "$cur_ch" "$cur_body"
      cur_ch=$(printf '%s\n' "${line#\#\#FILE }" |
        sed -n 's/.*ffmpeg-argv-ch\([0-9][0-9]*\)\.log/\1/p')
      cur_body=""
      ;;
    *) cur_body="${cur_body}${line}"$'\n' ;;
  esac
done <<< "$argv_dump"
# The last file's block is still pending when the input ends.
check_block "$cur_ch" "$cur_body"

echo
echo "rendered channel config — what the station ASKED for"

# Layer two, and a different failure: argv proves what ffmpeg did, this proves
# what the station requested. An empty `accel` here means no channel was ever
# asked to use the GPU, which is the #258 starting state.
config_accels() {
  if [ -n "$LOCAL_DIAG" ]; then
    grep -h '"accel"' "$repo_root"/examples/output/test/channel*.json 2>/dev/null
  else
    : "${ETV_STATION_APPDATA:?set ETV_STATION_APPDATA in .env}"
    # shellcheck disable=SC2029  # ETV_STATION_APPDATA must expand locally
    ssh "${UNRAID_USER:-root}@${UNRAID_HOST}" \
      "grep -h '\"accel\"' ${ETV_STATION_APPDATA}/etv-next/channel*.json 2>/dev/null"
  fi
}

empty_accel=$(config_accels | grep -c '"accel": *""' || true)
set_accel=$(config_accels | grep -c "\"accel\": *\"$ACCEL\"" || true)

if [ "$empty_accel" -gt 0 ]; then
  fail "$empty_accel channel config(s) carry \"accel\": \"\" — software by request"
elif [ "$set_accel" -eq 0 ]; then
  fail "no channel config carries \"accel\": \"$ACCEL\""
else
  pass "$set_accel channel config(s) carry \"accel\": \"$ACCEL\""
fi

echo
if [ "$failures" -eq 0 ]; then
  fresh=$((checked - stale))
  printf '%s %d channel(s) on %s (%d verified within %sd, %d stale)\n' \
    "$(green 'OK')" "$checked" "$ENCODER" "$fresh" "$STALE_DAYS" "$stale"
  if [ "$fresh" -eq 0 ]; then
    printf '%s every passing channel is stale — this proves the GPU worked THEN, not now.\n' \
      "$(yellow 'note:')"
    printf '      Play a channel and re-run to get a current answer.\n'
  fi
  exit 0
fi
printf '%s %d of %d channel(s) were not using %s\n' "$(red 'FAILED')" "$failures" "$checked" "$ENCODER"
exit 1
