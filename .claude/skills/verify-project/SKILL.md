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

Green baseline (2026-08-20): parent 832 tests, `vendor/etv-next` 148 tests
(404 ignored), clippy and fmt clean in both.

`crates/etv-overlay/tests/watch_teardown.rs:106` ("sanity: ffmpeg child should be
running") has an unguarded race, tracked as **#322**. If only that fails, re-run
it; do not chase it.

`determinism::tests::a_plugin_reading_wall_clock_time_is_caught`
(`crates/etv-station/src/determinism.rs:718`) is flaky the same way: the fixture
reads Rhai's own wall clock, and when both passes land inside the same tick the
two schedules match and the test panics with "the two passes matched". Observed
2026-08-20 passing and failing on back-to-back runs of the identical tree. Re-run
it; if it is the only failure, the tree is green.

`tools/epg-browser.py` is Python, so `verify-all.sh` never touches it. `admin
test` runs `tools/epg-layout-check.py` alongside the cargo trees: it drives the
real TUI under Textual's headless driver against fixture data (no network, no
station) at 80x24 and fails if any pane spills past the terminal edge. Run it
directly at another size with `uv run tools/epg-layout-check.py 128 40`. The
three-column layout it replaced needed 128 columns before titles clipped, which
is wider than a split pane.

`admin epg`'s action must stay `kind = "shell-passthrough"` with `interactive =
true`. That is the only action kind that hands the child this terminal's stdin
and stdout directly. It was `kind = "interactive-shell"` until 2026-08-21, which
launches with `stdout=subprocess.PIPE` and ignores `interactive` entirely — the
flag is only read by the shell-passthrough factory. `shutil.get_terminal_size()`
then saw no tty and returned its 80x24 fallback, so the TUI composited a 24-row
frame into a 45-row pane and the rows below its footer kept a stale copy of an
earlier frame. Measured in a 120x45 pty: the piped path reports `isatty=False,
size=(80, 24)`; the passthrough path reports `isatty=True, size=(120, 45)`. If
the TUI ever renders duplicated or half-height again, check that kind first —
`epg-layout-check.py` cannot see this, since it never touches a terminal.

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

## Verifying the seed cascade

A channel with no `seed:` inherits the station `seed:`, salted with the
channel's folder name (#324). Two surfaces prove it:

```bash
cargo test -p etv-station --test station_seed_cascade   # load() → derived, salted, stable seeds
```

At the daemon, boot the same config twice — once with `seed:` on the station
file, once without — and read the startup log:

```
# station seed set: nothing about seeds in the log, channels just load
INFO etv_station: loaded channel event="channel.load" channel=alpha …

# station seed absent: one INFO line per unseeded channel, naming it
INFO etv_station::config::load: no channel seed and no station seed:
     this channel reshuffles on every generation channel=alpha
