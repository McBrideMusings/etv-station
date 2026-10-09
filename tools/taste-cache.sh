#!/usr/bin/env bash
# Keeps the local copies tools/taste-debug.sh and tools/taste-lab.sh read under
# tmp/claude/scratchpad/taste-debug/ in step with the Unraid host's
# /mnt/user/appdata/etv-station/data/.
#
#   tools/taste-cache.sh catalog.db plexdb.snapshot.db
#
# One SSH call reads each named database's schema_version and its newest write
# time (the later of the file and its -wal) on the host. A local copy that is
# missing, at a different schema, or older than the host's last write is
# refetched. A copy left at an older schema than the reader expects fails deep
# inside the tool with an error that suggests migrating the store, when what it
# needs is a refetch.
#
# The fetch streams `sqlite3 .dump` from the host and rebuilds the database
# here. The daemon writes catalog.db in WAL mode while this runs, and copying
# the file and its -wal separately can pair pages from two different moments;
# a dump reads one transaction. It writes nothing on the host.
#
# Exit 1 when a named copy is missing and can't be fetched, or when the host
# proved the local copy stale and the refetch failed. When the host is
# unreachable it keeps a local copy it already has, unchecked, and says so.
set -uo pipefail

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

CACHE_DIR="tmp/claude/scratchpad/taste-debug"
REMOTE_DIR="/mnt/user/appdata/etv-station/data"

if [ "$#" -eq 0 ]; then
  echo "usage: tools/taste-cache.sh <catalog.db|plexdb.snapshot.db>..." >&2
  exit 2
fi

mkdir -p "$CACHE_DIR"

local_version() {
  [ -f "$CACHE_DIR/$1" ] || return 0
  sqlite3 -readonly "$CACHE_DIR/$1" "SELECT MAX(version) FROM schema_version" 2>/dev/null
}

# Prints one `name<TAB>OK<TAB>version<TAB>stamp` or `name<TAB>ERR<TAB>message`
# line per database.
remote_state() {
  ssh -o BatchMode=yes -o ConnectTimeout=5 "$target" bash -s -- "$REMOTE_DIR" "$@" <<'EOF'
dir="$1"; shift
for name in "$@"; do
  f="$dir/$name"
  if ! v="$(sqlite3 -readonly "$f" "SELECT MAX(version) FROM schema_version" 2>&1)"; then
    printf '%s\tERR\t%s\n' "$name" "$(printf '%s' "$v" | tr '\n\t' '  ')"
    continue
  fi
  stamp="$(stat -c %Y "$f" "$f-wal" 2>/dev/null | sort -n | tail -1)"
  printf '%s\tOK\t%s\t%s\n' "$name" "$v" "$stamp"
done
EOF
}

# Rebuilds $1 from the host's dump; prints why on failure.
fetch() {
  local name="$1" dest="$CACHE_DIR/$1" tmp="$CACHE_DIR/$1.new"
  rm -f "$tmp"
  if ! ssh -o BatchMode=yes "$target" "sqlite3 -readonly '$REMOTE_DIR/$name' .dump | gzip -1" \
      | gunzip | sqlite3 -bail "$tmp" >/dev/null; then
    rm -f "$tmp"
    echo "dump or rebuild failed"
    return 1
  fi
  local check
  check="$(sqlite3 -readonly "$tmp" "PRAGMA quick_check" 2>&1)"
  if [ "$check" != "ok" ]; then
    rm -f "$tmp"
    echo "rebuilt copy failed quick_check: $check"
    return 1
  fi
  rm -f "$dest-wal" "$dest-shm"
  mv "$tmp" "$dest"
}

host_ok=0
remote=""
if [ -n "${UNRAID_HOST:-}" ]; then
  target="${UNRAID_USER:-root}@$UNRAID_HOST"
  if remote="$(remote_state "$@")"; then
    host_ok=1
  fi
fi

status=0
for name in "$@"; do
  dest="$CACHE_DIR/$name"
  stamp_file="$dest.host-stamp"
  have="$(local_version "$name")"

  if [ "$host_ok" -eq 0 ]; then
    if [ -f "$dest" ]; then
      echo "taste-cache: $name: host unreachable, using local copy at schema ${have:-?} unchecked" >&2
    else
      echo "taste-cache: $name: no local copy and host unreachable (UNRAID_HOST=${UNRAID_HOST:-unset})" >&2
      status=1
    fi
    continue
  fi

  line="$(printf '%s\n' "$remote" | awk -F'\t' -v n="$name" '$1 == n')"
  state="$(printf '%s' "$line" | cut -f2)"
  if [ "$state" != "OK" ]; then
    echo "taste-cache: $name: host can't report its version: $(printf '%s' "$line" | cut -f3-)" >&2
    [ -f "$dest" ] || status=1
    continue
  fi
  want="$(printf '%s' "$line" | cut -f3)"
  host_stamp="$(printf '%s' "$line" | cut -f4)"
  had_stamp="$(cat "$stamp_file" 2>/dev/null)"

  if [ "$have" = "$want" ] && [ "$had_stamp" = "$host_stamp" ]; then
    echo "taste-cache: $name: up to date (schema $have)" >&2
    continue
  fi

  if [ "$have" != "$want" ]; then
    why="local schema ${have:-missing}, host schema $want"
  else
    why="host written since the local copy was fetched"
  fi
  echo "taste-cache: $name: $why — fetching" >&2
  if ! err="$(fetch "$name")"; then
    echo "taste-cache: $name: fetch failed: $err" >&2
    status=1
    continue
  fi
  got="$(local_version "$name")"
  if [ "$got" != "$want" ]; then
    echo "taste-cache: $name: fetched copy is at schema ${got:-?}, host reported $want" >&2
    status=1
    continue
  fi
  printf '%s\n' "$host_stamp" > "$stamp_file"
  echo "taste-cache: $name: now schema $got" >&2
done

exit "$status"
