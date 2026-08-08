#!/usr/bin/env bash
# Drive tools/diag-install.sh against a throwaway Linux container.
#
# Why a container and not the Mac: every liveness question this script asks is
# answered by reading /proc/<pid>/cmdline, and macOS has no /proc. Why a fake
# ssh and not the real Unraid box: a test must not touch the live host, and the
# remote bodies are the part worth exercising — they are the code that decides
# whether a diagnostic is running, gets restarted, or gets signalled.
#
# The fake `ssh` throws away every argument but the last and runs that last one
# (the remote command body) inside the container under /bin/sh, which is the
# same POSIX shell Unraid gives it. The fake `scp` copies into the container.
# Both are written to a temp directory at run time, so this script is the whole
# harness — nothing else needs to exist on disk.
#
# Nine cases, 18 assertions. Run it after any change to diag-install.sh:
#
#   tools/test-diag-install.sh
#
# Needs docker. Exits non-zero if any assertion fails.
set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required (the checks read /proc, which macOS does not have)" >&2
  exit 2
fi

SCRATCH=$(mktemp -d)
C="diag-test-$$"
trap 'docker rm -f "$C" >/dev/null 2>&1; rm -rf "$SCRATCH"' EXIT

mkdir -p "$SCRATCH/fakebin"

cat > "$SCRATCH/fakebin/ssh" <<'EOF'
#!/bin/sh
# Run the remote command body under /bin/sh inside the container, so /proc is
# real. Every arg but the last is ssh plumbing we ignore — including the host,
# which is why this can never reach the live box.
body=""
for a in "$@"; do body=$a; done
exec docker exec "${DIAG_TEST_CONTAINER:?}" /bin/sh -c "$body"
EOF

cat > "$SCRATCH/fakebin/scp" <<'EOF'
#!/bin/sh
c="${DIAG_TEST_CONTAINER:?}"
files=""
target=""
for a in "$@"; do
  case "$a" in
    -*) continue ;;
  esac
  if [ -n "$target" ]; then files="$files $target"; fi
  target=$a