```

That second line is the pass condition for "an unseeded channel is visible
rather than silent". Seeing it in a production log means that channel still
redraws a wall-clock seed on every regeneration and reshuffles its whole future
window whenever a catalog refresh changes its candidate set.

## Verifying two builds emit the same schedule (playout-JSON parity)

For a change that should not alter what airs — a config-format migration, a
refactor of the resolve path — generate with both builds and compare. Do **not**
compare bytes. The daemon anchors the first item at wall-clock now
(`daemon.rs:2078`, `resolve.rs:155`), so two runs minutes apart share no
timestamp and no trailing chunk filename; on a 64-channel config, 0 of 331 files
match byte-for-byte even when the binary and config are identical. Compare after
re-anchoring each channel's items to an offset from its own first start.

The harness from #310 does this, in `tmp/claude/parity/` (gitignored, rebuild it
if it's gone — `run.sh` is ~30 lines):

```bash
bash run.sh <label> <binary> <config-dir>     # one generation, own output + catalog copy
python3 diff_norm.py out-<a> out-<b>          # time-normalized; this is the verdict
python3 diff.py out-<a> out-<b>               # byte + per-item overlay-field comparison
```

Three rules that make the result mean something:

- **Always run a control.** Same binary, same config, second run. Its differing
  count is the floor; only channels that differ in the experiment *and not* in
  the control are real. Without it you cannot tell a regression from ordinary
  run-to-run drift.
- **Pin the catalog.** Copy `tmp/catalog.db` per run and start with no
  `PLEX_URL`/`PLEX_TOKEN`, or an ingest between the two runs changes the library
  underneath you. Rewrite `entry_sources.playback_path` from `/media/` to
  `/Volumes/media/library/` in the copy — otherwise every item resolves to a
  "file not found" card and the `source` field is never really compared.
- **Keep the two config trees at the same path depth**, or better, mask the path.
  A channel that fails validation emits a card whose text embeds its own config
  path, which then reads as a false difference.

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

## Verifying no two Plex items share one catalog entry (#340)

The symptom is a guide row, not a crash: a movie airing under a TV series'
title, artwork, season and episode. Channel 20 aired *Spider-Man: Brand New Day*
for 2h42m billed as "Dragon Ball Z, S3E27, The Last Wish". The cause is two Plex
rating keys collapsing onto one `entry_id`, which happened whenever a movie and
an episode shared a tmdb or tvdb number — those id spaces are partitioned by
media type and Plex reports the bare number.

**Read the guide first.** Every pool on `020-trending` is a movie collection, so
any season/episode on it is a defect:

```sh
uv run tools/epg-browser.py channel ersatztv.6
```

Look for: no programme with a non-null `season` or `episode`, and no `sub_title`.
One is enough to fail.

**Then ask the catalog directly.** This is the invariant, and it is one query —
the entry a rating key is pinned to is pinned for life (ADR 0009), so a merge
that gets in never unwinds on its own:

```sh
ssh "$UNRAID_USER@$UNRAID_HOST" "sqlite3 -readonly \
  /mnt/user/appdata/etv-station/data/catalog.db \
  'select count(*) from (select entry_id from entry_sources where source = \"plex\" \
    group by entry_id having count(*) > 1)'"
```

Look for: `0`. It read 1,446 before the v8 migration. A non-zero count that is
*not* a genuine multi-file dedupe (one title held as both 4K and 1080p — those
share a kind, so they are supposed to collapse) means the type partition leaked
again. To tell them apart, compare each entry's rating keys against the
`plex-db-ex` snapshot's `external_ids.kind`.

**Locally**, the same invariant is two unit tests, and they are cheaper than
either check above:

```sh
cargo test -p etv-station --lib catalog::ingest::plex::tests::a_movie_and_an_episode
cargo test -p etv-station --lib catalog::schema
```

The first ingests the real colliding pair and asserts they stay two entries with
the film's own title, no `show`, no season and no episode. The second drives the
v8 migration over a v7 catalog holding a merged row and asserts the row is
deleted, the FK cascade took its provenance, and `last_plex_ingest` is cleared —
that cleared cursor is the whole repair, because it is what forces the next start
into a full pass instead of a delta.

**After deploying a schema change, confirm the migration actually ran** rather
than assuming it did:

```sh
ssh "$UNRAID_USER@$UNRAID_HOST" "sqlite3 -readonly \
  /mnt/user/appdata/etv-station/data/catalog.db 'select max(version) from schema_version'"
