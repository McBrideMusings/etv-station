#!/usr/bin/env bash
# Attribute a channel stall (#327) to the side that actually stopped first.
#
# THE PROBLEM THIS SOLVES
#
# A stall is symmetric from the outside. Segments stop landing, ffmpeg's stderr
# stays empty, and ~60s later ETV-next kills the session with exit 75. That
# looks identical whether ffmpeg stopped producing frames or whether ffmpeg kept
# producing and its output stopped being counted. The container log cannot tell
# the two apart, and guessing between them has cost this repo real time.
#
# THE DISCRIMINATOR
#
# ETV-next pumps ffmpeg's `-progress pipe:1` into the `ffmpeg_progress` log
# target (channel_session.rs:1560), which docker/entrypoint.sh splits into the
# rotated /data/diag/ffmpeg-progress.log. One timestamped line per channel per
# report. Line up the moment `frame=` last CHANGED against the moment ETV-next
# logged `exit status: 75`:
#
#   frame= still advancing at the kill
#       -> ffmpeg was healthy. Whatever stopped is downstream — ETV-next's
#          segment accounting in playlist_manager::update(), which is what feeds
#          `last_progress` and therefore the stall detector
#          (channel_session.rs:975).
#
#   frame= flatlined ~60s before the kill
#       -> ffmpeg stopped producing frames while staying alive enough to keep
#          reporting. The encoder stopped first, and the kill is the detector
#          working correctly rather than firing early.
#
# Measured in production on 2026-08-22: every stall examined on channels 4 and 5
# was the second case — 58-63s of zero frames against a STALL_THRESHOLD of 60s
# (channel_session.rs:51).
#
# COMPLETED-BUT-WOULD-NOT-EXIT
#
# One further split matters. If the frozen `out_time` had already reached the
# `-t` the item was assigned, ffmpeg finished all its work and then failed to
# terminate rather than wedging partway through — a different bug wearing the
# same signature. The assignment is read from `ffmpeg-argv-ch<N>.log`, whose
# `=== <ts> pid=<pid>` headers let the right invocation be picked by time.
#
# TWO SOURCES, AND WHY
#
# Until b6f5ac8 the probe appended its own `-progress` to a per-channel file,
# which REPLACED ETV-next's `-progress pipe:1` and left the rotated log empty.
# That commit removed it. Deployed images older than b6f5ac8 therefore have
# per-session `ffmpeg-progress-ch<N>-<stamp>-<pid>.log` files and a stale
# rotated log; newer ones have the reverse. This reads the rotated log first and
# falls back to the per-session files, so it works either side of that deploy.
# The per-session path infers timing from file mtime and is the weaker of the
# two — it can only see the LAST stall of a session, since a session that
# survives a stall leaves no mtime marking it.
#
# Usage:
#   tools/attribute-stalls.sh                    # every stall in the container log
#   tools/attribute-stalls.sh --channel 4        # one channel only
#   tools/attribute-stalls.sh --at <ISO8601Z> --channel 4
#                                                # attribute one named moment
#   tools/attribute-stalls.sh --local DIR        # a local diag dir (dev-run)
#
# Env (from .env, gitignored — no fallbacks, these name a specific machine):
#   UNRAID_HOST, UNRAID_USER
#
# Exits 0 when every stall was attributed, 1 when any was not, 2 on a setup
# problem. It never exits non-zero merely because stalls exist — this reports on
# them, it does not assert their absence.
set -uo pipefail

FLATLINE_MIN_S=45   # at/above this, the encoder had clearly stopped
HEALTHY_MAX_S=5     # at/below this, ffmpeg was still producing at the kill
WINDOW_S=150        # how far back of a kill to read progress

usage() { sed -n '2,70p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

MODE=remote; LOCAL_DIR=; ONLY_CHANNEL=; AT=

while [ $# -gt 0 ]; do
  case "$1" in
    --local)   MODE=local; LOCAL_DIR="${2:-}"; shift 2 || usage 2 ;;
    --channel) ONLY_CHANNEL="${2:-}"; shift 2 || usage 2 ;;
    --at)      AT="${2:-}"; shift 2 || usage 2 ;;
    -h|--help) usage 0 ;;
    *) echo "unknown argument: $1" >&2; usage 2 ;;
  esac
done

# Read live state through `docker exec`, never a host-side path: /mnt/user is
# shfs and its attribute cache serves mtimes hours stale, which would corrupt
# every duration computed here.
if [ "$MODE" = local ]; then
  [ -n "$LOCAL_DIR" ] || { echo "--local needs a directory" >&2; exit 2; }
  [ -d "$LOCAL_DIR" ] || { echo "no such diag dir: $LOCAL_DIR" >&2; exit 2; }
  run() { DIAG="$LOCAL_DIR" sh -c "$1"; }
  kills() { echo ""; }   # no container log locally; use --at
