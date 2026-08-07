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
# after one; `status` will tell you they are not running.
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

if [ -f "$REPO_ROOT/.env" ]; then
  set -a
  # shellcheck disable=SC1091
  . "$REPO_ROOT/.env"
  set +a
fi

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
  remote "
    cd '$REMOTE_DIR' || exit 2
    if [ -f access.pid ] && kill -0 \"\$(cat access.pid)\" 2>/dev/null; then
      echo 'access log already running'
    else
      PORT='$ETV_PORT_HOST' LOG_FILE='$ACCESS_LOG' \
        setsid nohup ./stream-access-log.py >/dev/null 2>&1 &
      echo \$! > access.pid
      echo 'access log started'
    fi
    if [ -f watch.pid ] && kill -0 \"\$(cat watch.pid)\" 2>/dev/null; then
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
  remote "
    cd '$REMOTE_DIR' 2>/dev/null || exit 0
    for name in access watch; do
      if [ -f \$name.pid ]; then
        pid=\$(cat \$name.pid)
        if kill -0 \$pid 2>/dev/null; then
          kill \$pid 2>/dev/null && echo \"stopped \$name (\$pid)\"
        else
          echo \"\$name was not running\"
        fi
        rm -f \$name.pid
      else
        echo \"\$name was not running\"
      fi
    done
    pkill -f 'tcp dst port $ETV_PORT_HOST' 2>/dev/null || true
  "
}

do_status() {
  printf '%s status\n' "$(bold '==>')"
  remote "
    cd '$REMOTE_DIR' 2>/dev/null || { echo 'not installed'; exit 0; }
    for pair in 'access log:access.pid' 'stream watcher:watch.pid'; do
      label=\${pair%%:*}; file=\${pair##*:}
      if [ -f \$file ] && kill -0 \"\$(cat \$file)\" 2>/dev/null; then
        echo \"  RUNNING  \$label (pid \$(cat \$file))\"
      else
        echo \"  stopped  \$label\"
      fi
    done
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
