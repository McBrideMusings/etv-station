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

The web UI's pages are plain HTML with an inline script that no cargo test loads.
Run the load check after any edit to `crates/etv-station/src/web_ui/shell.html`:

```bash
node tools/ui-page-check.js crates/etv-station/src/web_ui/shell.html
# expect: dashboard page script loads without error
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

The dashboard's Guide tab reads ETV-next's `/channels.m3u` and `/xmltv.xml` in the browser
(base URL: `ETV_NEXT_URL` from `GET /api/health`'s `etv_next_url`, else the page's host on
port 8409). Clicking a programme asks `GET /api/airings/{number}.{start unix seconds}/audit`
for that airing's stored audit record; the HTTP-seam test is
`web_ui::airing::tests::the_endpoint_returns_the_stored_record_for_a_known_airing_and_404s_an_unknown_one`.
To see the Guide against a running stack: start `./tools/dev-run.sh` (ETV-next on 8409), start
`./tools/station-ui.sh`, open `http://127.0.0.1:4747/#/guide`. Stop ETV-next and reload to see
the error-upstream state, which names the URLs tried.

Clicking a channel name in the Guide plays `{base}/channel/{n}.m3u8` in a player in the rail: native
HLS where `canPlayType('application/vnd.apple.mpegurl')` answers (Safari, recent Chrome), otherwise
hls.js from `GET /assets/hls.min.js` (embedded in the binary, test
`web_ui::tests::the_embedded_hls_script_is_served_from_the_dashboard`). A manifest 404 shows
"Cannot play <url> / Network error: HTTP 404 …" under the video. With no stack running, a stand-in
server answering `/api/health`, `/channels.m3u`, `/xmltv.xml`, `/assets/hls.min.js` and
`/channel/{n}.m3u8` (an `ffmpeg -f hls` testsrc) exercises the page; to force the hls.js path in a
browser with native HLS, override `HTMLMediaElement.prototype.canPlayType` to return `''` first.

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

For a quick local check that needs no media on disk and no Plex reachable at all, use
`examples/station-test.yaml` instead — one lavfi channel, no `${…}` expansions, so nothing
needs sourcing. Run it from the repo root with Plex's env vars explicitly unset (see
blocker 3 below for why that matters):

```bash
env -u PLEX_URL -u PLEX_TOKEN -u TAUTULLI_URL -u TAUTULLI_API_KEY \
  ./target/debug/etv-station --config examples/station-test.yaml
```

Reaches `event="playout.first_emit"` in about 15 seconds.

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

## Verifying the ledger has no holes (`--backfill-history`)

Every airing in a channel's playout JSON must have a row in `playout/history.db`. When it
does not, that channel reads as having no past: its `advance = "resume"` pools restart every
series at its top, and the adjacency seam stops seeing what just aired. The symptom on
screen is a channel replaying the same few titles and the same first three episodes of every
show, forever.

The dry run is the check — it reports what is missing and writes nothing:

```bash
./target/debug/etv-station --config examples/station-test.yaml --backfill-history --dry-run
# healthy: "N channels, M airings on disk, would insert 0"
# damaged: one line per channel naming the count and the span it would fill
```

Drop `--dry-run` to repair. It inserts only what is missing, so a second run must report
`inserted 0` — that idempotence is the thing to assert, not the first run's count.

Against prod it is `admin backfill-history [--dry-run]`, which runs it inside the container.
It recovers only what is still on disk; `retention_days` prunes elapsed chunks, so a hole
older than that window is unrecoverable and the count will not add up to the damage.

The hole this repairs came from the coverage-heal path truncating history from the instant
its wipe started while regeneration resumed at the surviving frontier
(`daemon.rs`, `coverage.heal`). That is fixed, and
`a_coverage_heal_keeps_rows_for_the_airings_it_leaves_on_disk` in `daemon.rs` is the
regression test — it punches a hole *inside* a chunk (not by deleting the file, which makes
`regen_from` and the frontier coincide and loses nothing) and asserts no airing on disk ends
up without a row.

## Verifying a config edit reaches the screen (`resume.refresh`)

Editing a channel's config and redeploying it should change what is *about to*
play, not just what plays a day from now. On the next start or SIGHUP the daemon
compares `ResumeMap::config_id` — a hash of the channel's config and resolved
overlay — against the channel as it now stands, and on a mismatch throws the
schedule away from the next item boundary forward.

```bash
admin deploy files && admin reload
# expect, per edited channel:
#   event="resume.refresh" reason="config" from=… truncated_from=… removed=…
# and for every channel you did NOT edit: no refresh event at all
```

`from` must be at or after the finish of the item currently airing — the cut
never lands inside what is on screen. Unedited channels must stay silent: the
hash excludes the catalog precisely so a Plex ingest does not rewrite a
taste-scored channel's future.

**This recipe only proves a content edit to an already-serving channel reaches
the screen.** It does not prove a channel starting to load (or stopping) is
now visible in the *lineup* — check that separately, always, after any
`admin deploy files` that touches `channel.yaml` or a plugin a channel's
`channel.yaml` references:

```bash
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "cat /mnt/user/appdata/etv-station/etv-next/lineup.json" | grep -c '"number"'
# expect: the count of channel directories under deploy/appdata/channels/
```