```

Look for: the current `SCHEMA_VERSION` in `crates/etv-station/src/catalog/schema.rs`
(the length of `MIGRATIONS`). A v8 deploy also re-walks the whole library once,
so the first `catalog.ingest.plex` line after it reads `mode="full"` and takes
minutes, not seconds. That is the repair working, not a hang.

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

## Verifying hardware encoding (#258)

`admin verify-accel` — read-only, over ssh, touches no container.

**Codec name proves nothing here.** A software encode and a VAAPI encode both
produce H.264, so `ffprobe` on a segment reports `h264` either way, and ETV-next
logs a fallback to software only at DEBUG. The ffmpeg command line is the only
thing that separates them, which is why the check reads
`data/diag/ffmpeg-argv-ch<N>.log` and asserts, per channel's newest transcode:

- the expected encoder is present (`h264_vaapi` for `ETV_ACCEL=vaapi`)
- `libx264` is **absent** — its presence is the fallback fingerprint
- the configured render node (`/dev/dri/renderD128`) appears in the argv

Expected values are read from `deploy/unraid-template.xml`, so the check asserts
the host matches the repo rather than asking the host what it's set to.

**It reads the ffmpeg probe's argv logs.** The probe ships in the image
(`Dockerfile`) and is wired by `ffmpeg.ffmpeg_path` in
`deploy/appdata/station.yaml`, so it is always on — there is no re-apply step and
no host-side copy. A run reporting "no ffmpeg-argv-ch*.log found" means no
channel has transcoded since the container last started, not that the probe is
missing: the log is only written when a transcode begins, and only a channel
someone is watching transcodes at all.

A `STALE` verdict means the same thing in miniature — that channel's newest argv
is older than `STALE_DAYS` (3), so it proves the GPU worked then, not now. Tune
in to the channel and re-run for a current answer.

Locally, against a `admin dev` run: `tools/verify-accel.sh --local <diag-dir>`.

## Verifying no sub-segment transcodes (#339)

`admin verify-no-slivers` — read-only, over ssh, touches no container.

A **sliver** is an ffmpeg invocation whose `-t` is shorter than its own
`-hls_time`: a full process spawn, deep seek, hwaccel decode init and overlay
composite to emit a fraction of one segment. On 2026-08-22 prod had three —
channel 2 at 153ms, channel 1 at 1336ms, channel 9 at 3619ms, all against
`-hls_time 4`.

**It is not a scheduling defect, and the playout JSON is the wrong place to
look.** Every item the station emits is a whole catalog item; an item straddling
a chunk boundary is re-emitted whole in both chunks. The sliver is made by
`reburst_decision` in
`vendor/etv-next/crates/ersatztv-channel/src/channel_session.rs`, which restarts
a still-playing item once its buffer lead drains below `REBURST_AT_LEAD` (20s).
Before #339 it did that without asking whether any of the item was left, so a
reburst firing a second before an item ended handed its replacement 153ms of
work. The fix gates firing on `remaining > REBURST_AT_LEAD`, where `remaining` is
`item.finish - last_segment_end`.

The container log names the restart, and it is **not** an exit-75 stall:

```
lead down to 18s while still playing; restarting the item to rebuild the buffer
resuming the same item from 2026-08-21 20:31:18.460403686 +00:00:00
```

The check reads **every** invocation in each `ffmpeg-argv-ch<N>.log`, not just
the newest — a sliver is rare, and verify-accel's newest-block-only approach
would miss all three of the ones above. The segment length comes from each
invocation's own `-hls_time`, because it is a compile-time constant
(`SEGMENT_SECONDS` in `vendor/etv-next/crates/ffpipeline/src/pipeline.rs:36`) and
not a `station.yaml` key.

Locally, against a `admin dev` run: `tools/verify-no-slivers.sh --local <diag-dir>`.

**A pass on a freshly restarted container proves little.** Rebursts need an item
to have been playing long enough to build and then drain a lead, so a short run
has no chance to produce one. The meaningful read is against a host that has been
up for hours with channels being watched.

## Backups — what rollback actually protects

`admin backup` snapshots the host state that has no second copy, into
`$ETV_STATION_BACKUP_DIR/<UTC-stamp>/`, keeping the newest 10. Every `admin
deploy` runs it first.

What matters and why, so nobody "optimises" the list later:

- `playout/history.db` — the only record of what each channel has aired
  (`deploy/appdata/README.md:60`). Lose it and all 64 channels reset their
  resume position.
- `.device_id` — cannot be regenerated (`etv_next.rs:267-276`). A new one makes
  Plex silently drop the channel mapping for every channel.
- `catalog.db` — nominally rebuildable, but a rebuild can miss renamed files
  (the 2026-08-16 Radarr incident), so treat it as expensive to lose.

Deliberately excluded: `artwork/` (24 GB, re-fetchable) and `diag/` (704 MB,
disposable). That exclusion is why a snapshot is ~198 MB and 8 seconds.

Two safety properties worth not breaking:

- **A missing source file fails the run.** It used to count as a skip, which
  meant a wrong path produced an empty snapshot that then pruned the good ones.
- **Pruning happens only after a verified snapshot** — every database is
  reopened and `PRAGMA quick_check`'d first. A failed run prunes nothing and
  leaves its partial directory as evidence.

The pre-deploy run is stale-gated (`ETV_STATION_BACKUP_IF_STALE=1`), so a
`--dry-run` cannot churn the retention window; `admin backup` always snapshots.

## Gotchas

- The first Plex ingest reads the whole library and genuinely takes minutes
  (`catalog.ingest.plex_start mode="full"`). It hits the live Plex server — read-only, but
  it is real network traffic to the real box.
- `admin deploy files` vs `admin deploy image`: a new channel or edited block is `files`;
  a code change is `image`. `files` is the cheap one and is also the one that historically
  broke ownership on arrival — see the `post_sync` chown comment in `admin.toml`.
- Remote log tailing is `admin diag` (access log + stream events over ssh).

## Diagnosing a frozen channel (the overlay fifo)

A channel that freezes while somebody is watching shows the same symptom from
outside no matter which side broke: ffmpeg's `out_time` and frame counter stop
advancing, ffmpeg's stderr stays empty, and ETV-next's stall detector kills the
session ~60s later with exit 75. Three tools separate the causes.

| Tool | Runs where | Answers |
|---|---|---|
| `./tools/overlay-stall-repro.sh` | local | Does a silent overlay writer wedge ffmpeg? (yes — deterministic) |
| `./tools/overlay-heartbeat-check.sh` | local | Does the overlay's own clock report correctly when the reader stops? |
| `./tools/ffmpeg-probe-check.sh` | local | Does the probe wrapper still pass argv through untouched? |
| `./tools/progress-split-check.sh` | local | Does `ffmpeg_progress` land in its own rotated file and off stdout? |
| `./tools/two-clock-capture.sh --self-test` | local | Does the verdict logic still classify all 12 wait-channel shapes? |
| `two-clock.log` in the container | Unraid host | Which side stopped first, during a real freeze |

`two-clock-capture.sh` takes no channel argument and is not started by hand:
`docker/entrypoint.sh` runs it in the container, watching every channel with a
live transcode, so it comes back with the container. Read its evidence with
`docker exec etv-station sh -c 'grep -A6 "channel=<N>" /data/diag/two-clock.log'`.

**Both of the first two telemetry paths have failed silently before, which is why
they now have checks.** `two-clock-capture.sh` matched ffmpeg by a bare `ffmpeg `
argv[0] while `tools/ffmpeg-probe.sh` execs the real binary by absolute path — it
matched zero processes and recorded nothing across 25 stalls. And the probe
appended a second `-progress`, which replaced ETV-next's `-progress pipe:1` and
left `/data/diag/ffmpeg-progress.log` empty. Neither failure logged anything; both
looked healthy in `ps`. When a diagnostic goes quiet, suspect the diagnostic.

`overlay-stall-repro.sh` runs three arms. `stall` (writer goes quiet holding the
fifo open) wedges the graph with an empty stderr; `close` (writer closes) exits
cleanly; `resume` (writer pauses then resumes) recovers fully. So a freeze needs
**≥60s of continuous writer silence** to trip the detector — a short hiccup at an
item boundary is invisible.

`two-clock-capture.sh` takes an ETV-next channel id, not the station folder
number (`032-action` is channel 10 — check `/channels.m3u`). It watches the HLS
segment index and, on a gap, records both clocks plus each process's kernel wait
channel, then prints a verdict. `--self-test` exercises the verdict logic with no
host and no freeze.

**`pipe_write` on its own is not evidence.** One 1280x720 rgba frame is 3.5MB
against a 64KB pipe buffer, so a healthy overlay sits inside `write_all` nearly
all the time — measured healthy `phase_age_ms` is 16-241ms. The discriminator is
`frames_written` standing still across two heartbeat samples, which is why the
capture samples twice before ruling.

The overlay half is `crates/etv-overlay/src/phase_watchdog.rs`: the frame loop
marks its phase, a watchdog thread writes `overlay.heartbeat` beside the fifo
once a second and logs `overlay.phase_stall` on a 1/2/4/8s backoff. Cost in the
frame loop is two relaxed atomic stores per phase.
