---
name: verify-project
description: Boot the etv-station daemon locally and watch it schedule and emit playout. Use when verifying a change to this repo before committing.
---

# Verify etv-station

Rust daemon that schedules channels and writes playout JSON. It ships as a Docker
container on Unraid, but the useful local loop is running the debug binary against
`examples/` and watching channels roll.

## Before you commit: run both workspaces

`etv-station` is **two cargo workspaces**. The parent at the repo root, and
`vendor/etv-next/`, which has its own `[workspace]` and is *excluded* from the
parent — a `cargo test --workspace` or `cargo clippy --workspace` run at the root
never touches a single file under `vendor/etv-next/`. That gap let a clippy
`items_after_test_module` error (#305) pass every check, merge, and sit on `main`
for three orchestrate rounds.

```bash
./tools/verify-all.sh              # test + clippy + fmt --check, BOTH workspaces
./tools/verify-all.sh lint         # just clippy, still both
./tools/verify-all.sh test         # just tests, still both
```

The script is the only place the command list lives — `CLAUDE.md` and `admin.toml`
(`admin test` / `admin vet` / `admin fmt`) call it rather than repeating it. Each
tree gets its own documented commands: the vendored one adds `--locked`, taken
from `vendor/etv-next/CLAUDE.md`. Every selected check runs in both trees even
after one fails, then the summary names which failed and the exit status is 1.

Green baseline (2026-08-20): parent 824 tests, `vendor/etv-next` 148 tests
(404 ignored), clippy and fmt clean in both.

`crates/etv-overlay/tests/watch_teardown.rs:106` ("sanity: ffmpeg child should be
running") has an unguarded race, tracked as **#322**. If only that fails, re-run
it; do not chase it.

Passing this is necessary, not sufficient — the rest of this skill is booting the
daemon and watching it actually do its job.

## The handle

```bash
cargo build                        # ~21s cold, debug profile
admin dev-station                  # daemon only, .env sourced (no ETV-next build)
admin dev                          # daemon + ETV-next together
```

`admin dev-station` sources `.env` for you. If you run the binary directly you must
source it yourself or every `${…}` in the example configs fails validation:

```bash
set -a; . ./.env; set +a
./target/debug/etv-station --config examples/station.yaml
```

## What a pass looks like

Channel load lines first, then rolling. Observed on a good run (2026-08-12):

```
INFO etv_station: loaded channel event="channel.load" channel=x-files blocks=1 window_days=1 chunk_hours=6
INFO etv_station::daemon: local-fs catalog ingest complete event="catalog.ingest.fs" entries=0
INFO etv_station::daemon: contacting plex to ingest the catalog … event="catalog.ingest.plex_start" mode="full"
INFO … roll_tick: emitted playout files event="chunk.write" channel=marquee files=1
INFO … roll_tick: window already materialized through … event="chunk.skip" channel=starwars
```

`chunk.write` is the daemon doing its actual job. The artifacts land in
`examples/output/<channel>/` as `<start>_<end>.json` alongside `.history` and `.resume` —
check file mtimes there to confirm your run wrote something rather than reading a file
from a previous one.

`chunk.skip` with "window already materialized" is a pass, not a stall: the window was
already built. To force real work, change the channel config or clear that channel's
output dir.

## Verifying the catalog refresh and the reconciliation sweep

**Nothing here is an operation anyone runs.** Both are timers inside the daemon:
the deployed container re-ingests the catalog and sweeps its own playout JSON
every `catalog_refresh_secs`, forever, with no command, no signal, and no
restart. This section is only how to *watch it happen locally* before shipping a
change to it — the same reason the rest of this skill boots a debug binary
against `examples/`. In prod you read the logs (`admin logs`), you do not
trigger anything.

The refresh is a `select!` arm on `catalog_refresh_secs`, so a default station
(900s) shows nothing for fifteen minutes. There is no env override for it —
`load.rs` reads only `ETV_STATION_TZ`/`OUTPUT_BASE`/`CATALOG`/`SOURCE_ROOTS`/
`IDENTITY_ROOTS`/`ARTWORK_CACHE` — so set it low in `examples/station.yaml`
itself, then `admin dev-station` and watch for a second ingest line:

```
INFO … event="catalog.ingest.fs" sources_marked_missing=0 entries_marked_missing=0
INFO … event="catalog.ingest.plex" mode="delta" sources_marked_missing=… entries_marked_missing=…
```

Those two counters are the soft delete (ADR 0006) reporting; they are only ever
non-zero on a **full** pass, so a delta line showing 0 is correct, not a stall.

To exercise the sweep end to end without waiting on Plex:

1. Run the station until a channel has playout JSON covering the next hour.
2. `mv` one of the referenced media files to a new name under the same root.
3. Wait one `catalog_refresh_secs`.

A pass logs one line per patched item plus a summary:

```
INFO … event="reconcile.path_patched" item=imdb:tt0095016 was=/media/old.mkv now=/media/new.mkv
INFO … event="reconcile.swept" files_examined=4 files_rewritten=1 paths_patched=1 items_carded=0
```

`event="reconcile.clean"` at DEBUG is the steady state — playout already agrees
with the catalog, and nothing was rewritten. A file's mtime is the ground truth:
a clean sweep must not touch it. Delete the media file instead of renaming it and
the same tick logs `event="reconcile.item_carded"` and the slot becomes a black
card that still carries the title in the guide.

On the deployed container the same three lines are the whole story, and the
counters are the thing to read: a `reconcile.swept` with `paths_patched` above
zero means the station just repaired chunks that would otherwise have aired
black. `admin logs | grep reconcile` is the check after a big Radarr rename
batch — not because anything needs doing, but to confirm nothing did.

## Verifying a graphics overlay actually reaches the screen

**Fastest check: `admin overlay-watch <channel-dir>`** — no station, no
ETV-next, no Plex. Loops a background fixture through the real Vello+Rhai
render and streams it into VLC, hot-reloading on every save to the channel's
overlay (or the shared file it references). Defaults to a 10x time scale so a
multi-minute animation cycle (e.g. `title-chyron.rhai`) plays out in seconds.
This is the right first stop for anything overlay-shaped — layout, color, a
new script, an animation — and the only one of these that doesn't need a real
channel or a real stream. Reach for the rest of this section only once the
isolated render looks right and you need to confirm it survives contact with
the real station (a script reading real `title`/`item_elapsed`, real PNG
decode, the fifo/process-supervision path).

The overlay spec lives on the **channel**, in `channel{N}.json` — never on a
playout item. Check the render first, because it is instant and needs no stream:

```sh
mkdir -p tmp/claude/render
cargo run -q -p etv-station --bin etv-station -- \
  --config <a station yaml> --render-etv-next tmp/claude/render
python3 -c "import json;print(json.load(open('tmp/claude/render/channel1.json')).get('overlay'))"
```

A channel with an overlay prints its own `fifo_path`; a channel without prints
`None`. Two channels printing the *same* fifo path is a bug — each writes its own.

**Only a real frame proves it.** The spec reaching ffmpeg does not mean anything
was drawn: the overlay process is spawned on demand and can crash after ffmpeg
has already committed to reading its fifo. Grab a frame and look:

```sh
ffmpeg -y -i "http://$HOST:$PORT/channel/<N>.m3u8" -frames:v 1 tmp/claude/frame.png
ffmpeg -y -i tmp/claude/frame.png -vf "crop=440:200:840:520" tmp/claude/corner.png
```

Every overlay in the deployed station is `corner = "bottom_right"`, so that crop
is where the logo is. Stack several crops with `vstack` and label each with
`drawtext` before reading them — on dark content the boundary between two
unlabelled crops is impossible to place, and a logo gets attributed to the wrong
channel.

**A fully black frame is the overlay failing, not the channel being idle.** The
channel worker opens the fifo and waits for a writer; if `etv-overlay` died there
is no writer, and ffmpeg blocks. The cause is in the container log:

```sh
admin logs | grep -iE "overlay\.(spawn|exit)|unsupported PNG"
```

`etv-overlay` accepts **8-bit RGBA PNGs only**. A GrayscaleAlpha or Palette PNG
exits 1 in a crash-loop and blacks out every channel using it. Audit before
adding artwork — `d[25]` is the PNG colour type (6) and `d[24]` the bit depth (8):

```sh
python3 -c "
import glob
for f in sorted(glob.glob('deploy/appdata/channels/*/logo.png') + glob.glob('deploy/appdata/shared/*.png')):
    d=open(f,'rb').read(33)
    if (d[25],d[24])!=(6,8): print('CONVERT', f, d[25], d[24])"
ffmpeg -y -i bad.png -pix_fmt rgba fixed.png   # the fix; alpha survives
```

## Two blockers you will hit, and how to get past them

**1. `PLEXDB_SNAPSHOT_PATH` is not in `.env`.** `examples/samples/foryou.yaml` references
it, and config validation resolves env vars eagerly, so the daemon exits before serving:

```
ERROR failed to load configuration event="config.error" error=invalid config at examples/samples/foryou.yaml:
  env var `PLEXDB_SNAPSHOT_PATH` referenced by "${PLEXDB_SNAPSHOT_PATH}" is not set
```

**2. The plexdb reader is a schema version behind.** Pointing it at a store built by the
current sibling `plex-db-ex` gets you:

```
datastore "taste" … could not be opened: store at … is schema version 8,
but plexdb-reader only understands version 7
```

In a `plex-db-ex` checkout, `PLEXDB_PATH=/tmp/x.db uv run plexdb init` creates a **v8**
store today, so it cannot satisfy this reader. Until the reader is rebuilt against a
matching plex-db-ex, the sample channels cannot load at all — this is a real
incompatibility between the two projects, not a config mistake.

**The workaround that gets the daemon up:** drop the sample channels. `examples/station.yaml`
registers them with a glob, so copy it inside `examples/` (paths resolve relative to the
config file — a copy under `tmp/` fails with `channel pattern "channels/*.yaml" matched no
files`) and comment out the samples line:

```bash
sed 's|^  - samples/\*\.yaml|# samples excluded|' examples/station.yaml > examples/verify-smoke.yaml
set -a; . ./.env; set +a
./target/debug/etv-station --config examples/verify-smoke.yaml
rm examples/verify-smoke.yaml     # it is untracked; delete it when done
```

That boots clean and rolls all of `examples/channels/`.

## The HTTP surface is not the daemon

`admin.toml` lists `http://127.0.0.1:8409/channels.m3u`, `/xmltv.xml`, `/channel/1.m3u8`.
**The station daemon alone does not bind 8409** — I polled it for 150s while the daemon was
happily rolling channels and nothing ever listened. Those URLs are the container's, served
with ETV-next. For the HTTP surface locally use `admin dev` (daemon + ETV-next), not
`admin dev-station`.

## Gotchas

- The first Plex ingest reads the whole library and genuinely takes minutes
  (`catalog.ingest.plex_start mode="full"`). It hits the live Plex server — read-only, but
  it is real network traffic to the real box.
- `admin deploy files` vs `admin deploy image`: a new channel or edited block is `files`;
  a code change is `image`. `files` is the cheap one and is also the one that historically
  broke ownership on arrival — see the `post_sync` chown comment in `admin.toml`.
- Remote log tailing is `admin diag` (access log + stream events over ssh).