The daemon re-renders `lineup.json` on every `SIGHUP`-triggered generation
(`rerender_etv_next` in `daemon.rs`), so `admin reload` now covers this too —
but reading the daemon's own logs (`chunk.write`, `resume.load`,
`channel.load_failed`'s *absence*) is not the same check and does not catch a
regression in that render path. The lineup file itself, or the actual channel
count on `/channels.m3u`, is the only thing that proves what ETV-next is
serving. This is what bit prod on 2026-09-27: 53 of 62 channels silently
dropped from the lineup by a stale `channel.yaml` overlay schema after a code
deploy, invisible in the daemon's own (healthy-looking) logs for three days,
and even after a first-pass fix, still invisible because a check of the
daemon's internal state was mistaken for a check of what was actually being
served.

To force one without a config change:

```bash
admin refresh channel 002-for-pierce  # by directory name — prefer this
admin refresh channel all             # every channel
# expect: reason="forced" within one roll_interval, not instantly
```

A bare number resolves by lineup position, not the declared `number:`
(etv-station-421): with 62 channels, `admin refresh channel 85` fails with "no
channel number 85". Use the directory name until that is fixed.

The CLI writes a `.refresh-request` marker in the channel's playout folder and
the daemon consumes it on its next tick — it does not rewrite playout itself,
because the daemon owns those files. A marker that is still there a minute later
means the daemon is not ticking.

## Verifying a seed reset against a live daemon (#142)

`--refresh-channel <ch> --seed N` rewrites `seed:` in the channel's YAML, marks the
channel, and SIGHUPs the daemon through `<output_base>/.etv-station.pid`. Boot the
no-Plex config from a scratch copy (absolute `output_base`, absolute overlay path), wait
for `chunk.write`, then:

```bash
./target/debug/etv-station --config <scratch>/station.yaml --refresh-channel test --seed 777
grep -E 'signal.reload|resume.refresh' <daemon log>
grep '^seed' <scratch>/channels/lavfi-test.yaml
```

Look for: `sent SIGHUP to the daemon (pid …)`, `event="signal.reload"` then
`event="resume.refresh"`, `seed: 777` in the YAML, and the pid file gone after the daemon
stops. Observed 2026-10-18: 480 `chunk.write` lines before, reload and refresh 52ms apart.
`cargo test -p etv-station --lib a_forced_refresh_rebuilds_the_same_schedule` is the
same-seed identity proof. On prod the YAML edit lands inside the container only; copy the
seed into `deploy/appdata/channels/<ch>/channel.yaml` or the next `admin deploy files`
reverts it.

## Verifying a partial wipe re-airs nothing (#378)

Every wipe — a refresh, a coverage heal, the window sweep — cuts at an item
boundary that can sit inside a generation, and rewinds the `.resume` state to
where the surviving schedule ends. If it rewinds any further, the regenerated span
repeats items the kept schedule already aired: in the guide, the same series
twice in a row with the second at a lower episode.

The regression tests are
`a_partial_wipe_rewinds_the_list_position_only_past_what_it_spared` (`daemon.rs`,
a real `regen_floor` wipe on a flat list),
`a_partial_wipe_on_a_pattern_channel_repeats_no_rotation_step` (`daemon.rs`, the
same wipe on a pattern channel whose missing files air as error cards), and
`a_partial_wipe_rewinds_pool_rotation_only_past_what_it_removed` (`resolve.rs`,
the per-item rotation state on its own).

At the daemon, the committed lavfi channel loops three items (`testsrc`,
`smptebars`, `mandelbrot`) and each generation lays all three, so a forced refresh
usually cuts mid-generation. Boot it, then force a few refreshes a minute apart:

```bash
mkdir -p tmp && rm -rf examples/output/test
env -u PLEX_URL -u PLEX_TOKEN -u TAUTULLI_URL -u TAUTULLI_API_KEY \
  ./target/debug/etv-station --config examples/station-test.yaml
# in another shell, three times, ~70s apart (the marker is read on the next tick):
./target/debug/etv-station --config examples/station-test.yaml --refresh-channel test
```

Then stop the daemon and walk the airings on disk in order:

```bash
python3 - examples/output/test <<'EOF'
import glob, json, os, sys
order = ["testsrc", "smptebars", "mandelbrot"]
seen = {}
for p in glob.glob(os.path.join(sys.argv[1], "*.json")):
    for i in json.load(open(p)).get("items", []):
        seen[(i["start"], i["id"])] = i
a = sorted(seen.values(), key=lambda i: i["start"])
idx = lambda i: next(n for n, s in enumerate(order) if s in i["id"])
print(sum(idx(y) != (idx(x) + 1) % 3 for x, y in zip(a, a[1:])), "out-of-order steps")
EOF
```

Look for: `0 out-of-order steps`, and one `event="resume.refresh"` line per forced
refresh in the daemon log. A straddling item appears in both neighbouring chunk
files; the `(start, id)` key counts it once. Measured 2026-09-25: 1444 airings, 0
steps with per-item checkpoints; 3 steps (one per refresh) with them disabled.

## Verifying a restart's resume decision (skip vs rewind)