else
  ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || { echo "not in a git repo" >&2; exit 2; }
  if [ -f "$ROOT/.env" ]; then set -a; . "$ROOT/.env"; set +a; fi
  : "${UNRAID_HOST:?set UNRAID_HOST in .env}"
  : "${UNRAID_USER:?set UNRAID_USER in .env}"
  esc() { printf '%s' "$1" | sed "s/'/'\\\\''/g"; }
  run()   { ssh -n "$UNRAID_USER@$UNRAID_HOST" "docker exec etv-station sh -c 'DIAG=/data/diag; $(esc "$1")'"; }
  # `<ts> <channel>` per stall. Rebursts (#339) carry no exit 75 and so never
  # appear here — the two populations must not be mixed.
  kills() {
    ssh -n "$UNRAID_USER@$UNRAID_HOST" \
      "docker logs etv-station 2>&1 | grep 'exited with status exit status: 75'" \
      | sed -E 's/^.?\[?([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z).*channel ([0-9]+).*/\1 \2/' \
      | grep -E '^[0-9]{4}-'
  }
fi

# Portable ISO8601-UTC arithmetic. GNU `date -d` is absent on macOS, where this
# is usually run, so python3 does the math for both platforms.
shift_ts() { python3 -c 'import sys,datetime as d;t=d.datetime.strptime(sys.argv[1],"%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=d.timezone.utc)+d.timedelta(seconds=int(sys.argv[2]));print(t.strftime("%Y-%m-%dT%H:%M:%SZ"))' "$1" "$2"; }
delta_s()  { python3 -c 'import sys,datetime as d;f="%Y-%m-%dT%H:%M:%SZ";a=d.datetime.strptime(sys.argv[1],f);b=d.datetime.strptime(sys.argv[2],f);print(int((a-b).total_seconds()))' "$1" "$2"; }

