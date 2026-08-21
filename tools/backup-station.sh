#!/usr/bin/env bash
# Snapshot the station's irreplaceable state on the Unraid host.
#
# WHAT ROLLBACK ACTUALLY NEEDS TO PROTECT. The container is the easy thing to
# restore — `admin deploy` rebuilds it from the image in a minute. The hard
# things live on the data volume and have no other copy anywhere:
#
#   playout/history.db  the only record of what each channel has aired
#                       (deploy/appdata/README.md:60). Losing it resets every
#                       channel's resume position — 64 channels start over.
#   .device_id          the tuner identity. Cannot be regenerated
#                       (crates/etv-station/src/etv_next.rs:267-276): mint a new
#                       one and Plex silently drops the channel mapping for all
#                       64 channels.
#   catalog.db          nominally "a rebuildable cache" (README.md:53), but a
#                       rebuild is neither cheap nor guaranteed faithful — on
#                       2026-08-16 Radarr renamed four films without bumping
#                       Plex's `updatedAt`, so no delta could see the new paths
#                       and two channels aired black.
#   station.yaml,       the deployed config. Also in deploy/ on the Mac, but a
#   channels/           snapshot beside the databases makes a restore one step.
#
# DELIBERATELY EXCLUDED: artwork/ (24 GB, re-fetchable from Plex), diag/ (704 MB,
# disposable), hls/ (a working set), plexdb.snapshot.db (written by the separate
# plex-db-ex container). Skipping those is what keeps a snapshot ~190 MB and a
# few seconds rather than a 25 GB copy nobody would run often enough to help.
#
# RUNS ON THE HOST, PIPED IN OVER SSH — never copied to it. sqlite3 lives at
# /usr/bin/sqlite3 on Unraid, and this file is fed to `bash -s` from the repo, so
# there is no host-side copy to go stale or be wiped by a deploy. That is the
# same failure the ffmpeg probe had (#258) and it is not worth repeating.
#
# WHY sqlite3 .backup AND NOT cp: history.db carries a live multi-megabyte WAL
# while the daemon writes. A plain copy of the .db alone captures a file whose
# committed data is still in the WAL, and copying .db/.wal/.shm separately races
# a checkpoint. `.backup` uses SQLite's online backup API and yields one
# consistent file with no need to stop the container.
#
# Env (all required; supplied by admin from .env):
#   ETV_STATION_APPDATA  ETV_STATION_DATA  ETV_STATION_BACKUP_DIR
#   ETV_STATION_BACKUP_KEEP  (optional, default 10)
set -uo pipefail

: "${ETV_STATION_APPDATA:?}"
: "${ETV_STATION_DATA:?}"
: "${ETV_STATION_BACKUP_DIR:?}"
KEEP="${ETV_STATION_BACKUP_KEEP:-10}"

command -v sqlite3 >/dev/null 2>&1 || {
    echo "fatal: no sqlite3 on this host — cannot take a consistent snapshot" >&2
    exit 2
}

stamp=$(date -u +%Y%m%dT%H%M%SZ)
dest="$ETV_STATION_BACKUP_DIR/$stamp"
mkdir -p "$dest" || { echo "fatal: cannot create $dest" >&2; exit 2; }

fail=0
note() { printf '  %s\n' "$*"; }

# Copy one sqlite database through the online backup API, then prove the result
# opens and is structurally sound. An unverified backup is worse than none: it
# reads as protection right up to the moment it is needed.
snap_db() {
    local src="$1" name="$2"
    if [ ! -f "$src" ]; then
        note "SKIP $name (not present at $src)"
        return 0
    fi
    if ! sqlite3 "$src" ".backup '$dest/$name'" 2>/dev/null; then
        note "FAIL $name (.backup errored)"
        fail=1
        return 1
    fi
    local check
    check=$(sqlite3 "$dest/$name" 'PRAGMA quick_check;' 2>/dev/null | head -1)
    if [ "$check" != "ok" ]; then
        note "FAIL $name (quick_check said: ${check:-<nothing>})"
        fail=1
        return 1
    fi
    note "ok   $name ($(du -h "$dest/$name" | cut -f1), quick_check ok)"
}

copy_path() {
    local src="$1" name="$2"
    if [ ! -e "$src" ]; then
        note "SKIP $name (not present)"
        return 0
    fi
    if cp -a "$src" "$dest/$name" 2>/dev/null; then
        note "ok   $name"
    else
        note "FAIL $name (copy errored)"
        fail=1
    fi
}

echo "snapshot -> $dest"
snap_db "$ETV_STATION_DATA/playout/history.db" history.db
snap_db "$ETV_STATION_DATA/catalog.db" catalog.db
copy_path "$ETV_STATION_APPDATA/.device_id" device_id
copy_path "$ETV_STATION_APPDATA/station.yaml" station.yaml
copy_path "$ETV_STATION_APPDATA/channels" channels

if [ "$fail" -ne 0 ]; then
    # Leave the partial directory in place: it is evidence, and deleting it
    # would also hide that the run happened at all.
    echo "BACKUP INCOMPLETE — kept $dest for inspection, and pruned nothing." >&2
    echo "Older snapshots are untouched, so the last good one is still there." >&2
    exit 1
fi

{
    printf 'taken %s\n' "$stamp"
    printf 'host  %s\n' "$(uname -n)"
    ( cd "$dest" && find . -type f -exec sha256sum {} + 2>/dev/null | sort -k2 )
} > "$dest/MANIFEST.txt" 2>/dev/null

# Prune ONLY after a fully verified snapshot. Pruning on a failed run is how a
# broken backup job quietly eats the history it was meant to protect.
mapfile -t all < <(find "$ETV_STATION_BACKUP_DIR" -mindepth 1 -maxdepth 1 -type d -print 2>/dev/null | sort)
total=${#all[@]}
if [ "$total" -gt "$KEEP" ]; then
    drop=$((total - KEEP))
    for old in "${all[@]:0:$drop}"; do
        rm -rf "$old" && note "pruned $(basename "$old")"
    done
fi

echo "OK $(du -sh "$dest" | cut -f1) in $dest — $(( total > KEEP ? KEEP : total ))/$KEEP kept"
