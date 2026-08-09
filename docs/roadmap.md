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

**Shipped.** Per-channel `etv-overlay` subprocess renders Vello frames to a fifo etv-next composites on. Rhai scripts read the station-emitted chunked playout JSON to template lower-thirds with the current/next item's title and gate visibility on `item_elapsed` / `item_remaining`. Per-layer overrides (visibility, opacity, content, corner) compose with global `visible`/`opacity`. Sample scripts in `crates/etv-overlay/fixtures/scripts/`: `now_playing.rhai`, `up_next.rhai`, `pulse_watermark.rhai`, `corner_rotate.rhai`, `now_and_next.rhai`.

Out of scope until Phase C: scripted `size`/`color`, channel/block/item overlay cascade (#48). Lottie / `velato` tracked separately (#50).

### Phase C — Schema overhaul

With the query language picked and graphics rendering working, redesign the channel/block/entries schema and integrate everything:

- New TOML (or YAML) schema with blocks, channels, `[[entries]]`, includes, modes (`all` / `count`), filters, channel-seeded random order.
- Pools + pattern interleave ([#72](https://github.com/McBrideMusings/etv-station/issues/72), shipped) — "1 movie, then 3 episodes, repeat" across independently-progressing series, with a `.resume` sidecar carrying progression across window seams. Ships the resume-map half of the generation model.
- Generation model ([#70](https://github.com/McBrideMusings/etv-station/issues/70), shipped) — the play-history ledger: one `.history` line per scheduled airing, with the per-series resume cursor as a projection of it rather than a second store.
- Adjacency constraints (#73) — `[constraints]` with both the identity rule (`no_repeat_within = N`) and the property rule (`separate_by` + `separate_min_gap`, e.g. no two nearby films sharing a performer), enforced over the whole channel list and across the generation seam via the ledger. On a pattern block the table moves to the pool ([#115](https://github.com/McBrideMusings/etv-station/issues/115), shipped): the rule is enforced inside the pool — once over its resolved list, then again on every draw — so a repair can no longer swap items between the pattern's slots, and the gap counts that pool's draws.
- One emission model. The `LoopForever` rule and its `.anchor` sidecar are gone: every channel materializes forward, and a channel whose list never changes loops by repeating that list. Pool `wrap = "drop"` and the "channel exhausted" state went with them — television does not stop when it reaches the end of its library.
- Plex catalog ingester + local-FS catalog ingester (bumpers / commercials / errata).
- Runtime query resolution with snapshot-at-boot and configurable refresh interval.
- Graphics overlay cascade: channel default → block override → item override.
- Migration script from current `[rule] type = "loop_forever"` configs.

### Phase D — Plugin boundary and catalog-aware scheduling

**[Milestone](https://github.com/McBrideMusings/etv-station/milestone/6).** The 58 ported channels split cleanly into three groups: ones the current schema already builds, ones needing markup the schema does not have yet, and ones needing a metadata graph that does not live in this repo. Phase D builds the second group and draws the boundary the third reaches through.

**The plugin contract splits into two declared hooks.** ADR 0002 already makes a plugin replace a pool's `expr`, which is the candidate-set hook; what is missing is a plugin saying so. A plugin declares which hooks it implements ([#159](https://github.com/McBrideMusings/etv-station/issues/159)), declares the host capabilities it needs so nothing is ambient and core never links an external store it was not granted ([#167](https://github.com/McBrideMusings/etv-station/issues/167)), and returns entries carrying an optional metadata blob and per-entry take override rather than bare ids ([#166](https://github.com/McBrideMusings/etv-station/issues/166)). The second hook, `sequencer` ([#169](https://github.com/McBrideMusings/etv-station/issues/169)), emits a block's final timeline in place of the pattern walk. Reproducibility becomes checkable rather than assumed ([#168](https://github.com/McBrideMusings/etv-station/issues/168)).

Most channels need exactly one of the two hooks — a taste model needs a custom candidate set and standard arrangement; a network mirror needs the reverse. Keeping them separate is what stops the plugin surface from ballooning.

**Markup the ported channels are waiting on.** Keyword scoring so a comedy pool stops admitting horror-comedies ([#161](https://github.com/McBrideMusings/etv-station/issues/161)), named and seed-inferred categories over it ([#170](https://github.com/McBrideMusings/etv-station/issues/170), [#175](https://github.com/McBrideMusings/etv-station/issues/175)); per-show cursors so a series resumes wherever it surfaces ([#160](https://github.com/McBrideMusings/etv-station/issues/160)); arc ranges as pseudo-seasons ([#163](https://github.com/McBrideMusings/etv-station/issues/163), [#171](https://github.com/McBrideMusings/etv-station/issues/171)); standalone chronologies ([#164](https://github.com/McBrideMusings/etv-station/issues/164)); show groups as a rotation domain ([#165](https://github.com/McBrideMusings/etv-station/issues/165)); date-windowed blocks with a first-match cascade ([#162](https://github.com/McBrideMusings/etv-station/issues/162)); item-bound overlays ([#174](https://github.com/McBrideMusings/etv-station/issues/174)).

Two of those channels turned out to need catalog work rather than markup. Selecting on "well reviewed" is impossible today because the catalog stores `content_rating` — who may watch — and no critic or audience score at all ([#178](https://github.com/McBrideMusings/etv-station/issues/178)). And inferring a category from example titles alone ranks keywords by how rare they are library-wide, which never forces them to separate the seeds from what they are nearly identical to; counter-example seeds are what make a category exclude its near misses ([#179](https://github.com/McBrideMusings/etv-station/issues/179)).

**Out of this repo entirely.** Negative seeds, embedding similarity, crowd-list bootstrap, affinity-graph expansion, network and critic-rating enrichment, weighted external collections, awards data, trending edges — all need an extended metadata graph. They reach `etv-station` only as pool-provider plugins once the hooks above exist.

## Later

- **Lottie animation runtime spike** — designer-friendly After Effects format for richer overlays via [`velato`](https://github.com/linebender/velato). Tracked as a side project; the maintainer can author equivalent behavior in Rhai for now.
- [Dayparting](https://github.com/McBrideMusings/etv-station/issues/14) — a block that airs at a fixed time on fixed weekdays. Blocks the network-mirror channels (Adult Swim, HBO, Bravo). Open call: block-level schema field, or a Phase D sequencer plugin. Design alongside [#162](https://github.com/McBrideMusings/etv-station/issues/162), which is the calendar half of the same conditional-selection question.
- [Live event injection](https://github.com/McBrideMusings/etv-station/issues/16) — operator declares a one-shot override window. No channel in the current inventory needs it; recorded so it is not picked up before one does.
- [Web UI for editing channels and items](https://github.com/McBrideMusings/etv-station/issues/17) — once channel count grows past TOML-by-hand ergonomics.
- Public open-source release — revisit once the rule abstraction is validated by 2+ rule implementations.

## Deferred / won't fix

- Real-time control plane (REST API, network injection) — v1 is config-file driven by design. Reload signal + file-watcher are sufficient.
- Encoding decisions (ffmpeg invocation for transcode, hwaccel selection) — ETV-next's responsibility. `etv-station` only reads media metadata via `ffprobe` for duration.
- Forking ETV-next to add scheduling — eats merge conflicts forever. Companion-program approach was chosen specifically to avoid this.
- [Library importer (Plex / Jellyfin / Sonarr metadata) as a separate tool](https://github.com/McBrideMusings/etv-station/issues/18) — **superseded by Phase C's runtime catalog ingester.** Issue #18 stays open until Phase C lands, then closes with a pointer.
- [Random / shuffle rule](https://github.com/McBrideMusings/etv-station/issues/15) — **subsumed by Phase C's `order = "random"` with channel-level seed.** Closes with Phase C.