# ---- source selection -------------------------------------------------------
# ISO8601-UTC sorts lexicographically, so a string range is a time range and the
# whole scan can be one awk pass on the host.
rotated_newest="$(run 'cat "$DIAG"/ffmpeg-progress.log 2>/dev/null | tail -1 | sed -E "s/^\[([^ ]+).*/\1/"')"
[ -n "$rotated_newest" ] || rotated_newest="(empty)"

# Last frame CHANGE and the final counters for <channel> in the window ending at
# <kill>. Prints: <last_change_ts> <final_frame> <final_out_time_us>
progress_window() {
  ch="$1"; from="$2"; to="$3"
  run '
    for f in "$DIAG"/ffmpeg-progress.log.2 "$DIAG"/ffmpeg-progress.log.1 "$DIAG"/ffmpeg-progress.log; do
      [ -f "$f" ] && cat "$f"
    done 2>/dev/null | awk -v ch="channel '"$ch"':" -v from="'"$from"'" -v to="'"$to"'" "
      index(\$0, ch) == 0 { next }
      { ts = substr(\$1, 2) }
      ts < from || ts > to { next }
      { fr = \"\"; ot = \"\"
        for (i = 1; i <= NF; i++) {
          if (\$i ~ /^frame=/)       fr = substr(\$i, 7)
          if (\$i ~ /^out_time_us=/) ot = substr(\$i, 13)
        }
        if (fr == \"\") next
        seen = 1
        if (fr != prev) { change = ts; prev = fr }
        lastfr = fr; lastot = ot
      }
      END { if (seen) print change, lastfr, lastot }
    "'
}

# Fallback for images older than b6f5ac8, whose probe wrote per-session files
# instead of letting the rotated log fill. Those files carry no timestamps, so
# timing is inferred: the session began at the stamp in its name and its last
# write is its mtime, which gives a measured blocks-per-second rather than an
# assumed one. A file is matched to a kill by mtime, since a killed session's
# final write lands in the same second as the kill.
#
# Weaker than the rotated log in one specific way: only the LAST stall of a
# session leaves an mtime, so a session that stalled, recovered, and stalled
# again is attributable once. Prints: <flatline_s> <final_frame> <final_out_us>
legacy_window() {
  ch="$1"; kill_ts="$2"
  run '
    kill_e=$(date -u -d "'"$kill_ts"'" +%s 2>/dev/null) || exit 0
    for f in "$DIAG"/ffmpeg-progress-ch'"$ch"'-*.log; do
      [ -f "$f" ] || continue
      mt=$(stat -c %Y "$f") || continue
      d=$((mt - kill_e)); [ $d -lt 0 ] && d=$((-d))
      [ $d -le 3 ] || continue
      b=$(basename "$f"); s=${b#ffmpeg-progress-ch'"$ch"'-}; s=${s%%-*}
      st=$(date -u -d "$(echo "$s" | sed -E "s/(....)(..)(..)T(..)(..)(..)/\1-\2-\3 \4:\5:\6/") UTC" +%s 2>/dev/null) || continue
      span=$((mt - st)); [ "$span" -gt 30 ] || continue
      awk -v span="$span" "
        /^frame=/ { n++; if (\$0 == last) flat++; else flat = 1; last = \$0; lf = substr(\$0, 7) }
        /^out_time_us=/ { lo = substr(\$0, 13) }
        END { if (n > 10) printf \"%.0f %s %s\n\", flat / (n / span), lf, (lo == \"\" ? 0 : lo) }
      " "$f"
      break
    done'
}

# The invocation that was running at <kill>: its start time and the `-t` it was
# handed. Prints: <start_ts> <t_ms>. Reads only the argv log, which the probe
# writes on every image, so this survives the b6f5ac8 deploy either way.
invocation_at() {
  ch="$1"; kill_ts="$2"
  run 'awk -v to="'"$kill_ts"'" "
      /^=== / { if (\$2 <= to) { ts = \$2; keep = 1; t = \"\" } else keep = 0; next }
      keep && prev == \"-t\" && \$0 ~ /^[0-9]+ms\$/ { t = \$0; sts = ts }
      keep { prev = \$0 }
      END { if (t != \"\") print sts, t }
    " "$DIAG/ffmpeg-argv-ch'"$ch"'.log" 2>/dev/null'
}

# ---- the stalls to attribute ------------------------------------------------
if [ -n "$AT" ]; then
  [ -n "$ONLY_CHANNEL" ] || { echo "--at needs --channel" >&2; exit 2; }
  events="$AT $ONLY_CHANNEL"
else
  events="$(kills)"
fi

if [ -z "$events" ]; then
  echo "no exit-75 stalls in the container log (rebursts are a different population and are excluded)"
  exit 0
fi

echo "rotated progress log newest line: $rotated_newest"
printf '%-22s %-4s %10s  %s\n' KILLED-AT CH FLATLINE VERDICT
printf -- '-------------------------------------------------------------------------------------------\n'

n_enc=0; n_con=0; n_done=0; n_none=0

while read -r ts ch; do
  [ -n "$ts" ] || continue
  [ -z "$ONLY_CHANNEL" ] || [ "$ch" = "$ONLY_CHANNEL" ] || continue

  from="$(shift_ts "$ts" "-$WINDOW_S")"
  to="$(shift_ts "$ts" 5)"
  read -r change final_frame final_ot <<EOF
$(progress_window "$ch" "$from" "$to")
EOF

  src=rotated
  if [ -n "${change:-}" ]; then
    flat_s="$(delta_s "$ts" "$change")"
  else
    src=per-session
    read -r flat_s final_frame final_ot <<EOF
$(legacy_window "$ch" "$ts")
EOF
  fi

  # The overrun check below reads only the argv log, so a kill with no progress
  # data at all is still attributable — just without the corroborating flatline.
  [ -n "${flat_s:-}" ] || { flat_s="?"; src=argv-only; }

  # How long past its own assigned end did ffmpeg survive? An invocation given
  # `-t X` should exit at start+X. When the kill lands ~STALL_THRESHOLD after
  # that, ffmpeg finished every frame it was asked for and then refused to
  # terminate — the detector is reporting a hang, not a mid-item wedge.
  overrun=""
  read -r inv_start inv_t <<EOF
$(invocation_at "$ch" "$ts")
EOF
  if [ -n "${inv_start:-}" ] && [ -n "${inv_t:-}" ]; then
    pred="$(shift_ts "$inv_start" "$(( ${inv_t%ms} / 1000 ))")"
    overrun="$(delta_s "$ts" "$pred")"
  fi

  if [ -n "$overrun" ] && [ "$overrun" -ge 30 ] 2>/dev/null && [ "$overrun" -le 100 ] 2>/dev/null; then
    printf '%-22s %-4s %9ss  %s\n' "$ts" "$ch" "$flat_s" \
      "COMPLETED-BUT-WOULD-NOT-EXIT (hit its -t of $inv_t at $pred, killed ${overrun}s later) [$src]"
    n_done=$((n_done+1))
  elif [ "$flat_s" = "?" ]; then
    printf '%-22s %-4s %10s  %s\n' "$ts" "$ch" "-" \
      "NO-PROGRESS-DATA (and ${overrun:-no}s overrun does not match a completed item)"
    n_none=$((n_none+1))
  elif [ "$flat_s" -ge "$FLATLINE_MIN_S" ] 2>/dev/null; then
    printf '%-22s %-4s %9ss  %s\n' "$ts" "$ch" "$flat_s" \
      "ENCODER-STOPPED-FIRST (wedged mid-item, ${overrun:-?}s vs its -t, last frame=$final_frame) [$src]"
    n_enc=$((n_enc+1))
  elif [ "$flat_s" -le "$HEALTHY_MAX_S" ] 2>/dev/null; then
    printf '%-22s %-4s %9ss  %s\n' "$ts" "$ch" "$flat_s" \
      "CONSUMER-STOPPED-FIRST (ffmpeg still producing at the kill, frame=$final_frame) [$src]"
    n_con=$((n_con+1))
  else
    printf '%-22s %-4s %9ss  %s\n' "$ts" "$ch" "$flat_s" \
      "AMBIGUOUS (neither side is clearly first) [$src]"
    n_none=$((n_none+1))
  fi
done <<EOF
$events
EOF

printf -- '-------------------------------------------------------------------------------------------\n'
printf 'encoder-stopped-first=%s consumer-stopped-first=%s completed-but-hung=%s no-data-or-ambiguous=%s\n' \
  "$n_enc" "$n_con" "$n_done" "$n_none"
echo "A NO-PROGRESS-DATA row is a gap in instrumentation, not an unattributable stall."
