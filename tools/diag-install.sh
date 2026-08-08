#!/usr/bin/env bash
# Install, start, stop, and inspect the two stream diagnostics on the Unraid
# host: the client access log and the stream event watcher.
#
# Both have to run on the host rather than in the container — the access log
# needs the client's real address (the container only ever sees the docker
# bridge), and the watcher needs the playlist files without going through HTTP.
# So they are copied to the appdata folder, which survives both a container
# rebuild and `admin deploy files` (that rsync runs with delete = false).
#
# They do NOT survive an Unraid reboot. Run `tools/diag-install.sh start` again
# after one; `status` tells NOT INSTALLED (never copied to this host, so an
# empty log means nothing) apart from NOT RUNNING (copied, but the process died
# — most likely a reboot, and an empty log for that window is a real gap).
#
# Usage:
#   tools/diag-install.sh start     # copy the scripts up and run both
#   tools/diag-install.sh stop      # stop both, leave the logs in place
#   tools/diag-install.sh status    # running state, log sizes, last rows
#   tools/diag-install.sh logs      # tail both logs
#   tools/diag-install.sh logs access
#   tools/diag-install.sh logs events
set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# `.env` fills in what the environment has not already said, and never overrules
# it. Sourcing it plainly does the opposite: every assignment in the file wins,
# so a caller that exports ETV_STATION_APPDATA is silently ignored and there is
# no way to point this script at anything but the real host. That cost a
# debugging session — tools/test-diag-install.sh passed against a worktree,
# which has no .env because it is gitignored, and failed 9 of 18 in the primary
# checkout, which has one.
DIAG_ENV_VARS=(
  UNRAID_HOST
  UNRAID_USER
  ETV_STATION_APPDATA
  ETV_STATION_DATA
  ETV_PORT_HOST
)

declare -A _diag_preset=()
for _v in "${DIAG_ENV_VARS[@]}"; do
  if [ -n "${!_v:-}" ]; then _diag_preset[$_v]="${!_v}"; fi
done

if [ -f "$REPO_ROOT/.env" ]; then
  set -a
  # shellcheck disable=SC1091
  . "$REPO_ROOT/.env"
  set +a
fi

for _v in "${!_diag_preset[@]}"; do
  printf -v "$_v" '%s' "${_diag_preset[$_v]}"
done
unset _v _diag_preset

UNRAID_HOST="${UNRAID_HOST:-}"
UNRAID_USER="${UNRAID_USER:-root}"
ETV_STATION_APPDATA="${ETV_STATION_APPDATA:-/mnt/user/appdata/etv-station}"
ETV_STATION_DATA="${ETV_STATION_DATA:-$ETV_STATION_APPDATA/data}"
ETV_PORT_HOST="${ETV_PORT_HOST:-8409}"

REMOTE_DIR="$ETV_STATION_APPDATA/diag"
LOG_DIR="$ETV_STATION_DATA/diag"
ACCESS_LOG="$LOG_DIR/access.log"
EVENTS_LOG="$LOG_DIR/stream-events.log"

red()   { printf '\033[31m%s\033[0m' "$1"; }
green() { printf '\033[32m%s\033[0m' "$1"; }
bold()  { printf '\033[1m%s\033[0m' "$1"; }

if [ -z "$UNRAID_HOST" ]; then
  printf '%s UNRAID_HOST is not set (expected in %s/.env)\n' "$(red 'fatal:')" "$REPO_ROOT" >&2
  exit 2
fi

SSH_TARGET="$UNRAID_USER@$UNRAID_HOST"

remote() { ssh -o ConnectTimeout=10 "$SSH_TARGET" "$@"; }

# Sent ahead of every remote body that cares whether a diagnostic is alive:
# is the pid recorded in $1 a live process that is actually the script named
# in $2?
#
# Asking the kernel whether the number exists is not enough. Linux hands out
# process ids in a loop, so once a diagnostic dies the number left behind in
# its pid file is eventually handed to something else — an smbd worker, a
# docker helper — and a bare existence check then swears the diagnostic is
# still recording. Match the script's own name against /proc/<pid>/cmdline,
# which the kernel already maintains, and treat a pid that belongs to a
# stranger exactly like no pid at all: report it dead and delete the stale
# file, so `start` stops believing there is nothing to do.
#
# POSIX sh — this runs under /bin/sh on the Unraid host, not bash. On success
# it sets ALIVE_PID to the confirmed pid.
# shellcheck disable=SC2016  # the $1/$2 in here are the remote shell's, not ours
REMOTE_ALIVE_FN='
alive_pid() {
  ALIVE_PID=
  [ -f "$1" ] || return 1
  _pid=$(cat "$1" 2>/dev/null)
  case "$_pid" in
    "" | *[!0-9]*) rm -f "$1"; return 1 ;;
  esac
  if [ -r "/proc/$_pid/cmdline" ] && tr "\000" " " < "/proc/$_pid/cmdline" | grep -qF "$2"; then
    ALIVE_PID="$_pid"
    return 0
  fi
  rm -f "$1"
  return 1
}
'

do_install() {
  printf '%s copying diagnostics to %s:%s\n' "$(bold '==>')" "$UNRAID_HOST" "$REMOTE_DIR"
  remote "mkdir -p '$REMOTE_DIR' '$LOG_DIR'" || exit 2
  scp -q \
    "$REPO_ROOT/tools/stream-access-log.py" \
    "$REPO_ROOT/tools/stream-watch.py" \
    "$SSH_TARGET:$REMOTE_DIR/" || exit 2
  remote "chmod +x '$REMOTE_DIR'/stream-access-log.py '$REMOTE_DIR'/stream-watch.py"
}

