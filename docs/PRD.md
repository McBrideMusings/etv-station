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

Two Docker images, two containers, one shared volume:

```yaml
# docker-compose-style sketch (the real compose file lives elsewhere; this
# is just to fix the topology in the reader's head)
services:
  etv-next:
    image: ersatztv-next:latest
    volumes:
      - playout:/var/lib/ersatztv/playout:ro    # read-only mount
      - hls:/var/lib/ersatztv/hls
    ports: ["8409:8409"]

  etv-station:
    image: etv-station:latest
    volumes:
      - playout:/var/lib/ersatztv/playout       # read/write
      - ./station-config:/etc/etv-station:ro
      - media:/media:ro                         # for ffprobe duration reads

volumes:
  playout: {}
  hls: {}
  media:
    driver: local
    driver_opts: { type: bind, o: bind, device: /mnt/user/media }
```

Key properties:
- `etv-station` has read/write on the playout volume; `etv-next` has read-only. Lock-free producer/consumer; the OS guarantees atomicity for `rename(2)`.
- Either container can restart independently. ETV-next keeps serving from already-materialized files; etv-station picks up after the last thing it wrote.
- Neither container has any knowledge of the other's existence at the protocol level. The only coupling is the playout JSON schema and the directory layout convention.

## Emission model

Every channel materializes **forward**. Each generation resolves the channel, lays the resulting sequence end-to-end after the last thing already written, and records the seam. The emitted chunk JSON is the durable timeline.

There is **one** model, not one per rule. An earlier design had a separate "Loop Forever" rule that resolved a list once and replayed it from a persisted `.anchor` for any `t` via `(t - anchor) mod total_loop_duration`. It was removed: a channel whose list never changes resolves the same list every generation, and those laid end-to-end *are* the loop, so looping needs no rule of its own. Keeping it also cost correctness in two places — a list that advances between generations (any pool with `advance = "resume"`) re-anchored and restarted its schedule on every change, and an unseeded `order = "random"` channel resolved exactly once per process, replaying one shuffle until the daemon restarted rather than reshuffling per pass.

**Running out is not a state.** A series that reaches its last item starts over. Nothing retires a series or a pool, because a television channel does not stop broadcasting when it reaches the end of its library. Resolving to zero items therefore always means the resolved *set* is empty — an expression that matches nothing, an empty catalog — which is a config error and is reported as one.

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

A series is keyed by the catalog `show_id`; an item without one — a movie — is its own series of one, which is why a movie pool needs no special case. `cycles` defaults to enough passes for the largest pool to play through once.

A series that reaches its last item starts over. That is the only behaviour there is — there is no setting that retires a series or a pool, because a television channel does not stop broadcasting when it reaches the end of its library.

### Generation model

Channels **materialize forward**. Generation is a pure function of `(catalog, config, resume_in) → (items, resume_out)`. Each pass lays its sequence end-to-end after the last thing already written and stores where it got to in a `.resume` sidecar; already-written chunk JSON is never rewritten, so the emitted files are the durable timeline and the sidecar holds only the seam. There is no live cursor anywhere.

Two files carry that state, and they hold different things. The **play-history ledger** (`.history`) is one JSONL line per scheduled airing — `entry_id`, `show_id`, the scheduled `start`, and when the row was written. It is a dumb record: no taste logic, no TTL, no relevance. Where each series left off is a **projection** of it ("the last airing per `show_id`"), so there is exactly one place that knows a show's position and nothing to drift out of sync. A future taste scorer reads the same lines the other way — all of them, with timestamps. One structure, two read shapes.

Positions are therefore recorded as the **last-played `entry_id`**, never an index: the resolved set churns, and an index would silently mean something else after any change. An id that has vanished restarts its own series and no other, and a show that leaves the resolved set entirely and later returns resumes where it stopped, because the ledger is never pruned to the current set. A torn line is skipped rather than failing the channel.

The `.resume` sidecar holds only what the ledger cannot express: which series is next in each pool's rotation, and the checkpoints below. A missing or corrupt one starts every pool from the top rather than failing the channel.

It also carries **checkpoints**: the pool state entering each generation that has not started airing. On startup the channel rewinds to the earliest of them, deletes the emitted files from that instant forward, and regenerates them from the current config — so a config or overlay edit reaches a pattern channel without waiting for its whole written window to play out, and without losing or repeating an item. Aired and currently-airing chunks are never touched.

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
| `window_days` | no, default 30 | How far into the future to materialize. |
| `chunk_hours` | no, default 24 | Each playout JSON file's `[start, finish)` span. |
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
- `etv-station` and `etv-next` running continuously for 7 days as two containers sharing the playout volume.
- At every probe (hourly): ETV-next's `/channel/1.m3u8` returns valid HLS, `/xmltv.xml` includes correctly populated `<programme>` entries for the next ≥7 days, and ETV-next's logs contain zero `unable to find playout JSON file for time …` errors.
- Stopping `etv-station` mid-run: ETV-next continues serving until the materialized window expires; the failure mode is graceful degradation (back to synthetic black + silence after the window's end), not an immediate outage.
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
- **Adjacency constraints.** A block's `[constraints]` table carries `no_repeat_within = N`: the same item may not recur within N positions (`1` = never back-to-back). Blocks resolve and order independently, then one pass runs over the whole concatenated channel list, so a repeat straddling a block join is caught too. When the set offers no alternative (one title, "no two in a row"), the violation is accepted and generation completes rather than hanging. The richer property-level form — `separate_by = "cast"` with `separate_min_gap`, meaning no two adjacent films share a performer — is proposed, not v1.
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
- **Why two containers, not one?** Honest separation of failure domains. ETV-station can crash, leak memory, get stuck on a bad rule — ETV-next keeps streaming. Independent restart cadence. Independent resource limits. Independent images = independent CI. Cost: one more container to deploy. Worth it.
- **Why Rust?** Already chosen language for ETV-next, but the deciding factor is the submodule + path-dep approach: depending on `ersatztv-playout` as a Rust crate from inside the submodule is essentially free, and any other language would need to either re-implement the schema models (drift risk) or codegen them from `schema/playout.json` (added build complexity, weaker type safety than serde-on-the-shared-types).