On startup, `daemon.rs:2181`/`:2211` decides, per channel, whether a written-but-unaired
future survives a restart or gets thrown away and regenerated:

- `event="resume.skip"` — the catalog, config, and overlay fingerprint matched what was
  true when the unaired window was written, so it is kept as-is.
- `event="resume.rewind" from=… removed=… airings=…` — it did not match, so the daemon
  rewound to the earliest unaired generation, wiped the playout JSON from there on, and is
  regenerating it from the current config.

Recipe, using the no-media/no-Plex config:

```bash
env -u PLEX_URL -u PLEX_TOKEN -u TAUTULLI_URL -u TAUTULLI_API_KEY \
  ./target/debug/etv-station --config examples/station-test.yaml
# wait for: event="chunk.write" channel=test
# Ctrl-C (SIGINT)
env -u PLEX_URL -u PLEX_TOKEN -u TAUTULLI_URL -u TAUTULLI_API_KEY \
  ./target/debug/etv-station --config examples/station-test.yaml
# read the startup line: resume.skip or resume.rewind
```

Nothing changed between the two boots → expect `resume.skip`. To force the rewind arm,
edit `examples/channels/lavfi-test.yaml` (e.g. change `program.title`) between the two
boots — and revert that edit before committing anything, since this file is tracked.

**Neither line appearing is a legitimate outcome, not a failure.** Both are gated on
`has_unaired` (daemon.rs ~2168): a channel with no checkpoints in the future — nothing
written yet, or everything already aired — logs neither line, because there is nothing to
decide between.

Artifacts live under `examples/output/test/`: `.resume`, `.history`, and
`<start>_<end>.json` files. `examples/output/` is gitignored, so nothing here needs
cleanup before a commit.

## Verifying the seed cascade and seed/temperature rotation

A channel with no `seed:` inherits the station `seed:`, salted with the
channel's declared `name:` (#324, #414); nothing set anywhere is a new seed
each local day. `seed:` and `temperature:` rotate on a cadence
(etv-station-tflc, `docs/schema.md` § Seed and temperature). The unit proof:

```bash
cargo test -p etv-station --test station_seed_cascade        # load() → derived, salted, stable seeds
cargo test -p etv-station --lib variety::                     # periods, cycles, default, rollover instant
cargo test -p etv-station --lib pattern::tests::a_rotating    # one list per period, then a new one
cargo test -p etv-station --lib pattern::tests::a_rollover    # lands on the first visit after the boundary
cargo test -p etv-station --test taste_cosine_scorer temperature
```

**The surface is the web UI's review**, which carries per airing a `ranking`
object: the `seed`/`temperature` the visit drew with, `seed_period`, and
`list_position` of `list_length` in that period's ranked list. Copy the real
appdata to scratch (never edit `deploy/appdata` from a worktree), set the
channel rotating, and run the web UI on it:

```bash
SIM=$(~/.claude/tools/repo-slug --path)/sim && mkdir -p "$SIM"
rsync -a /Users/pierce/Projects/etv-station/deploy/appdata/ "$SIM/appdata/"
cp examples/plugins/taste-cosine.rhai "$SIM/appdata/plugins/"
# in $SIM/appdata/channels/034-syfy/channel.yaml: seed: { rotate: 2d },
# temperature: 0.5, films pools advance: resume, shows pools temperature: 0
set -a; . /Users/pierce/Projects/etv-station/.env; set +a
export PLEXDB_SNAPSHOT_PATH=/Users/pierce/Projects/etv-station/tmp/claude/scratchpad/station-cache/plexdb.snapshot.db
export ETV_STATION_CATALOG=/Users/pierce/Projects/etv-station/tmp/claude/scratchpad/station-cache/catalog.db
cargo run --release --bin etv-station -- \
  --config "$SIM/appdata/station.yaml" --serve-ui --ui-port <PORT>
ETV_UI_PORT=<PORT> tools/ui-get.sh '/api/channels/034-syfy/review?project=14d' > a.json   # twice: a.json, b.json
```

Look for, per films pool: `list_position` 1, 2, … inside one `seed_period`
and back near 1 at the next, a different title opening every period, and
`a.json`'s `airings` equal to `b.json`'s. Observed 2026-10-15 on 034 (2d, T 0.5):
7/7 films pools opened every one of their 7-8 periods on a different film,
positions climbed inside every period, and both runs' 361 airings matched. A
position that skips (1 then 3) is the pool's `no_repeat_within` passing over a
film aired in an earlier period, not a broken walk.

**Prod's 034 `channel.yaml` overlay block does not parse on this code** (as of
2026-10-15: "`overlay:` must be `clear`, a `{file: <path>}` reference…"), so a
web UI run of the real file reports a `load_error`. Delete the `overlay:`
block from the scratch copy; the schedule does not read it.

At the daemon, `event="variety.ranked" pool=… list=…` logs once per pool per
period and `event="variety.rollover"` when a period turns over mid-generation.

## Verifying a folder rename keeps a channel's history (#414)

A channel's identity is its required `name:`, never its folder. That string is
the `channel` column on every `airings` row in `history.db`, the key series
resume cursors are read by, the seed salt and the output folder leaf — so a
folder rename must change none of them. Config-level proof:

```bash
cargo test -p etv-station --test station_seed_cascade renaming_a_channel_folder
cargo test -p etv-station --lib config::load::tests::a_channel_with_no_name
cargo test -p etv-station --lib config::load::tests::two_channels_sharing_a_name
```

At the daemon: copy `examples/station-test.yaml` into a scratch dir with
`channels: [channels/*/channel.yaml]`, an absolute `output_base`, and
`lavfi-test.yaml` (overlay path made absolute) at `channels/001-test/channel.yaml`.
Boot it with the `env -u` invocation above until `event="chunk.write"`, stop it,
`mv channels/001-test channels/009-test-renamed`, boot again, then:

```bash
sqlite3 <scratch>/out/history.db "select channel, count(*) from airings group by channel"
```

Look for: one row, `test`, before and after (2026-10-08: `test|1440`, then
`test|1443`), the second boot's `event="channel.load" channel=test
config=…/009-test-renamed/channel.yaml` followed by `resume.load` and a
`chunk.write` continuing the first boot's window, and `out/` holding only `test/`.
Delete the `name:` line and boot again: `event="channel.load_failed"` with
``missing field `name` ``.

Against prod, every `deploy/appdata/channels/*/channel.yaml` must declare a
`name:` equal to the identity its history is already filed under (its folder
name at the time #414 landed). Audit:

```bash
for f in deploy/appdata/channels/*/channel.yaml; do d=$(basename "$(dirname "$f")"); n=$(sed -n 's/^name: //p' "$f"); [ "$n" = "$d" ] || echo "DIFF $d -> ${n:-<none>}"; done
```

Empty output is a pass until a folder is deliberately renamed; after that, a
`DIFF` line is expected and correct — the `name:` is what must not change.

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

`event="reconcile.swept"` at INFO fires on every sweep, including one where
nothing changed — that's the healthy steady state where playout already agrees
with the catalog. A file's mtime is the ground truth: a no-op sweep must not
touch it. The absence of this line means the sweep did not run. Delete the media
file instead of renaming it and the same tick logs `event="reconcile.item_carded"`
and the slot becomes a black card that still carries the title in the guide.

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
curl -s "$ETV_NEXT_URL/xmltv.xml" \
  | xmllint --xpath 'count(//programme[@channel="ersatztv.6"][episode-num or sub-title])' -
```