do_start() {
  do_install
  printf '%s starting\n' "$(bold '==>')"
  # setsid detaches from this ssh session, so the processes outlive the
  # connection rather than dying with it.
  remote "$REMOTE_ALIVE_FN
    cd '$REMOTE_DIR' || exit 2
    if alive_pid access.pid stream-access-log.py; then
      echo 'access log already running'
    else
      PORT='$ETV_PORT_HOST' LOG_FILE='$ACCESS_LOG' \
        setsid nohup ./stream-access-log.py >/dev/null 2>&1 &
      echo \$! > access.pid
      echo 'access log started'
    fi
    if alive_pid watch.pid stream-watch.py; then
      echo 'stream watcher already running'
    else
      HLS_ROOT='$ETV_STATION_DATA/hls' LOG_FILE='$EVENTS_LOG' \
        setsid nohup ./stream-watch.py >/dev/null 2>&1 &
      echo \$! > watch.pid
      echo 'stream watcher started'
    fi
  "
  do_status
}

do_stop() {
  printf '%s stopping\n' "$(bold '==>')"
  # Signal the whole process group, not just the script. `start` launches each
  # diagnostic under setsid, so it leads its own group and the tcpdump it spawns
  # sits in that group beside it; a negative pid reaches both.
  #
  # What this replaces was a second, separate kill that hunted the tcpdump by
  # grepping the process list for its capture filter. The filter is written in
  # stream-access-log.py and the pattern that had to match it lived down here,
  # with nothing tying the two together — so rewording the filter would have
  # left a tcpdump running against a log nobody reads, while this script
  # reported a clean stop. Nothing here spells out the filter any more.
  #
  # Reaching a whole group makes alive_pid's identity check load-bearing rather
  # than merely tidy: a stale pid file naming a number the kernel has since
  # handed to something else would otherwise take down that program and every
  # process it started.
  remote "$REMOTE_ALIVE_FN
    cd '$REMOTE_DIR' 2>/dev/null || exit 0
    stop_one() {
      if alive_pid \"\$1.pid\" \"\$2\"; then
        if kill -TERM \"-\$ALIVE_PID\" 2>/dev/null || kill \"\$ALIVE_PID\" 2>/dev/null; then
          echo \"stopped \$1 (\$ALIVE_PID)\"
          rm -f \"\$1.pid\"
        else
          echo \"could not signal \$1 (\$ALIVE_PID) — still running\"
        fi
      else
        echo \"\$1 was not running\"
      fi
    }
    stop_one access stream-access-log.py
    stop_one watch stream-watch.py
  "
}

do_status() {
  printf '%s status\n' "$(bold '==>')"
  # Absolute paths rather than a cd, so a missing diag folder is just one more
  # script reported as never installed instead of a separate one-line dead end.
  remote "$REMOTE_ALIVE_FN
    report() {
      label=\$1; pid_file='$REMOTE_DIR'/\$2; script='$REMOTE_DIR'/\$3
      if alive_pid \"\$pid_file\" \"\$3\"; then
        echo \"  RUNNING        \$label (pid \$ALIVE_PID)\"
      elif [ -f \"\$script\" ]; then
        echo \"  NOT RUNNING    \$label — installed on this host but no process is alive.\"
        echo \"                 Restart it (needed after every Unraid reboot): tools/diag-install.sh start\"
      else
        echo \"  NOT INSTALLED  \$label — never installed on this host, so it has never logged anything.\"
        echo \"                 Install and start it: tools/diag-install.sh start\"
      fi
    }
    report 'access log' access.pid stream-access-log.py
    report 'stream watcher' watch.pid stream-watch.py
    echo
    for log in '$ACCESS_LOG' '$EVENTS_LOG'; do
      if [ -f \"\$log\" ]; then
        echo \"  \$log — \$(wc -l < \"\$log\" | tr -d ' ') rows, \$(du -h \"\$log\" | cut -f1)\"
      else
        echo \"  \$log — not created yet\"
      fi
    done
    echo
    echo '  latest stream events:'
    tail -5 '$EVENTS_LOG' 2>/dev/null | sed 's/^/    /' || true
  "
}

do_logs() {
  local which="${1:-both}"
  case "$which" in
    access) remote "tail -40 '$ACCESS_LOG'" ;;
    events) remote "tail -40 '$EVENTS_LOG'" ;;
    both)
      printf '%s access log\n' "$(bold '==>')"
      remote "tail -20 '$ACCESS_LOG' 2>/dev/null || echo '(none yet)'"
      printf '\n%s stream events\n' "$(bold '==>')"
      remote "tail -20 '$EVENTS_LOG' 2>/dev/null || echo '(none yet)'"
      ;;
    *)
      printf '%s unknown log %s (use access, events, or nothing)\n' "$(red 'fatal:')" "$which" >&2
      exit 2
      ;;
  esac
}

case "${1:-status}" in
  install) do_install ;;
  start)   do_start ;;
  stop)    do_stop ;;
  status)  do_status ;;
  logs)    do_logs "${2:-both}" ;;
  *)
    printf 'usage: %s {start|stop|status|logs [access|events]|install}\n' "$0" >&2
    exit 2
    ;;
esac

printf '%s done\n' "$(green 'OK')"
