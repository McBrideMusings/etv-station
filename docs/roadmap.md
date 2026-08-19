# Roadmap

> Direction, not task tracking. Concrete work lives in [GitHub Issues](https://github.com/McBrideMusings/etv-station/issues).

## Now

**v1 — Continuous Loop Forever playout** ([milestone](https://github.com/McBrideMusings/etv-station/milestone/1))

Goal: at any moment, every configured channel has playout JSON files on disk whose `[start, finish)` window contains "now" and extends `window_days` into the future, with item metadata populated so ETV-next's XMLTV is correct. Acceptance per [PRD §Verification](/PRD#verification-v1-acceptance) — 7 days continuous, populated XMLTV, zero loader errors.

The 13 v1 issues group into four implicit phases:

- ✅ **Foundations** — config parsing, atomic writes, sample fixtures (#2, #3, #21).
- ✅ **Loop Forever happy path** — rule + chunk slicer + anchor + ffprobe cache + startup scan + roll loop (#1, #4, #5, #9, #10, #12). `./tools/dev-run.sh` now boots station + ETV-next together and serves HLS segments end-to-end.
- **Operational** — reload, retention sweep, structured logging, container ([#6](https://github.com/McBrideMusings/etv-station/issues/6), [#7](https://github.com/McBrideMusings/etv-station/issues/7), [#11](https://github.com/McBrideMusings/etv-station/issues/11), [#13](https://github.com/McBrideMusings/etv-station/issues/13)).
- **Acceptance** — the 7-day soak run against a live ETV-next instance ([#20](https://github.com/McBrideMusings/etv-station/issues/20)).

**Deployed 2026-08-06.** The stack now runs on the Unraid host alongside the
legacy ErsatzTV (separate port, separate appdata, nothing shared), carrying 58
channels ported from that ErsatzTV's schedules — the working set for judging
whether ErsatzTV can be retired. Three ErsatzTV channels have no equivalent yet:
Seasonal, which is defined by a Plex label ([#136](https://github.com/McBrideMusings/etv-station/issues/136)),
plus MTV and DJ, which come from ErsatzTV-local libraries with no Plex source.
The port also surfaced [#135](https://github.com/McBrideMusings/etv-station/issues/135)
(`release_date` never populated) and [#137](https://github.com/McBrideMusings/etv-station/issues/137)
(one Plex film can become two catalog entries).

**The catalog stopped being a startup snapshot (2026-08-16).** Ingest ran once
before the first generation and never again, so the running station's view of
the library was frozen at boot. A batch of Radarr renames on the host proved what
that costs: playout written before the rename still named the old files, ffmpeg
could not open them, and channels 2 and 21 aired black and silence for whole
slots while Plex had reported the correct path all along. Three changes, together:
ingest now re-runs every `catalog_refresh_secs` inside the daemon loop; a
reconciliation sweep on the same tick patches already-written playout JSON in
place; and an entry the library loses is marked missing rather than deleted
([ADR 0006](/adr/0006-catalog-entries-are-soft-deleted)), so `entry_id` stays the
durable join key the ledger and the coming enrichment graph need.

## Next — three sequential phases of v2+ scope expansion

The v2+ direction extends `etv-station` from a hand-authored Loop Forever generator into a composable, catalog-aware playout system with overlay graphics. See [PRD §Scope evolution beyond v1](/PRD#scope-evolution-beyond-v1) for the framing and rationale.

Each phase is a milestone with a small, focused set of issues. Phases run sequentially because each de-risks the next.

### ✅ Phase A — Query language evaluation

**Shipped.** CEL (`cel` crate v0.13) validated against all 6 roadmap cases. Key findings:

- CEL handles the real-world queries cleanly. `title.startsWith(...)`, `season_in(lo, hi)`, `collections.exists(...)`, `icontains(...)` all expressed naturally in 1-2 lines.
- Plex episode metadata lacks genre tags and per-episode Collection — both require show-level enrichment at ingest time (implemented).
- Plex `type` field ("movie"/"episode") is too coarse for special libraries; type is now derived from Plex section name or FS directory name.
- `source`/`type` are orthogonal: source = catalog (plex, fs), type = semantic kind (episode, movie, concert, power_hour, music_video, bumper, …).

Deliverable: `crates/etv-query-test` — interactive CEL query harness with Plex + FS catalogs, path-key dedup, 1h disk cache, and `./tools/query.sh`.

### ✅ Phase B — Graphics rendering

**Shipped.** Per-channel `etv-overlay` subprocess renders Vello frames to a fifo etv-next composites on. Rhai scripts read the station-emitted chunked playout JSON to template lower-thirds with the current/next item's title and gate visibility on `item_elapsed` / `item_remaining`. Per-layer overrides (visibility, opacity, content, corner, and — new — `offset_x`/`offset_y`, a pixel offset added on top of `corner`/`margin` for a layer that animates its position, not just its visibility) compose with global `visible`/`opacity`. Sample scripts in `crates/etv-overlay/fixtures/scripts/`: `now_playing.rhai`, `up_next.rhai`, `pulse_watermark.rhai`, `corner_rotate.rhai`, `now_and_next.rhai`, `title_chyron.rhai` (slides the current item's title out from behind a corner logo on a configurable interval — live on the Pierce channels).

Isolated overlay preview: `admin watch` (dev/prod → channel → live/overlay) or `admin overlay-watch <channel>` renders a channel's real overlay over a looping background fixture and streams it into VLC, hot-reloading on save — no station, ETV-next, or Plex needed. `--time-scale` compresses a multi-minute animation cycle for fast iteration.

Out of scope until Phase C: scripted `size`/`color`. Lottie / `velato` tracked separately (#50).

### Phase C — Schema overhaul

With the query language picked and graphics rendering working, redesign the channel/block/entries schema and integrate everything:

- New TOML (or YAML) schema with blocks, channels, `[[entries]]`, includes, modes (`all` / `count`), filters, channel-seeded random order.
- Pools + pattern interleave ([#72](https://github.com/McBrideMusings/etv-station/issues/72), shipped) — "1 movie, then 3 episodes, repeat" across independently-progressing series, with a `.resume` sidecar carrying progression across window seams. Ships the resume-map half of the generation model.
- Generation model ([#70](https://github.com/McBrideMusings/etv-station/issues/70), shipped) — the play-history ledger: one `.history` line per scheduled airing, with the per-series resume cursor as a projection of it rather than a second store.
- Adjacency constraints (#73) — `[constraints]` with both the identity rule (`no_repeat_within = N`) and the property rule (`separate_by` + `separate_min_gap`, e.g. no two nearby films sharing a performer), enforced over the whole channel list and across the generation seam via the ledger. On a pattern block the table moves to the pool ([#115](https://github.com/McBrideMusings/etv-station/issues/115), shipped): the rule is enforced inside the pool — once over its resolved list, then again on every draw — so a repair can no longer swap items between the pattern's slots, and the gap counts that pool's draws.
- One emission model. The `LoopForever` rule and its `.anchor` sidecar are gone: every channel materializes forward, and a channel whose list never changes loops by repeating that list. Pool `wrap = "drop"` and the "channel exhausted" state went with them — television does not stop when it reaches the end of its library.
- Plex catalog ingester + local-FS catalog ingester (bumpers / commercials / errata).
- Runtime query resolution with snapshot-at-boot and configurable refresh interval.
- Graphics overlay cascade: station default → channel override → block override, shipped ([#48](https://github.com/McBrideMusings/etv-station/issues/48), [#304](https://github.com/McBrideMusings/etv-station/issues/304)); the per-program/item tier is deferred until a channel needs it (no deployed channel uses `Entry::Item`).
- Migration script from current `[rule] type = "loop_forever"` configs.

### Phase D — Plugin boundary and catalog-aware scheduling

**[Milestone](https://github.com/McBrideMusings/etv-station/milestone/6).** The 58 ported channels split cleanly into three groups: ones the current schema already builds, ones needing markup the schema does not have yet, and ones needing a metadata graph that does not live in this repo. Phase D builds the second group and draws the boundary the third reaches through.

**The plugin contract splits into two declared hooks — both now shipped.** ADR 0002 already makes a plugin replace a pool's `expr`, which is the candidate-set hook; what was missing is a plugin saying so. A plugin declares which hooks it implements via a `hooks()` function read at config load, and the station refuses a channel whose config and script disagree ([#159](https://github.com/McBrideMusings/etv-station/issues/159), **shipped**). It also declares the host capabilities it needs — catalog read, watch history, or a named external datastore — and the channel config grants exactly that set, checked both ways, so nothing is ambient and core never links an external store no channel asked for ([#167](https://github.com/McBrideMusings/etv-station/issues/167), **shipped**). The second hook, `sequencer`, takes a block's resolved pools plus the generation window and emits the block's final timeline in place of the pattern walk ([#169](https://github.com/McBrideMusings/etv-station/issues/169), **shipped**) — which is what unblocks dayparting for the network mirrors. Reproducibility is checkable rather than assumed: `etv-station --check-determinism <channel>` generates a channel twice from identical inputs and diffs the schedules, naming the first differing position and both entry ids, and it covers both hooks because it diffs the resolved timeline rather than any one hook's call ([#168](https://github.com/McBrideMusings/etv-station/issues/168), **shipped**). A plugin may return entries carrying an optional metadata blob and a per-entry take override rather than bare ids — the metadata rides `PlayoutItem::metadata`, a carrier added upstream in etv-next for exactly this ([#166](https://github.com/McBrideMusings/etv-station/issues/166), **shipped** for both `pattern:` and `sequencer:` blocks — [#201](https://github.com/McBrideMusings/etv-station/issues/201), **shipped**). The take override reaches the pattern draw, which now reads it in place of the step's own `take` for the series it names ([#173](https://github.com/McBrideMusings/etv-station/issues/173), **shipped**) — under `rotate: "visit"` only, and with no sequencer-side equivalent, since `arrange()` already decides its own order.

Most channels need exactly one of the two hooks — a taste model needs a custom candidate set and standard arrangement; a network mirror needs the reverse. Keeping them separate is what stops the plugin surface from ballooning.

**Dayparting ([#14](https://github.com/McBrideMusings/etv-station/issues/14), shipped).** A network-mirror channel is one block whose pools are its dayparts plus a default pool, all naming the reusable `examples/plugins/daypart-sequencer.rhai` — no schema field, per [ADR 0004](/adr/0004-calendar-and-clock-sit-at-different-seams). Each daypart pool declares its hour range and weekdays in its own `config:` (ADR 0002's existing generic carrier, exposed to the sequencer as `ctx.pool_config`); a new `local_time()` global function resolves any instant against the station's configured `tz`. Boundaries drift rather than truncating an item or leaving a gap: the script prefers whichever item fits the time left before the next boundary, and when nothing fits, the next item plays anyway and the following daypart starts late. Built end to end against the real library as `examples/samples/adult-swim.yaml` — its Plex source is a smart collection that ingests zero members ([#139](https://github.com/McBrideMusings/etv-station/issues/139)), so its shows are listed by name. Observed drift on a real generation (`America/Chicago`): the 21:00:00 daypart boundary opened at 21:07:01 — the Aqua Teen Hunger Force episode airing when the boundary passed ran to completion first, a 7m01s overrun — and the following 00:00:00 boundary (primetime handing back to the default pool) opened at 00:18:49, an 18m49s overrun from the Rick and Morty episode in progress at midnight. Both stayed within the longest item in their pool, as ADR 0004 predicts. HBO and Bravo are not yet built: both need the same `#139` collection-ingest workaround Adult Swim uses.

**Markup the ported channels are waiting on.** Keyword scoring so a comedy pool stops admitting horror-comedies ([#161](https://github.com/McBrideMusings/etv-station/issues/161)), named and seed-inferred categories over it ([#170](https://github.com/McBrideMusings/etv-station/issues/170), [#175](https://github.com/McBrideMusings/etv-station/issues/175)); per-show cursors so a series resumes wherever it surfaces ([#160](https://github.com/McBrideMusings/etv-station/issues/160)); arc ranges as pseudo-seasons ([#163](https://github.com/McBrideMusings/etv-station/issues/163), [#171](https://github.com/McBrideMusings/etv-station/issues/171)); standalone chronologies ([#164](https://github.com/McBrideMusings/etv-station/issues/164)); show groups as a rotation domain ([#165](https://github.com/McBrideMusings/etv-station/issues/165)); date-windowed blocks with a first-match cascade ([#162](https://github.com/McBrideMusings/etv-station/issues/162)); item-bound overlays ([#174](https://github.com/McBrideMusings/etv-station/issues/174)).

Two of those channels turned out to need catalog work rather than markup. Selecting on "well reviewed" is impossible today because the catalog stores `content_rating` — who may watch — and no critic or audience score at all ([#178](https://github.com/McBrideMusings/etv-station/issues/178)). And inferring a category from example titles alone ranks keywords by how rare they are library-wide, which never forces them to separate the seeds from what they are nearly identical to; counter-example seeds are what make a category exclude its near misses ([#179](https://github.com/McBrideMusings/etv-station/issues/179)).

**Out of this repo entirely.** Negative seeds, embedding similarity, crowd-list bootstrap, affinity-graph expansion, network and critic-rating enrichment, weighted external collections, awards data, trending edges — all need an extended metadata graph. They reach `etv-station` only as pool-provider plugins once the hooks above exist.

## Later

- **Lottie animation runtime spike** — designer-friendly After Effects format for richer overlays via [`velato`](https://github.com/linebender/velato). Tracked as a side project; the maintainer can author equivalent behavior in Rhai for now.
- Live event injection ([#16](https://github.com/McBrideMusings/etv-station/issues/16), closed) — operator declares a one-shot override window. No channel in the current inventory needs it, and the inventory is satisfied by dayparting ([#14](https://github.com/McBrideMusings/etv-station/issues/14), shipped) plus date windows. Recorded here so it is not picked up before a channel needs it; reopen the issue with that channel named.
- Web UI for editing channels and items ([#17](https://github.com/McBrideMusings/etv-station/issues/17), closed) — the channel count threshold is met at 62, but the ergonomic one is not: those configs total 1,741 lines, median 19 per channel. Reload-without-shelling shipped as `admin reload`, and the XMLTV guide already covers "what is airing" for every channel. Reopen with the specific editing task that hurts, not a channel count.
- Public open-source release — revisit once the rule abstraction is validated by 2+ rule implementations.

## Deferred / won't fix

- Real-time control plane (REST API, network injection) — v1 is config-file driven by design. Reload signal + file-watcher are sufficient.
- Encoding decisions (ffmpeg invocation for transcode, hwaccel selection) — ETV-next's responsibility. `etv-station` only reads media metadata via `ffprobe` for duration.
- Forking ETV-next to add scheduling — eats merge conflicts forever. Companion-program approach was chosen specifically to avoid this.
- Library importer (Plex / Jellyfin / Sonarr metadata) as a separate tool ([#18](https://github.com/McBrideMusings/etv-station/issues/18), closed) — superseded by Phase C's runtime catalog ingester. The catalog needs one metadata source and Plex is it; Jellyfin and Sonarr are not tracked as ingesters.
- Random / shuffle rule ([#15](https://github.com/McBrideMusings/etv-station/issues/15), closed) — subsumed by `order = "random"` with a channel-level seed, plus per-pool `weight` and per-step `chance`. One piece did not ship and is tracked separately: `no_repeat_within` counts positions rather than time ([#185](https://github.com/McBrideMusings/etv-station/issues/185)).