Look for: `0`, no programme on that channel with an `episode-num` or `sub-title`.
Anything above zero fails. (`ETV_NEXT_URL` is the Guide's base URL, the one
`GET /api/health` reports as `etv_next_url`; the channel id is the one in that
XMLTV's `<channel id>`.)

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

- Scorer-supplied data reaches an overlay: run `cargo test -p etv-overlay --lib metadata` and expect `script_reads_a_key_out_of_metadata`, `missing_metadata_is_an_empty_map` and `nested_metadata_is_an_array_of_maps` to pass. These feed `ProgramContext.metadata` and read it back through the Rhai scope, which is the only surface that tells scorer data from a script default (`admin overlay-watch` renders against a fixture with no plugin behind it).

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

## Verifying `bumper-preview` and `bumper-watch`

No station, catalog or Plex needed. Resolve a bumper on a gap and read the items:

```sh
cargo build -q -p etv-station -p etv-overlay
./target/debug/etv-station bumper-preview examples/plugins/text-card-bumper.rhai --gap 45s | jq -c '.[] | [.id, .start, .finish, .overlay]'
./target/debug/etv-station bumper-preview examples/plugins/text-card-bumper.rhai --gap 45s --otio | jq '.tracks.children[0].children | length'
```

Expect two items for the text-card bumper (a 2 s black, then a looped black card whose `overlay.script` is an absolute path), the first `start` to the last `finish` spanning exactly 45 s, and `2` from the OTIO count. A script that throws exits 1 with its message on stderr and nothing on stdout (the generator would air black; the preview must not).

The trailer bumper (`examples/plugins/trailer-bumper.rhai`) is checked against the real Plex, using a catalog that holds the film (`ETV_STATION_CATALOG`) and `PLEX_URL` / `PLEX_TOKEN` in the environment:

```sh
./target/debug/etv-station --config examples/station.yaml bumper-preview examples/plugins/trailer-bumper.rhai --gap 2m --next <entry_id> --script-config '{"mode":"next"}' | jq -c '.[] | [.source.source_type, .source.uri, .source.out_point_ms]'
./target/debug/etv-station --config examples/station.yaml bumper-preview examples/plugins/trailer-bumper.rhai --gap 8m --next <id> --upcoming <id2> --upcoming <id3> --script-config '{"mode":"upcoming"}' | jq -c '.[] | [.source.source_type, .source.out_point_ms]'
```

Expect, for a film with a Plex trailer, an `http` item whose `uri` starts `{{PLEX_URL}}/` and ends `X-Plex-Token={{PLEX_TOKEN}}` (the real token never appears), `out_point_ms` equal to the trailer's length (Rush Hour: `88000`), then a black item to the end of the gap; `upcoming` gives one `http` item per distinct film, nearest first. A film with no Plex id or no trailer exits 1 with `build() returned an empty timeline`, which in the fill pass makes it draw the next entry. With `RUST_LOG=info` stderr carries `trailer: item=… found=… len_ms=…` and, per trailer, a `bumper.http_source` line with the host.

`bumper-watch` plays those items through `etv-overlay watch --item`, one process per item, into VLC. Prove it without VLC by standing in a capture script for it (`overlay-watch.sh` reads `$VLC`), then count the frames the card drew over:

```sh
VLC=<script that runs: cat > out.ts> ./tools/bumper-watch.sh examples/channels/lavfi-test.yaml <bumper.rhai> --gap 5s
# after ~25 s, SIGINT the pipeline, then:
ffmpeg -v error -i out.ts -vf "signalstats,metadata=print:key=lavfi.signalstats.YMAX:file=-" -an -f null - | grep YMAX | sort | uniq -c
```

Expect both a black count (`YMAX=16`) and a text-frame count (`YMAX=248` for white text). The `etv-overlay` log shows `overlay=None` for an item with `"overlay": "none"` and `overlay=Script { script: …card.rhai, vars: … }` for the card item. `admin overlay-watch <channel> --metadata '<json>'` is the same pipeline for a single looping background.

## Verifying the block card and pool labels

A pool's `label:`/`advisory:` ride on every item it draws as
`metadata.pool_label` / `metadata.pool_advisory`, and
`deploy/appdata/shared/block-card.rhai` draws them. Three layers, cheapest first:

```bash
cargo test -p etv-station --lib -- pattern::tests::a_labelled pattern::tests::an_unlabelled config::validate
cargo test -p etv-station --test sequencer_plugin a_labelled_pool
cargo test -p etv-overlay --test render_smoke block_card
```

The first two prove the station stamps both keys on pattern and sequencer
blocks (and refuses a blank label, a blank advisory, an advisory with no label).
The third drives the shipped script through the real engine: change card, no
card on an unchanged label, interval card, advisory row, no label no card.

To see the card, render a still with the metadata a daemon would have written.
The spec needs both graphics, the snipe's four layers and the card's four;
`docs/schema.md` "Pool label / advisory" has the YAML:

```bash
etv-overlay render-still --config spec.yaml --output card.png --time 1000 \
  --title "Dawn of the Dead" --item-elapsed 3 \
  --metadata '{"pool_label":"The Zombies Block","pool_advisory":"Contains graphic violence"}' \
  --prev-metadata '{"pool_label":"The Kaiju Block"}'
```

Drop `pool_advisory` for the no-advisory card. `--time 1000` keeps the interval
trigger away (it fires 150-156s into each 300s cycle), so the card on screen is
the change card. Composite the PNG over a test pattern with ffmpeg before
judging legibility; on its own it is transparent.

Live: a scratch station with `source_roots` pointing at four tiny `lavfi` files
(`Kaiju A/B`, `Zombie C/D`), two labelled pools and a pattern `kaiju x2, zombies
x2`, run with `env -u PLEX_URL -u PLEX_TOKEN ...` so no ingest blocks. Read the
chunk files: items from `zombies` carry both keys, `kaiju` items carry only
`pool_label`. Then run `etv-overlay pipe --fifo F --create-fifo
--playout-folder <output>/<channel>` against that folder and read the fifo with
ffmpeg (`-use_wallclock_as_timestamps 1 -copyts`, `crop=200:40:0:0,alphaextract,
signalstats`): the top-left alpha is non-zero for the card's first seconds from
the item that changes label and zero for an item that keeps it.

A config change that moves a pool onto `graphics:` is proved by loading the
whole deployed tree: copy `deploy/appdata/{channels,shared,station.yaml}` aside,
run the migration, and `etv-station --config <copy>/station.yaml --dump-overlay
<name>` for every channel `name:` (62 of 62 load).

## Three blockers you will hit, and how to get past them

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

**The workaround that gets the daemon up:** drop the sample channels. Use the committed
`examples/station-test.yaml` — it already registers only `channels/lavfi-test.yaml`, no
`samples/*.yaml` glob, so neither blocker above can fire:

```bash
env -u PLEX_URL -u PLEX_TOKEN -u TAUTULLI_URL -u TAUTULLI_API_KEY \
  ./target/debug/etv-station --config examples/station-test.yaml
```

That boots clean and rolls the one lavfi channel. Do not hand-author a throwaway sibling
config for this — `examples/station-test.yaml` is tracked precisely so nobody has to.

**3. A configured Plex blocks the channel loop from ever starting.** In `daemon.rs`, the
startup ingest (`catalog.ingest.fs` → `catalog.ingest.plex_start mode="full"`) runs to
completion before any channel task rolls. With `PLEX_URL`/`PLEX_TOKEN` set, that means a
real library read taking minutes, and the daemon never reaches the resume path most local
verification actually wants to watch. Unset them with `env -u` as shown above rather than
relying on `.env` not having them — `admin dev-station` sources `.env`, which may set them,
so use the raw binary invocation for this, not the admin task.

`examples/station-test.yaml`'s `catalog_refresh_secs: 86400` only helps *subsequent* boots
once a catalog already exists on disk (`plex_ingest_plan` takes `PlexIngestPlan::Skip` and
logs `event="catalog.ingest.plex_skipped"`) — the first boot with Plex configured still
pays the full ingest regardless of that setting. `env -u` is what actually avoids it.

## The HTTP surface is not the daemon

`admin.toml` lists `http://127.0.0.1:8409/channels.m3u`, `/xmltv.xml`, `/channel/1.m3u8`.
**The station daemon alone does not bind 8409** — I polled it for 150s while the daemon was
happily rolling channels and nothing ever listened. Those URLs are the container's, served
with ETV-next. For the HTTP surface locally use `admin dev` (daemon + ETV-next), not
`admin dev-station`.

## Verifying channel failure accounting (etv-station-323)

ETV-next counts a worker exit as a failure when a viewer's heartbeat is fresh
and the run either stalled (exit 75) or lasted under 180s. Three in a row and
`/channel/{n}.m3u8` answers 503 with `Retry-After: 30`. Every step is logged,
and `GET /health/channels.json` returns the state.

Locally, under `admin dev` against `examples/station-test.yaml`, keep a viewer
polling `/session/1/live.m3u8` every 2s and force the failures:

- **A real stall:** SIGSTOP every ffmpeg child of the channel-1 worker, once a
  second, until the worker exits. Stopping one ffmpeg once is not enough: the
  worker's lead-rebuild path kills and restarts it inside 60s.
- **Fast kills:** SIGKILL the worker a few seconds after it respawns. A worker
  older than 180s exits as healthy and clears the count. That is the design, not
  a bug in the check.

Pass, observed 2026-10-07:

```
channel 1 (Test Pattern Channel) failed with a viewer watching, 88s after spawn: exit status: 75: channel 1 terminated after ffmpeg stall; consecutive failures 2/3
channel 1 (Test Pattern Channel) failed with a viewer watching, 6s after spawn: signal: 9 (SIGKILL); consecutive failures 3/3; next spawn allowed in 30s
channel 1 (Test Pattern Channel) is now FAILED after 3 consecutive failures; /channel/1.m3u8 answers 503 until a spawn is allowed in 30s
channel 1 (Test Pattern Channel) is no longer failed: a run of 102s exited without failing under a viewer, clearing 3 consecutive failures
```

Then `curl -D - http://127.0.0.1:8409/channel/1.m3u8` answers `503` and
`retry-after: 30`, and `tools/ui-get.sh /api/health/channels` shows the channel
with `failed: true` and `retry_in_secs` counting down. Against prod: add `--prod`.

## Verifying hardware encoding (#258)

`tools/ui-get.sh /api/health/channels` (add `--prod` for the deployed station) —
read-only, touches no container. The Health tab shows the same answer.

**Codec name proves nothing here.** A software encode and a VAAPI encode both
produce H.264, so `ffprobe` on a segment reports `h264` either way, and ETV-next
logs a fallback to software only at DEBUG. The ffmpeg command line is the only
thing that separates them, which is why the check reads
`data/diag/ffmpeg-argv-ch<N>.log` and asserts, per channel's newest transcode:

- the expected encoder is present (`h264_vaapi` for `ETV_ACCEL=vaapi`)
- `libx264` is **absent** — its presence is the fallback fingerprint
- the configured render node (`/dev/dri/renderD128`) appears in the argv

The expected encoder comes from the station's own `ETV_ACCEL` and
`ETV_VAAPI_DEVICE`, the variables the channel configs are rendered from, so the
verdict asserts ffmpeg did what the station asked.

**It reads the ffmpeg probe's argv logs.** The probe ships in the image
(`Dockerfile`) and is wired by `ffmpeg.ffmpeg_path` in
`deploy/appdata/station.yaml`, so it is always on — there is no re-apply step and
no host-side copy. A channel with verdict `NONE` (no argv log) means no
channel has transcoded since the container last started, not that the probe is
missing: the log is only written when a transcode begins, and only a channel
someone is watching transcodes at all.

A `STALE` verdict means the same thing in miniature — that channel's newest argv
is older than 3 days, so it proves the GPU worked then, not now. Tune
in to the channel and re-run for a current answer.

Locally, start `tools/station-ui.sh` with `ETV_DIAG_DIR=<diag-dir>`, `ETV_ACCEL` and
`ETV_VAAPI_DEVICE` set, then read `/api/health/channels`. A verdict is logged as
`ui.health_verdict` with the argv log line it rests on.

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
the newest — a sliver is rare, and the Health tab's newest-block-only verdict
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

## Verifying audio and subtitle track selection (#261)

```bash
admin verify-tracks        # or ./tools/verify-tracks.sh
```

It builds one Matroska file with a default-flagged commentary track listed first,
a main track, and an English text subtitle; boots the daemon against it with no
Plex; and asserts the first emitted item plays stream 2 (the main track) with
subtitle stream 3, that `channel1.json` carries `subtitle.default_on`, and that
`event="tracks.assigned"` was logged. Look for five `PASS` lines. It fails if the
station config's commentary exclusion stops working, because the commentary is
the one flagged default.

Picture subtitles cannot be made with ffmpeg, so the burn rule is checked by
`cargo test -p etv-station --lib tracks::`. To see it on real media, clip a disc
rip and run the daemon against it:

```bash
ffmpeg -ss 00:30:00 -i "<rip>.mkv" -t 40 -map 0:v:0 -map 0:a -map 0:s \
  -vf scale=640:360 -c:v libx264 -preset ultrafast -c:a aac -ac 2 -c:s copy clip.mkv
```

Give a block `tracks: { subtitle: { prefer: [ { image: true, lang: "^en", when: {
audio_lang_not: "^en" } } ] } }` and look for `event="tracks.burned"` and a
`tracks.subtitle.stream_index` in the playout JSON. Without the rule, the same
file's item carries no `subtitle` key.

To see what a player sees, start `etv-station` (with `env -u PLEX_URL -u
PLEX_TOKEN`) and `ersatztv` from `--render-etv-next` output, then:

```bash
curl http://127.0.0.1:8409/channel/1.m3u8        # expect #EXT-X-MEDIA:TYPE=SUBTITLES ... DEFAULT=NO
curl http://127.0.0.1:8409/session/1/live_sub.m3u8   # lists <run>/liveNNNNNN.vtt
curl http://127.0.0.1:8409/session/1/<run>/live000025.vtt   # cue lines under WEBVTT
```

Fetch a `.vtt` by the path `live_sub.m3u8` lists, including its `<run>/` prefix; the
bare file name returns an empty body. `./tools/dev-run.sh` sources `.env` and would
contact the real Plex, so run the two binaries by hand for this check.

## Verifying schedules (`rule.schedules`, ADR 0004)

A channel's `rule.schedules` decide, per generation, whether a `rule.slots`
block or the any-time `rule.blocks` airs, and cut the generation where that
changes. Every such generation logs one line:

```
INFO … event="calendar.select" window_start=… tz="America/Chicago" slot="late" schedule="nightly" cut=Some(…)
```

`slot=""` means the any-time blocks. To watch it with no media and no Plex,
copy `examples/station-test.yaml` into a scratch dir with an absolute
`output_base`, and give it one lavfi channel whose `schedules` place a slot a
few minutes ahead of the current local time (one lavfi item per list is
enough). Boot it with the `env -u` invocation above for ~40s, then:

```bash
grep calendar.select <log> | grep -oE 'window_start=[^ ]+ [^ ]+ [^ ]+ tz="[^"]+" slot="[^"]*" schedule="[^"]*" cut=[^ ]+( [^ ]+ [^ ]+\))?' | awk '{k=$5" "$6} k!=p{print; p=k}'
```

Look for one line per block change, each `window_start` a few seconds after
the previous line's `cut` (the item airing at the boundary finished first),
and the slot titles in the emitted JSON exactly inside those spans. Observed
2026-10-14: a `01:30..02:00` slot took over at 06:30:22 UTC and handed back at
07:00:22; a dated `03..03:15` slot ran 08:00:22-08:15:22.

`--check-config` is the check for the load errors: an undeclared slot, an
unplaced slot, two overlapping ranges in one schedule, `"02-30"`, a span
mixing `MM-DD` and `YYYY-MM-DD`, and `cycles:` on any block of a channel with
schedules each print `channel.load_failed` naming the schedule, slot or block.

For a real channel, run the web UI's Review (the `programming` skill) and
count airings per local day and pool. Review always starts today and caps at
50 days, so a date further out needs a scratch copy of the channel with its
`dates:` moved into range.

## Verifying the align/fill pass

`examples/station-align.yaml` runs one lavfi channel (`examples/channels/align-test.yaml`: programmes of 7, 11 and 17 minutes, `align: 30m`, `examples/plugins/bumpers/test_card.rhai`).

```sh
env -u PLEX_URL -u PLEX_TOKEN -u TAUTULLI_URL -u TAUTULLI_API_KEY \
  ./target/debug/etv-station --config examples/station-align.yaml
```

Stop it after ~30s. Look for: `event="bumper.fill"` lines (channel, script, gap_ms, segments, fallback), zero WARN/ERROR, and in `examples/output/align-test/*.json` every programme that follows a `bumper:` item starting on :00 or :30 with no gap between items. `sqlite3 examples/output/history.db "select kind,count(*) from airings group by kind"` shows only `programme`. Observed 2026-10-18: 48 programmes, 49 bumpers, 0 off-grid.

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

## Verifying the web UI (`etv-station --serve-ui`)

The container starts it from `docker/entrypoint.sh` beside the daemon and ETV-next,
on `0.0.0.0:4747` at `nice -n 19`. Its state query is `GET /api/health`: station,
catalog, plugins dir, bind, port, tunable-channel count, revision; 503 when the
station config does not load. Every request logs `event="ui.request"` with status
and ms; a review logs `ui.review` with its generation count and ms.

Locally: `./tools/station-ui.sh` (`admin tools ui`), then
`tools/ui-get.sh /api/health`. In the image, with no media and no Plex,
copy `examples/` to a scratch `config/`, mount `crates/etv-overlay/fixtures` at
`/crates/etv-overlay/fixtures` (the lavfi channel's overlay names it by a `../../`
path), and run with `-e ETV_STATION_CONFIG=/config/station-test.yaml -p 14747:4747`.
Look for, observed 2026-10-18 on a native build: health 200 within 2s of start,
`<title>etv-station</title>` at `/`, `/channels.m3u` still 200 on
8409, and `docker exec <ctr> ps -eo pid,ni,args` showing the `--serve-ui` process
at NI 19 while the daemon and `ersatztv` sit at 0. After `docker kill -s TERM`, the
`--serve-ui` process is gone within 3s. `GET /api/channels/nope/pools` answers 404 "no
channel".

**The dashboard** is one page at `/`: tabs (Channels, Guide, Health) and, on
Channels, a lineup rail fed by `GET /api/lineup` (every lineup channel in
`station.yaml` order: `channel` key, `number`, `display_name`, `scored`, and
`error` for one that failed to load; logs `ui.lineup`). The view lives in the
hash and mirrors the API: `#/channels/{ch}/schedule|pattern?project=Nd` is answered by
`GET /api/channels/{ch}/review?project=Nd`; a key not in the lineup is a 404. Run
`tools/ui-get.sh /api/lineup` and `tools/ui-get.sh '/api/channels/<key>/review?project=3d'`,
then screenshot, light and dark: a channel's Schedule populated, its Pattern, the
pending state (a fresh channel while the worker is busy), a 404 key
(`#/channels/nope/schedule` shows "The review could not be built"), Guide and
Health (each "Not built yet"), and the page at 400px wide (the rail folds into a
picker). After restarting the server on a new
build, load `about:blank` before the dashboard URL: a navigation that changes only
the hash keeps the old page's script running, so every screenshot after it shows
the previous build.

**Channels › Pools** is `#/channels/{ch}/pools?draft=ID`, answered by
`GET /api/channels/{ch}/pools?draft=ID`. Drive it with `tools/ui-get.sh` (it takes `-X` and `-d`):

```bash
tools/ui-get.sh /api/channels/034-syfy/pools | jq '.pools|keys'        # non-scored channel: every pool, sources and config, no ranking
tools/ui-get.sh /api/channels/001-for-you/pools | jq '.pools.movies.ranking[:3]'
D=$(tools/ui-get.sh -X POST -d '{}' /api/channels/001-for-you/drafts | jq -r .draft)
tools/ui-get.sh -X POST -d '{"op":"add","pool":"movies","entry":{"keyword":"<kw>","weight":3}}' /api/channels/001-for-you/drafts/$D/edits | jq '.pools.movies.ranking[:3]'
tools/ui-get.sh "/api/channels/001-for-you/drafts/$D/yaml?pool=movies"   # the edited profile:
tools/ui-get.sh -X DELETE /api/channels/001-for-you/drafts/$D
```

Look for: the edited ranking differs from the deployed one and its items carry
`movement`; the YAML holds the added entry; the log file holds one `ui.draft.edit`
line per edit with the draft id, pool, op, entry and `top_ranks`; a request naming
the discarded draft answers 404. Screenshot, light and dark: no draft, a draft with
edits (draft bar, movement column), the pending state while a ranking runs (the
"Ranking…" line; a pool ranks in 10-22s on the real catalog), and an error-server
state (`#/channels/001-for-you/pools?draft=gone` first shows the 404 notice, then the
deployed pools). The Tautulli account choice lists users only when `TAUTULLI_URL`
and `TAUTULLI_API_KEY` are set.

A review answers from the channel's real airings with no simulation: `GET /api/channels/{ch}/review`
is 200 at once (observed 2026-10-19 on a fixture station: 12 ms), every airing `projected: false`,
`span.label` "real only". `?project=1d|3d|7d|14d` adds that many days simulated from the frontier
on a worker thread (`projection`): 202 with the real review and `pending: true` until it lands (the
page keeps the real schedule up beside a spinner and asks again every second), then 200 with
`projected: true` airings, `span.projected_until` = frontier + N days and `projection.key`. Repeat
the request: the key and airings are identical, and touching only `catalog.db`'s mtime leaves the
key alone; editing `channel.yaml` changes it and `projection.changed` reads `["config"]`.
Results persist in `ui-projections/` beside the catalog (they survive a deploy). Each finish logs
`ui.projection.done` with ms and `changed`; a served review logs `ui.review`. `/api/health` carries
`projections: {in_memory, queued, running}`. The Schedule's screenshots (real, projecting,
projected, projection-changed; light and dark) can be taken against a fixture station:
a scratch binary that writes a catalog, a `station.yaml` and one chunk file is enough, since
the real side needs only the chunk files.

## Verifying the channel-wide repeat rule (`rule.no_repeat_within`)

`cargo test -p etv-station --lib a_channel_rule` covers spacing across pools, the ledger-tail seam, and the forced fallback. At the surface, run the web UI's Review on a scratch channel whose pools overlap — three `select: random` pools over one expression — once without the key and once with `rule: { no_repeat_within: "7d" }`, then compare `first_repeat.days` and the smallest gap between two airings of the same `entry_id` in `airings`. Observed 2026-10-17 on 14 days of Animation films: 0.15 days without the rule, 7.0 days with it. At the daemon, `event="channel_repeat.applied"` logs `blocked`/`forced` per generation, and `constraints.unsatisfied scope=channel` means the rule could not hold.
