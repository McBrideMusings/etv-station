# PRD — `etv-station`

A standalone playout-JSON generator daemon for [ErsatzTV-next](https://github.com/ErsatzTV/next). Companion to ETV-next, not a fork of it.

## Background

ETV-next ([upstream](https://github.com/ErsatzTV/next)) is a Rust IPTV server that consumes playout JSON files (described by `schema/playout.json`) with absolute timestamps and produces normalized HLS streams + XMLTV EPG. Its README explicitly states:

> Library and metadata management, scheduling and playout creation **are not in scope for this project**.

Therefore anyone running ETV-next must produce playout JSON externally. The bundled `ersatztv-playout-generator` is documented as "for development and testing only" — it writes a single 24-hour window with no rolling and no rule abstraction. There is no real production-grade playout generator in the ecosystem today.

`etv-station` fills that gap. It is positioned as the operator-side companion: ETV-next does transcoding and streaming reliably; `etv-station` decides what to play and writes the JSON that drives it.

## Goals

1. **Continuously feed ETV-next.** At any moment, every configured channel has playout JSON files on disk whose `[start, finish)` window contains "now" and extends N days into the future.
2. **Composable sequencing.** A channel is defined by blocks — flat entry lists or pool/pattern interleaves — resolved into one ordered list per generation. Architecture supports adding composition primitives without rewriting the core.
3. **Embed program metadata.** Items carry title / description / season / episode / categories / rating / artwork — written into the `program` block of each playout item so ETV-next's XMLTV is populated.
4. **Stay decoupled from ETV-next.** Filesystem-only contract. No IPC, no shared process, no schema fork. ETV-next's `schema/playout.json` is the boundary.
5. **Track ETV-next's schema without drifting.** Achieved by depending on ETV-next's `ersatztv-playout` Rust crate at the source level, via a git submodule (see Architecture below).

## Non-goals

- **Library management.** No NFO scraping, no online metadata providers, no media DB. Items are declared explicitly in config; the operator is responsible for accurate paths and metadata. (If they want richer metadata, that's another program upstream of this one.)
- **Real-time control plane.** v1 is config-file driven, not network-driven. No web UI, no REST API, no live-event injection endpoint. Config edits + reload signal are sufficient for v1.
- **Encoding decisions.** This program never invokes ffmpeg for encoding, never renders frames. It only reads media metadata it needs to produce playout entries (e.g. duration via `ffprobe`). Track selection / normalization / hwaccel is ETV-next's job.
- **Modifying ETV-next.** No PRs against `etv-next-private` originate from this repo as a side-effect of station work. If a schema change is needed, that's a deliberate, separate effort against the submodule.

## Architecture

Two programs, one shared filesystem, one shared schema.

```
                                ┌─────────────────────┐
                                │  shared volume      │
                                │  /playout/<chan>/   │
                                │    {start}_{finish}.json
                                │                     │
┌────────────────────┐  writes  │                     │  reads  ┌────────────────────┐
│  etv-station       │ ────────▶│                     │ ◀────── │  etv-next          │
│  container         │          └─────────────────────┘         │  container         │
│                    │                                          │                    │
│  rules → JSON      │                                          │  JSON → HLS+XMLTV  │
└────────────────────┘                                          └────────────────────┘
        │                                                                ▲
        │ reads                                                          │ HTTP
        ▼                                                                │
   station configs                                              IPTV clients (Plex,
   (channels, items,                                             Jellyfin, Channels DVR,
    rules)                                                       Kodi, …)
```

### Repository layout

`etv-station` is its own private GitHub repo (`McBrideMusings/etv-station`). It pulls ETV-next in as a **git submodule** for build-time access to the playout schema:

```
etv-station/                                 ← this repo (Cargo workspace root)
├── Cargo.toml                               ← workspace
├── crates/
│   └── etv-station/
│       ├── Cargo.toml                       ← path-dep on ../../etv-next/crates/ersatztv-playout
│       └── src/
├── etv-next/                                ← submodule → McBrideMusings/etv-next-private
│   └── crates/ersatztv-playout/             ← schema source of truth
├── docs/
│   └── PRD.md
└── README.md
```

The submodule pinning means:
- `etv-station` always builds against a known, reviewed commit of ETV-next's schema crate. No schema drift is even *expressible* — they share serde models.
- Adopting an upstream schema change is a deliberate two-step: pull `origin/main` into the submodule, bump the submodule SHA in `etv-station`, rebuild. If the schema change is incompatible, you find out immediately at compile time — not at runtime, not in production.
- `etv-next-private` itself has two upstreams: `origin` = `ErsatzTV/next` (Jason Dove), `mine` = `McBrideMusings/etv-next-private`. Standard fork pattern; lets you carry private patches against Jason's tree if ever needed.

### Deployment

One Docker image carrying both programs, three mounts:

```
docker run -d --name etv-station \
  -v /mnt/user/appdata/etv-station:/config \       # station.yaml, channels/, blocks/, overlays/
  -v /mnt/user/appdata/etv-station/data:/data \    # playout JSON, HLS working set, catalog cache
  -v /mnt/user/media/library:/media:ro \           # the library, for ffprobe and playback
  -p 8419:8409 \
  -e PLEX_URL=... -e PLEX_TOKEN=... \
  etv-station:latest
```

The image is built from this repo's `Dockerfile`: one builder stage for the station workspace, one for the ETV-next submodule, and a runtime stage on ErsatzTV's own ffmpeg image carrying `etv-station`, `etv-overlay`, `ersatztv`, and `ersatztv-channel`. `docker/entrypoint.sh` renders ETV-next's config from `station.yaml`, creates every channel's playout folder, then runs both processes.

Key properties:
- The playout folder is a plain directory inside the container that the daemon writes and ETV-next reads. Lock-free producer/consumer; the OS guarantees atomicity for `rename(2)`.
- The two halves still fail independently: a crashed daemon is restarted in place while ETV-next keeps serving the materialized window; a crashed ETV-next ends the container and the restart policy takes over.
- Neither program has any knowledge of the other at the protocol level. The only coupling is the playout JSON schema and the directory layout convention — and the layout is derived from `station.yaml` at every start, never authored twice.
- Config and code deploy separately: editing a channel is an rsync of the config folder, changing behavior is an image rebuild. Neither forces the other.

## Emission model

Every channel materializes **forward**. Each generation resolves the channel, lays the resulting sequence end-to-end after the last thing already written, and records the seam. The emitted chunk JSON is the durable timeline.

**One generation is about one window.** Whatever the channel is made of, a pass stops once it has laid down the airtime still missing between the last thing written and `window_days` from now, and records where it stopped so the next pass continues there. Without that stop the pass runs as long as the channel's content happens to be — a 950-item authored list is a month, and 51 pools of films is eleven years — and everything it wrote is a decision already made, so an edit to the channel file changes nothing on air until all of it has played out. The overshoot is at most one indivisible unit: one item for a flat list, one pattern cycle for an interleave.

There is **one** model, not one per rule. An earlier design had a separate "Loop Forever" rule that resolved a list once and replayed it from a persisted `.anchor` for any `t` via `(t - anchor) mod total_loop_duration`. It was removed: a channel whose list never changes re-resolves that same list each generation and takes the next stretch of it, and those stretches laid end-to-end *are* the loop, so looping needs no rule of its own. Keeping it also cost correctness in two places — a list that advances between generations (any pool with `advance = "resume"`) re-anchored and restarted its schedule on every change, and an unseeded `order = "random"` channel resolved exactly once per process, replaying one shuffle until the daemon restarted rather than reshuffling per pass.

**Running out is not a state.** A series that reaches its last item starts over. Nothing retires a series or a pool, because a television channel does not stop broadcasting when it reaches the end of its library. Resolving to zero items therefore always means the resolved *set* is empty — an expression that matches nothing, an empty catalog — which is a config error and is reported as one.

**Joining mid-list.** A channel may set `anchor` to an instant it is treated as having been broadcasting since. It affects only the first generation of a channel with nothing written yet: rather than starting at item 0, the channel joins its list where elapsed time since the anchor says it should be, so a newly-added channel feels like it has been running all along. The items it skips past do not come back — joining at the anchor is a claim that they already aired. After that the written timeline carries the phase; the anchor is a starting position, not a repeating origin.

**Determinism**
Generation is a pure function of `(catalog, config, resume_in)`: the same three inputs always produce the same items and the same `resume_out`. This is what makes regeneration after a config edit safe.

### Pattern interleave (Phase C)

A block declares named **pools** and a repeating **pattern** instead of a flat `entries` list — "1 movie, then 3 episodes, repeat", drawing each step from a different resolved set while every series progresses independently. A block is one or the other; a pattern block that also carries a block-level `order` or `duplicates: collapse` is rejected at load, because either would silently undo the interleave.

**Pool knobs** — every default is the stateless, least-surprising one, so a pool naming only `expr` behaves like a `query` entry.

| Field | Default | Meaning |
|---|---|---|
| `expr` | — | CEL query, as on a `query` entry |
| `order` | query order | Internal sort; also fixes the series rotation order |
| `select` | `round_robin` | *Which* series serves next — `round_robin` or `random` |
| `rotate` | `visit` | *When* the series changes — `visit` (take N consecutive from one series) or `slot` (a new series every item) |
| `advance` | `restart` | `restart` replays from the top; `resume` continues from the resume map |
| `on_short` | `next` | Who fills slots the current series can't supply — `next`, `wrap`, or `short` |

A pattern step is `{pool, take, chance}`. `chance` (default `1.0`) makes a step fire probabilistically — the "occasionally binge" knob. The roll is keyed on `(seed, cycle, step)`, so a pinned `seed` reproduces the whole skip/fire sequence, and a skipped step consumes no cursor.

A series is keyed by the catalog `show_id`; an item without one — a movie — is its own series of one, which is why a movie pool needs no special case.

`cycles` left unset means "run until the window is covered". Draining the largest pool once is only the ceiling: the walk stops at the first cycle boundary past `window_days`, and the next roll tick picks the pools up where it left them. Without that stop a block with dozens of pools would lay down years of schedule in a single pass, and a channel already booked to 2037 ignores every later edit to its config. An authored `cycles` is exempt — that number is the author saying how long a pass runs.

A series that reaches its last item starts over. That is the only behaviour there is — there is no setting that retires a series or a pool, because a television channel does not stop broadcasting when it reaches the end of its library.

### Generation model

Channels **materialize forward**. Generation is a pure function of `(catalog, config, resume_in) → (items, resume_out)`. Each pass lays its sequence end-to-end after the last thing already written and stores where it got to in a `.resume` sidecar; already-written chunk JSON is never rewritten, so the emitted files are the durable timeline and the sidecar holds only the seam. There is no live cursor anywhere.

Two files carry that state, and they hold different things. The **play-history ledger** (`.history`) is one JSONL line per scheduled airing — `entry_id`, `show_id`, the scheduled `start`, and when the row was written. It is a dumb record: no taste logic, no TTL, no relevance. Where each series left off is a **projection** of it ("the last airing per `show_id`"), so there is exactly one place that knows a show's position and nothing to drift out of sync. A future taste scorer reads the same lines the other way — all of them, with timestamps. One structure, two read shapes.

A series' position is recorded as the **last-played `entry_id`**, never an index: a pool's resolved set churns as the catalog and the query behind it change, and an index would silently mean something else after any change. An id that has vanished restarts its own series and no other, and a show that leaves the resolved set entirely and later returns resumes where it stopped, because the ledger is never pruned to the current set. A torn line is skipped rather than failing the channel.

A **flat `entries` channel** is the one place an index is right, and it is a different question: not "where is this show up to" but "how far down the authored list did the last generation get". That list is written in the config file rather than derived from a query, so item 37 means the same thing next tick; a config edit rewinds through the checkpoints below, which restore the position along with everything else. It is one number for the channel's blocks concatenated in config order, and a list that got shorter folds it back inside itself instead of running off the end.

The `.resume` sidecar holds only what the ledger cannot express: which series is next in each pool's rotation, how far into a flat list the channel got, and the checkpoints below. A missing or corrupt one starts every pool and every list from the top rather than failing the channel.

It also carries **checkpoints**: the scheduling state — pool rotation and list position — entering each generation that has not started airing. On startup the channel rewinds to the earliest of them, deletes the emitted files from that instant forward, and regenerates them from the current config — so a config or overlay edit reaches a pattern channel without waiting for its whole written window to play out, and without losing or repeating an item. Aired and currently-airing chunks are never touched.

There is no exhausted state. A channel cannot play its way to an empty list, because every series loops — so resolving to zero items always means the resolved *set* is empty (an expression that matches nothing, an empty catalog), which is a config error and is reported as one.

Both halves of the generation model are now in place: the resume map (#72) and the play-history ledger (#70). What remains open is what reads the ledger the *other* way — the taste scorers of #74 and #82.

### Future rules (designed for, not implemented)

- **Recurring grid** — "Tue 8pm = X; Wed 9pm = Y; otherwise fall through to a base loop."
- **Random / shuffle** — pick from a pool with constraints (no repeats within window, weight per item).
- **Hybrid** — multiple rules layered with priorities.
- **Live event injection** — operator declares "between [start, stop] play this; resume normal afterward."

The rule trait must accept these without core changes. v1 only validates the abstraction by implementing one rule.

## Inputs (per channel)

A `channel.toml` declaring:

| Field | Required | Description |
|---|---|---|
| `name` | no, default: config file stem | Channel identity override — drives the log label, overlay handshake, and output folder leaf. No path separators. |
| `window_days` | no, default 1 | How far into the future to materialize. Also the length of one generation, whichever shape the channel is: a pattern block with no `cycles` stops at the first cycle boundary that covers this span, and a flat `entries` list longer than the span stops at the item that covers it and resumes there next time. Neither can book the channel years ahead. |
| `chunk_hours` | no, default 6 | Each playout JSON file's `[start, finish)` span. File size only — it does not gate how far ahead the scheduler works. |
| `roll_interval` | no, default `1h` | How often to extend the window forward. |
| `retention_days` | no, default 7 | Past playout files older than this get deleted. |
| `rule` | yes | Rule type + rule-specific params. |
| `items` | yes (for an entries block) | Ordered list with metadata. |

A channel does **not** declare its own output folder. The daemon derives it as `{output_base}/{identity}`, where `output_base` is a station-level field and `identity` is the channel's `name` (above) or, unset, its config file stem. ETV-next still reads playout files from that same folder, configured on its own side.

A top-level station file (`station.toml` or `station.yaml`) declares `output_base` and lists the channel configs — mirrors how ETV-next's `lineup.json` lists its channels. It also carries the station-wide time zone (see below). Each `channels` entry is a literal path or a glob (e.g. `channels/*.yaml`) resolved relative to the station file; a glob expands to every match. The `ETV_STATION_OUTPUT_BASE` environment variable overrides `output_base` at runtime (the Docker-friendly knob), the same way `ETV_STATION_TZ` overrides `tz`.

## Time zone

The station file declares a station-wide `tz` field — an IANA zone name (e.g. `America/Chicago`). Default `UTC`. The `ETV_STATION_TZ` environment variable overrides the file value at runtime, which is the Docker-friendly knob.

The configured zone affects **chunk-boundary alignment only**: a 24-hour chunk rolls at local midnight in the station tz, not at 00:00 UTC. Persisted timestamps in the sidecars stay in UTC — tz is a presentation/scheduling concern, not a storage one. Emitted RFC3339 timestamps in the playout JSON itself can carry whatever offset is convenient (UTC is fine; ETV-next reads absolute instants).

Per-channel `tz` override is **not** in v1 — single household, single zone. Adding it later is a strict superset (channel-level overrides station-level) so deferring is safe.

## Outputs

- Files in `output_folder/` named `{start}_{finish}.json` with compact ISO 8601 timestamps (no separators) — exactly the format ETV-next's loader (`crates/ersatztv-channel/src/playout_loader.rs::playout_file_for_time`) expects.
- Each file conforms to ETV-next's `schema/playout.json` — including the `program` metadata block we added during the EPG work.

## Behavior over time

**Startup**
1. Read the station file + each channel config.
2. For each channel: scan `output_folder/` for existing playout files; compute the latest `finish` already materialized.
3. If less than `window_days` is materialized: render new chunks forward until full.
4. Compute the next roll tick.

**Roll tick**
1. For each channel: delete playout files whose `finish` < (now − `retention_days`).
2. Render new chunks until `window_days` from now is materialized.

**Config reload** (SIGHUP)
1. SIGHUP re-reads the station file and every channel config from disk. SIGTERM/SIGINT shut the daemon down; a file watcher is deferred (v2).
2. A malformed edit (parse error, unknown timezone, invalid overlay spec) is logged and rejected — the previous, still-valid config keeps running and the daemon does not exit.
3. On a valid reload the daemon stops every channel's playout + overlay tasks and re-runs them against the new config. Today this reuses the startup path, which wipes all emitted JSON and regenerates the future window for every channel (see [#53](https://github.com/McBrideMusings/etv-station/issues/53)); the targeted in-place rewrite of only the changed channels' future files is the intended end state. Determinism (see above) makes regeneration safe.

**Unreadable media**
One file the player cannot open is one bad slot, never a dead channel. When
`ffprobe` fails or the file has gone missing, the item keeps its place and its
length — taken from the catalog's `duration_ms`, which is what the source said
the item runs — and airs a black card naming the title and the underlying error
instead. The item keeps its program metadata, so the guide still lists what was
meant to air and a viewer sees why it is not playing. Only an item whose length
is unknown *as well* is dropped, since there is no slot to put a card in; both
cases log once with the path and reason so the library can be repaired.

A catalog row that carries no playback source at all is skipped the same way, for
the same reason: a single hollow row must not take down every channel whose query
happens to match it.

**Crash safety**
Files are written atomically (write to temp + `rename(2)`). ETV-next is unaffected by `etv-station` being down — it keeps playing materialized files until the window expires.

## Open questions

| # | Question | Current answer |
|---|---|---|
| 1 | Daemon vs. cron-invoked one-shot? | Daemon. Roll cadence + reload watcher both want a long-lived process. |
| 2 | Scheduling-state persistence | Sidecar files per channel: `.resume` (rotation + checkpoints) and `.history` (the play ledger). |
| 3 | Source-media duration probing | `ffprobe` at config-load time; cache durations in the `.durations.json` sidecar. Re-probe on file mtime change. |
| 4 | What if an item file is missing at probe time? | Fail loudly at config load (don't silently substitute). v1 is explicit about its inputs. |
| 5 | Logging/observability | stdout structured logs (JSON lines). Container runtime captures them. No metrics endpoint v1. |
| 6 | What if `etv-next-private` updates `ersatztv-playout` in a breaking way? | Compile-time error on submodule bump. PR cycle on `etv-station` to absorb the change. Considered a feature. |

## Verification (v1 acceptance)

- One channel configured with a single entries block, 4 items totaling ~9 hours.
- `etv-station` and `etv-next` running continuously for 7 days in one container over the shared playout folder.
- At every probe (hourly): ETV-next's `/channel/1.m3u8` returns valid HLS, `/xmltv.xml` includes correctly populated `<programme>` entries for the next ≥7 days, and ETV-next's logs contain zero `unable to find playout JSON file for time …` errors.
- Killing the `etv-station` process mid-run: the entrypoint restarts it, and ETV-next serves without interruption throughout; even with the restart suppressed, the failure mode is graceful degradation (back to synthetic black + silence once the materialized window ends), not an immediate outage.
- Restarting `etv-station`: the next roll tick refills the window without rewriting past files.
- Bumping the `etv-next` submodule by one commit: `cargo build` either still succeeds (schema-compatible change) or fails with a clear compiler diagnostic (schema-incompatible change). Either outcome is acceptable; silent runtime drift is not.

## Out of scope for v1, candidate for v2+

- Web UI for editing channel rules and items.
- A library importer that reads from Plex/Jellyfin/Sonarr metadata.
- Live event injection.
- Multi-rule channels (hybrid / layered rules).
- Distributed mode (multiple `etv-station` instances coordinating via leader election).
- Public open-source release. The repo is private at v1; once the rule abstraction is stable and one or two non-Loop-Forever rules exist, revisit publishing as "the companion piece to ETV-next."

## Scope evolution beyond v1

v1 is intentionally the smallest useful playout generator: hand-authored item lists, one rule, no library awareness, no overlay graphics. As v1 stabilizes, real-channel building (Star Trek release-order, Dragon Ball franchise-chronological, mixed bumper/movie blocks, etc.) has surfaced three concrete pains that the v1 model can't address:

1. **Authoring verbosity.** Hand-typing 29-episode Star Trek seasons (or 950-episode all-Trek lineups, or hundreds of Dragon Ball entries) with full path + program metadata is unworkable.
2. **Lack of composition.** A "show" can't be defined once and reused across channels; favorites/subset channels copy-paste.
3. **Graphics-less output.** Channels look like raw media playback, with no idents/bugs/lower-thirds. ErsatzTV's graphics engine concept is exactly the missing piece.

v2+ work proceeds in **three sequential phases**, each a milestone with focused issues. The order is deliberate — each de-risks the next, and the schema overhaul (the largest piece) comes last so it can integrate the foundations rather than predict them.

### Phase A — Query language evaluation

Live content sourcing requires a query language. ErsatzTV's Lucene variant had documented failure modes (prefix overmatch, no absolute episode numbers across show variants). Per the global off-the-shelf-first rule, we evaluate existing languages — top candidate [CEL](https://cel.dev/) via `cel-rust`, fallback Plex-API pass-through with structured TOML filters — against real-world channel-building cases. The deliverable is a standalone query tester (`crates/etv-query-test`) and a documented language pick. No daemon integration, no schema commit.

### Phase B — Graphics rendering (spike + static text shipped 2026-05-12; dynamic text templating remaining)

Inspired by [ErsatzTV's graphics engine](https://ersatztv.org/docs/advanced/graphics-engine/), but authored in a real scripting language ([Rhai](https://rhai.rs/)) rather than YAML. Two tracks:

- **Static.** Hardcoded channel watermark via [Vello](https://github.com/linebender/vello). Establishes overlay rendering inside etv-next's output pipeline and extends `PlayoutItem` with overlay config (etv-next submodule change).
- **Scripted.** Rhai-driven dynamic behavior — visibility, corner, size, opacity, fade-on-interval, now-playing / up-next text.

Deliverable: a working overlay pipeline with a small declarative + scripted primitive set. Lottie / `velato` integration is a side project, not a blocker.

### Phase C — Schema overhaul

With the language picked and graphics working, redesign the user-facing schema:

- **Block as the unit of reuse.** A block = `[program]` defaults + flat `[[entries]]` list (item / query / collection / include). Blocks are content-agnostic — TV, movies, home movies, bumpers, mixed.
- **Authoring format is by extension.** Every config file — `station`, `channel`, and path-referenced *block files* — may be authored in either TOML or YAML, selected by file extension: `.yaml`/`.yml` parse as YAML, anything else as TOML. Same serde types either way (no schema difference), so a station and its channels and blocks can all be one format. Inline entries inside a channel's `[[rule.blocks]]` stay in whatever format the channel file uses.
- **Channels compose blocks** via `[[rule.blocks]]` with `mode` (`all` or `count = N`), `order` (`manual`, seeded `random`, or a compound `field:dir` sort), and `filter` over the resolved item list.
- **Order is only what the items themselves determine.** A collection's hand-authored sequence is not an `order` value: `collection_items.position` belongs to the (collection, item) pair, so a flattened item list can no longer say which collection's positions to read. That sequence rides on a `collection` entry, which emits its members already ordered. Collections-as-a-set stays a `query` entry (`item.collections.contains(...)`) — one stored structure, two read paths. A relevance `score` failed the same test — it needed a plugin the item list can't reach — and is unspecified until there is a concrete source for it.
- **Adjacency constraints.** A block's `[constraints]` table carries two spacing rules. `no_repeat_within = N` is identity: the same item may not recur within N positions (`1` = never back-to-back). `separate_by = "<field>"` with `separate_min_gap = N` is property: two items sharing **any** value of a multi-valued catalog field may not sit within N positions, so `separate_by: "cast"` spreads out films sharing a performer. The field vocabulary is the one expressions use, so `separate_by: "cast"` reads what `item.cast` reads.

  Entries blocks resolve and order independently, then one pass runs over the whole concatenated channel list, so a conflict straddling a block join is caught too. The list reaches back across the generation seam via the play-history ledger, so a constraint holds between generations and not only inside one; how much history is carried is derived from the widest gap the config asks for. When the set offers no arrangement that satisfies everything (one title with "no two in a row", a cast too interlinked to separate), the remaining violations are accepted and generation completes rather than hanging — and are logged, so a channel quietly failing its constraint is distinguishable from one honouring it.

  **On a pattern block, `constraints` belongs to the pool.** The rule is enforced entirely inside each pool, never over the interleaved list — a pass over the finished list knows item ids and gaps and nothing else, so it repairs a repeat by swapping an episode into a movie slot and silently destroys the shape the pattern was written to build. Keeping it inside the pool makes the shape safe by construction. It takes two checks, because a pool makes repeats two ways: its resolved list is ordered under the rule before the pattern runs (which settles the opening item, the series rotation order, and the generation seam), and every draw is then checked against what the pool just emitted. The second is the one that usually bites — a query returns each item once, so the list has nothing to fix, while the draw loop repeats freely as the rotation holds its place on a half-filled visit and a played-out series loops. The gap therefore counts that pool's draws, not aired channel positions, and the seam is read the same way: the aired tail narrowed to what this pool could have supplied, with the retained history sized to cover the conversion. When nothing can be drawn without a clash, something is drawn anyway and the shortfall is logged. A block-level `[constraints]` on a pattern block is the default every pool that declares none inherits; a pool's own table replaces it wholesale rather than merging field by field. Two costs are real and stated rather than hidden: each pool is blind to every other, so pools that must not collide have to be disjoint by construction; and a no-repeat rule marches a heavily-drawn pool forward rather than letting it revisit, so the wider `window_days` is, the less a scorer plugin's own replay policy has left to suppress.
- **Unified catalog ingestion.** Plex (primary) + local-FS scan (bumpers / commercials / errata) feed a normalized **sqlite catalog** via `rusqlite`. Sonarr/Radarr deferred unless a Plex gap appears.
- **sqlite cache, not in-memory or JSON.** Tens of thousands of items rules out per-boot rescans (slow API round-trips) and full-file JSON snapshots (full reparse + RAM-resident). sqlite gives indexed lookups, incremental refresh from Plex's `lastUpdated`, WAL-mode concurrency between refresh writer and query reader, and `sqlite3` shell inspection for debugging. Schema is three tables — `items`, `collections` + `collection_items`, `catalog_meta` (per-source sync timestamps) — plus simple up-only migrations.
- **Runtime query resolution.** Channel TOML carries live queries; daemon translates them into sqlite reads at boot, snapshots the resolved item list for the chunk window. Stateless determinism preserved — the snapshot is the durable list, the catalog itself is the deterministically-rebuildable substrate.
- **Graphics overlay cascade.** Channel default → block override → item override, declared in the schema and emitted in the playout JSON.
- **Migration.** One-shot translator script from current `[rule] type = "loop_forever"` configs.

### Non-goal inversions

This phase reverses two v1 non-goals explicitly:

- *"Library management. No NFO scraping, no online metadata providers, no media DB."* — Phase C adds a Plex catalog ingester and an in-memory media DB. The scope is narrower than full library management (no scraping, no editing, read-only catalog) but it crosses the line v1 drew.
- *"A library importer that reads from Plex/Jellyfin/Sonarr metadata"* (listed as out-of-scope-for-v1) — Phase C makes Plex ingestion a first-class feature of the daemon. [Issue #18](https://github.com/McBrideMusings/etv-station/issues/18), originally framed as an external tool, is superseded.

### Non-goals that stand

- **Encoding decisions** stay etv-next's job. etv-station never invokes ffmpeg for transcoding; only ffprobe for duration.
- **Real-time control plane** is still deferred — v2+ remains config-file driven with reload signal / file watcher.
- **Modifying ETV-next** for non-schema reasons. The graphics overlay cascade *does* require an `PlayoutItem` schema extension on the etv-next side; that is a deliberate, planned submodule change, not drift.

---

## Decision log

This section captures decisions made *during PRD authoring* so future readers know what was considered and rejected.

- **Why not extend ETV-next directly with scheduling?** Upstream README explicitly excludes scheduling from scope. Adding it would mean a permanent fork, eating merge conflicts on every pipeline-side PR. Rejected in favor of separate-program approach.
- **Why not use the existing `ersatztv-playout-generator`?** Documented as "development and testing only," writes a single window, has no rule abstraction. Could be extended, but it lives inside the upstream repo — extending it = same fork problem. Rejected.
- **Why a separate repo (not a crate inside `etv-next-private`)?** Two reasons. (1) Clean independent release cadence and CI; the station program iterates on rules and metadata workflows that are unrelated to ETV-next's pipeline work. (2) Possible eventual public release as a standalone companion project — `etv-next-private` will always be private (it's a personal fork), but `etv-station` could be open-sourced cleanly without disentangling.
- **Why submodule rather than a vendored copy or a Cargo registry crate?** Submodule is the only option that gives source-level dependency on `ersatztv-playout` without forcing Jason to publish it on crates.io. Schema drift becomes a compile-time question. Vendoring duplicates the file and reintroduces drift risk.
- **Why filesystem-only IPC?** Matches ETV-next's existing process model (it already uses files for ready/heartbeat signaling between server and channel subprocesses). No new protocol surface. Easy to debug — `ls` shows you the state. Also: never builds in any assumption Jason has not himself adopted, so upstream evolution can't break the contract.
- **Why one container, not two?** *(Supersedes the original two-container decision.)* The playout folder is the entire interface between the two programs, so two containers meant sharing that folder, keeping two configs in step, and ordering their startup — machinery whose only purpose was separating two processes that are useless apart. One image makes the folder an ordinary directory and the deploy one image plus one config mount. The failure separation that motivated two containers is kept in `docker/entrypoint.sh`, which restarts a crashed daemon in place while ETV-next keeps streaming, and ends the container when ETV-next itself dies. What is actually given up: per-half resource limits and per-half restart cadence.
- **Why Rust?** Already chosen language for ETV-next, but the deciding factor is the submodule + path-dep approach: depending on `ersatztv-playout` as a Rust crate from inside the submodule is essentially free, and any other language would need to either re-implement the schema models (drift risk) or codegen them from `schema/playout.json` (added build complexity, weaker type safety than serde-on-the-shared-types).