done
dest=${target#*:}
for f in $files; do docker cp "$f" "$c:$dest" >/dev/null; done
EOF

cat > "$SCRATCH/reset.sh" <<'EOF'
#!/bin/sh
# Kill every process a case spawned (anything but pid 1's idle sleep) and clear
# the pid files, so each case starts from a known state.
for p in /proc/[0-9]*; do
  pid=${p#/proc/}
  [ "$pid" = 1 ] && continue
  c=$(tr '\000' ' ' < "$p/cmdline" 2>/dev/null)
  case "$c" in
    *stream-*|*"sleep 900"*|*"sleep 5"*) kill -9 "$pid" 2>/dev/null ;;
  esac
done
rm -f /scratch/appdata/diag/access.pid /scratch/appdata/diag/watch.pid
true
EOF

cat > "$SCRATCH/alive.sh" <<'EOF'
#!/bin/sh
# alive.sh <pid> -> "alive" or "dead".
#
# Not `kill -0`: the stubs are setsid'd, so their reaper is the container's idle
# sleep, which never reaps — a dead one lingers as a zombie that `kill -0` still
# reports as present. Not `[ -s ]` either: /proc/<pid>/cmdline always stats as
# zero bytes. Read the content.
c=$(tr -d '\000' < "/proc/$1/cmdline" 2>/dev/null)
if [ -n "$c" ]; then echo alive; else echo dead; fi
EOF

chmod +x "$SCRATCH/fakebin/ssh" "$SCRATCH/fakebin/scp"

export DIAG_TEST_CONTAINER=$C
export PATH="$SCRATCH/fakebin:$PATH"
# diag-install.sh takes an already-set value over anything in .env, so these
# hold whether or not the checkout has one. Before that was true this harness
# passed in a worktree and failed in the primary checkout, for no reason to do
# with the code under test.
export UNRAID_HOST=fake.invalid UNRAID_USER=root
export ETV_STATION_APPDATA=/scratch/appdata ETV_STATION_DATA=/scratch/data
DIAG=/scratch/appdata/diag

docker run -d --rm --name "$C" python:3.12-slim sleep 3600 >/dev/null || exit 2
docker exec "$C" mkdir -p $DIAG /scratch/data/diag
docker cp "$SCRATCH/reset.sh" "$C:/reset.sh" >/dev/null
docker cp "$SCRATCH/alive.sh" "$C:/alive.sh" >/dev/null

fail=0
pass=0
check() { # name, expected-substring, actual
  if printf '%s' "$3" | grep -qF "$2"; then
    pass=$((pass + 1)); printf 'ok    %s\n' "$1"
  else
    fail=$((fail + 1)); printf 'FAIL  %s\n      wanted: %s\n      got: %s\n' "$1" "$2" "$3"
  fi
}
notin() { # name, forbidden-substring, actual
  if printf '%s' "$3" | grep -qF "$2"; then
    fail=$((fail + 1)); printf 'FAIL  %s\n      must not contain: %s\n      got: %s\n' "$1" "$2" "$3"
  else
    pass=$((pass + 1)); printf 'ok    %s\n' "$1"
  fi
}
inc() { docker exec "$C" /bin/sh -c "$1" 2>&1; }
# No `exec` in the stub: exec would replace its cmdline with the sleep's and
# hide the very name the identity check reads. The TERM trap makes it die the
# instant `stop` signals it, instead of finishing its current sleep first and
# still looking alive to the assertion.
stub() { docker exec "$C" /bin/sh -c "printf '%s\n' '#!/bin/sh' \"trap 'exit 0' TERM\" 'while true; do sleep 5 & wait; done' > $DIAG/$1; chmod +x $DIAG/$1"; }
reset() { docker exec "$C" /bin/sh /reset.sh >/dev/null 2>&1; }
status() { "$REPO/tools/diag-install.sh" status 2>&1; }

echo "=== case 1: genuinely running script reports RUNNING"
reset; stub stream-access-log.py; stub stream-watch.py
real_pid=$(inc "cd $DIAG && setsid ./stream-access-log.py >/dev/null 2>&1 & echo \$!")
inc "echo $real_pid > $DIAG/access.pid" >/dev/null
out=$(status)
check "reports RUNNING with the real pid" "RUNNING        access log (pid $real_pid)" "$out"
check "pid file survives" "$real_pid" "$(inc "cat $DIAG/access.pid")"

echo
echo "=== case 2: pid reused by an unrelated live process reports NOT RUNNING"
reset; stub stream-access-log.py; stub stream-watch.py
other_pid=$(inc "setsid sleep 900 >/dev/null 2>&1 & echo \$!")
inc "echo $other_pid > $DIAG/access.pid" >/dev/null
out=$(status)
check "reports NOT RUNNING" "NOT RUNNING    access log" "$out"
notin "does not report RUNNING" "RUNNING        access log" "$out"
check "stale pid file removed" "gone" "$(inc "[ -e $DIAG/access.pid ] && echo present || echo gone")"
check "innocent process left alive" "alive" "$(inc "/bin/sh /alive.sh $other_pid")"

echo
echo "=== case 3: pid file naming a dead pid"
reset; stub stream-access-log.py; stub stream-watch.py
inc "echo 999999 > $DIAG/access.pid" >/dev/null
out=$(status)
check "reports NOT RUNNING" "NOT RUNNING    access log" "$out"
check "stale pid file removed" "gone" "$(inc "[ -e $DIAG/access.pid ] && echo present || echo gone")"

echo
echo "=== case 4/5: no pid file, and nothing installed"
reset; stub stream-access-log.py; stub stream-watch.py
out=$(status)
check "installed but no pid file -> NOT RUNNING" "NOT RUNNING    access log" "$out"
inc "rm -f $DIAG/stream-access-log.py" >/dev/null
out=$(status)
check "no script -> NOT INSTALLED" "NOT INSTALLED  access log" "$out"

echo
echo "=== case 6: start restarts a script whose pid was reused"
reset; stub stream-access-log.py; stub stream-watch.py
other_pid=$(inc "setsid sleep 900 >/dev/null 2>&1 & echo \$!")
inc "echo $other_pid > $DIAG/access.pid" >/dev/null
out=$("$REPO/tools/diag-install.sh" start 2>&1)
check "start actually starts it" "access log started" "$out"
notin "start does not claim already running" "access log already running" "$out"
check "fresh pid file written" "present" "$(inc "[ -e $DIAG/access.pid ] && echo present || echo gone")"

echo
echo "=== case 7: start leaves a genuinely running script alone"
reset; stub stream-access-log.py; stub stream-watch.py
real_pid=$(inc "cd $DIAG && setsid ./stream-access-log.py >/dev/null 2>&1 & echo \$!")
inc "echo $real_pid > $DIAG/access.pid" >/dev/null
out=$("$REPO/tools/diag-install.sh" start 2>&1)
check "start says already running" "access log already running" "$out"

echo
echo "=== case 8: stop does not kill the stranger holding a reused pid"
reset; stub stream-access-log.py; stub stream-watch.py
other_pid=$(inc "setsid sleep 900 >/dev/null 2>&1 & echo \$!")
inc "echo $other_pid > $DIAG/access.pid" >/dev/null
out=$("$REPO/tools/diag-install.sh" stop 2>&1)
check "reports not running" "access was not running" "$out"
check "stranger untouched" "alive" "$(inc "/bin/sh /alive.sh $other_pid")"

echo
echo "=== case 9: stop kills the genuine process"
reset; stub stream-access-log.py; stub stream-watch.py
real_pid=$(inc "cd $DIAG && setsid ./stream-access-log.py >/dev/null 2>&1 & echo \$!")
inc "echo $real_pid > $DIAG/access.pid" >/dev/null
out=$("$REPO/tools/diag-install.sh" stop 2>&1)
check "reports stopped" "stopped access ($real_pid)" "$out"
# Read cmdline, not `kill -0` — see alive.sh.
check "process is gone" "dead" "$(inc "/bin/sh /alive.sh $real_pid")"

echo
printf 'passed %s, failed %s\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
