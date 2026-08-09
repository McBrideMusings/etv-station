# etv-station — System & Channel-Building Briefing

> Handoff document. Everything another session needs to help plan new channels and
> judge whether they're buildable with today's config markup + query language, or
> whether the markup needs to be expanded.
>
> Repo: `github.com/McBrideMusings/etv-station` (default branch `main`) — **private at v1**,
> so links resolve only for accounts with access.

---

## 1. What it is, in one sentence

`etv-station` is a standalone **playout-JSON generator daemon**. It decides "what plays
when" and writes schedule files to a shared folder; **ErsatzTV-next (etv-next)** reads
those files and does the actual transcoding/streaming (HLS + XMLTV EPG). It is a
*companion* to etv-next, deliberately **not a fork** of it.

## 2. The two-program architecture

One Docker container runs two programs over one shared folder, bound by one shared schema:

- **etv-station** — read/write on the playout volume. Reads TOML/YAML configs, applies
  sequencing rules, writes `{start}_{finish}.json` chunk files per channel.
- **etv-next** — read-only on the same volume. Loads the JSON file whose `[start, finish)`
  window contains "now," produces HLS + XMLTV over HTTP (port 8409).

The only coupling is (1) the playout JSON schema and (2) the directory-layout convention.
The schema is pinned via a **git submodule** used as a Rust path-dependency, so **schema
drift is a compile-time error**. IPC is filesystem-only; atomic writes via `rename(2)`.

**Key principle — the playout JSON is the contract.** All program metadata (title, episode,
description, artwork, rating, etc.) must be baked into each item by the station. etv-next
never scrapes, never queries a library, never reaches outward.

## 3. The config model — three levels, one data model

All three levels deserialize into the same serde types. YAML is standard; TOML still accepted
(parser picked by file extension).

| File | Holds |
|---|---|
| `station.yaml` | timezone, `output_base`, list of channels, optional catalog settings |
| `channels/*.yaml` | playout window + a `rule` that composes blocks |
| `blocks/*.yaml` | `program` metadata defaults + a flat `entries` list (the unit of reuse) |

A channel does **not** declare its own output folder — it's derived as
`{output_base}/{identity}` (identity = the channel's `name` or its config file stem).

## 4. Entry kinds — what goes in a block

Every entry is tagged by `kind`:

1. **`item`** — an authored single file. `source` is `local` (a path), `lavfi` (ffmpeg
   synthetic source e.g. `testsrc`), or `http` (a URL). Supports `in_point`/`out_point`
   trimming + per-item `program` metadata. Identity is *derived from the source*, never
   authored, so two entries pointing at the same file collapse.
2. **`query`** — resolve a **CEL expression** against the catalog, expand to all matches.
   e.g. `item.title.contains("Lord of the Rings")` with `order: "release_date:asc"`.
3. **`collection`** — play a catalog collection in its **authored order** (the drag-arranged
   Plex sequence via `collection_items.position`).
4. **`include`** — pull in another block file, with its own `mode`/`order`/`filter`.

## 5. The query language (CEL) — item field vocabulary

Queries run over an `item`. Scalar columns compare with `==`, `!=`, `in`, and text ones add
`contains` / `startsWith` / `matches`. Tag namespaces are multi-valued (membership via
`contains`). `source`, `collections`, `fs_dir` are membership over a related table (`==`).

| Field | Kind | Notes |
|---|---|---|
| `title` | string | |
| `show` | string | series name on an episode |
| `type` | string | `movie`, `episode`, `bumper`, `concert`, `power_hour`, `music_video`, … |
| `content_rating` | string | |
| `edition` | string | empty = theatrical |
| `studio` | string | one production company |
| `library` | string | scope a channel to one library by name |
| `year` | int | |
| `season` | int | |
| `episode` | int | |
| `absolute_episode` | int | franchise-wide episode number |
| `duration_ms` | int | |
| `genres` | tags | |
| `labels` | tags | |
| `cast` | tags | |
| `directors` | tags | |
| `tags` | tags | every namespace at once |
| `source` | membership | `item.source == "plex"` / `"fs"` |
| `collections` | membership | by collection name |
| `fs_dir` | membership | `item.fs_dir == "bumpers"` — the folder a file sits in |

Notes: a missing string compares as empty (`item.edition == ""` selects no-edition items).
`library` is by name (renaming in Plex silently breaks the query). `fs_dir` is the last
folder in the path, recomputed every run — move a file and the next scan re-buckets it.
`source`/`type` are orthogonal: source = catalog (plex/fs), type = semantic kind.

## 6. Composition & sequencing capabilities

**Block include fields** (`rule.blocks`): `mode` (`all` / `count: N`), `order`
(`manual` / `random` seeded by channel `seed` / compound `field:dir` sort like
`season:asc,episode:asc`), `filter` (`seasons` / `episode_ids`), block-level `duplicates`
(`collapse` / `keep`).

**Pattern interleave (pools)** — instead of a flat list, a block declares named **pools** +
a repeating **pattern**: "1 movie, then 3 episodes, repeat," each pool progressing
independently.
- Pool knobs: `expr` (CEL) or `plugin` (Rhai scorer), `order`, `bucket_order`, `group_by`
  (`show` / `season`), `select` (`round_robin` / `random`), `rotate` (`visit` / `slot`),
  `advance` (`restart` / `resume`), `on_short` (`next` / `wrap` / `short`).
- Pattern step: `{pool, take, from, chance}`. `take` = a count or `all` (empty the series).
  `from` = `start` / `end` / `random`. `chance` = probabilistic fire (seeded, reproducible).
- "Random season played start-to-finish" = `group_by: season` + `take: all` + `select: random`.

**Adjacency constraints** (`[constraints]`): `no_repeat_within: N` (identity) and
`separate_by: "<field>"` + `separate_min_gap: N` (property, e.g. spread out shared cast). On
entries blocks it runs over the whole flattened channel list; on pattern blocks it moves into
the pool.

**Scorer plugins** — a pool can name a Rhai `plugin` instead of `expr`: the script runs its
own catalog queries, ranks, returns an ordered set. The station computes no score itself.
Watch history comes from Tautulli. Scope is per-channel (`all_users` / `single_user`).

## 7. The generation / emission model (why it stays continuous & safe)

- **Materialize forward.** Each pass lays the resolved sequence end-to-end after the last
  thing written and records the seam. Emitted chunk JSON *is* the durable timeline.
- **One window per pass.** A generation only covers the airtime missing up to `window_days`
  ahead, then stops — so config edits land soon instead of after a pre-booked month.
- **Looping needs no rule.** A channel whose list never changes re-resolves it each pass;
  those laid end-to-end *are* the loop.
- **Running out is not a state.** A series that hits its last item starts over. Resolving to
  *zero* items = empty resolved set = reported config error.
- **Determinism.** Generation is a pure function of `(catalog, config, resume_in)`.
- **State sidecars:** `.history` (JSONL play ledger — the single source of "where each series
  left off") and `.resume` (rotation position, flat-list position, and checkpoints for safe
  reload).
- **Window sweep** enforces both ends (retention deletes + a critical forward-edge sweep that
  prevents a stray future-dated file from freezing generation forever).
- **Unreadable media** = one black card naming the title + error, not a dead channel.

## 8. The catalog (content sourcing)

A normalized **sqlite catalog** (rusqlite, WAL mode) feeds the query language:
- **Plex** (primary) — `PLEX_URL` / `PLEX_TOKEN`.
- **Local-FS scan** — bumpers/commercials/idents/errata; walks `source_roots`.
Ingest once at startup; per-channel read-only handles. Refresh governed by
`catalog_refresh_secs` (trust window) and `full_sweep_after_secs` (deletion-catching full
re-read). Watch history from **Tautulli** (`TAUTULLI_URL` / `TAUTULLI_API_KEY`).

## 9. Per-channel knobs

`window_days` (how far ahead / one generation's span, default 1), `chunk_hours` (JSON file
span, default 6), `roll_interval` (default 1h), `retention_days` (default 7), `seed`,
`overlay`, `scoring:` (for plugin channels), and the `rule` itself.

## 10. Time zone

Station-wide `tz` (IANA, default UTC, overridable via `ETV_STATION_TZ`). Affects
chunk-boundary alignment only; persisted timestamps stay UTC.

---

## 11. The "For You" channel ("Four U") — deep dive

**File:** `examples/samples/foryou.yaml` (Sample S8) · **Plugin:**
`examples/plugins/taste-engine.rhai` · **Test:** `foryou_sample.rs`

A channel where *what plays is chosen by a scorer plugin*, not authored. The **shape** is
fixed — two movies, then three episodes of one show, repeat — but the **content** in each
slot is ranked at generation time by what the server has actually been watching.

**Division of labor:** etv-station computes no taste of its own. It resolves the plugin's
catalog queries, gathers pooled watch history (Tautulli) + the channel's recently-aired tail,
hands it all to the Rhai script, and takes back an ordered list of `entry_id`s. Swap the
script → nothing in etv-station changes.

**Config shape:**
```yaml
window_days: 1
roll_interval: "60s"
retention_days: 1
scoring:
  recent_depth: 200
  nominal_item_secs: 5400
  attribution: true
rule:
  blocks:
    - mode: "all"
      program: { title: "For You" }
      pools:
        - name: movies
          plugin: "../plugins/taste-engine.rhai"
          select: round_robin
          advance: restart
        - name: shows
          plugin: "../plugins/taste-engine.rhai"
          select: round_robin
          rotate: visit
          advance: resume
          on_short: next
      pattern:
        - { pool: movies, take: 2 }
        - { pool: shows,  take: 3 }
```

- **Two pools, one script.** `sources()` splits the library by `type`; each pool draws its
  half (`ctx.pool` tells the script who's asking).
- **`advance` differs on purpose:** `movies` → `restart` (re-rank fresh each pass); `shows` →
  `resume` (a surfaced show progresses through its episodes across days, keyed on `show_id`).
- **Short window on purpose** so the ranking is at most hours stale when it airs.
- **No `constraints: no_repeat_within`** — the plugin already suppresses recent airings, and
  the config floor + script suppression *do not layer*.
- **Whose taste:** no `taste_scope` → `all_users` (house channel). A personal one is a
  *separate file* with `taste_scope: single_user` + `user:`, not a fan-out.
- **`attribution: true`** appends "Watched recently by \<names\> and N others" to guide +
  overlay (meaningful here because history is the whole house).

**The ranking algorithm (taste-engine.rhai):** per candidate —
1. Recently aired within `replay_ttl_days` (30) → hard **zero** (out).
2. Else base `1.0`, plus boosts: watch affinity `(1 − age/window) × 3` within
   `affinity_window_days` (14, episodes inherit the show's affinity); freshness `+0.5` for
   `year >= 2024`; easy-entry `+0.25` for `season == 1`.
3. Sort by score desc, `entry_id` tiebreak (deterministic).
4. Emit score > 0; if everything is suppressed, play the stalest thing (never go off air);
   truncate to `target_count + 10`.

Two performance rules baked in (measured): build `entry_id`-keyed maps *before* the item loop
(a linear scan inside was ~101M steps / 6m34s on an 84,722-item library), and keep per-item
scoring **inline** (Rhai passes by value; helpers cloned all of `ctx` — 124s vs 13s inline).

**Needs to run:** a populated catalog (`catalog_path`) or it resolves to nothing; a reachable
Tautulli or ranking degrades to freshness/season signals only.

> Note: this is a **sample** config, not one of the 58 production channels deployed on Unraid.

---

## 12. "Can I build it today?" cheat sheet

| You want… | Today? | With |
|---|---|---|
| Hand-picked file list in fixed order | ✅ | `kind: item`, block, `order: manual` |
| "All episodes of X in season/episode order" | ✅ | `kind: query` + `order: "season:asc,episode:asc"` |
| Franchise/collection in curated order | ✅ | `kind: collection` (Plex-arranged) |
| Cross-show query (genre/cast/year/rating/library) | ✅ | CEL `query` over `item.*` |
| Folder-based sourcing (bumpers vs commercials) | ✅ | `item.fs_dir == "bumpers"` |
| "1 movie then 3 episodes, repeat" | ✅ | pools + `pattern` |
| "Random season, played end-to-end" | ✅ | `group_by: season` + `take: all` + `select: random` |
| Seeded shuffle | ✅ | `order: random` + channel `seed` |
| No back-to-back repeats / spread shared cast | ✅ | `[constraints]` |
| Taste/recommendation ranking | ✅ | pool `plugin:` (Rhai scorer) |
| Fixed-time grid ("Tue 8pm = X") | ❌ not yet | designed, not implemented (roadmap "Later") |
| Live event injection / one-shot override window | ❌ not yet | roadmap "Later" |
| Web UI editing | ❌ | out of scope |

The two ❌ rows are the most likely places you'd need to **expand the markup**. Everything
above them is supported today.

---

## 13. Link index (all on `main`)

**Reference docs**
- Config + query schema reference — https://github.com/McBrideMusings/etv-station/blob/main/docs/schema.md
- PRD — https://github.com/McBrideMusings/etv-station/blob/main/docs/PRD.md
- Architecture — https://github.com/McBrideMusings/etv-station/blob/main/docs/architecture.md
- Roadmap (shipped vs planned) — https://github.com/McBrideMusings/etv-station/blob/main/docs/roadmap.md

**Query language (CEL)**
- Query-test crate (CEL harness) — https://github.com/McBrideMusings/etv-station/tree/main/crates/etv-query-test
- CEL evaluator (operators/functions) — https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-query-test/src/cel_eval.rs
- Cases: [01 TOS marathon](https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-query-test/cases/01-tos-marathon.toml) · [02 multi-Trek](https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-query-test/cases/02-multi-trek.toml) · [03 TNG s3–5](https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-query-test/cases/03-tng-seasons-3-5.toml) · [04 bumper block](https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-query-test/cases/04-bumper-block.toml) · [05 Dragon Ball](https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-query-test/cases/05-dragon-ball-chronological.toml) · [06 Trek in-universe](https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-query-test/cases/06-trek-in-universe.toml)

**Config markup — source of truth (serde structs)**
- All config structs — https://github.com/McBrideMusings/etv-station/tree/main/crates/etv-station/src/config
- entry.rs — https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-station/src/config/entry.rs
- pool.rs — https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-station/src/config/pool.rs
- order.rs — https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-station/src/config/order.rs
- mode.rs — https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-station/src/config/mode.rs
- filter.rs — https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-station/src/config/filter.rs
- constraints.rs — https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-station/src/config/constraints.rs
- source.rs — https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-station/src/config/source.rs
- rule.rs — https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-station/src/config/rule.rs
- station.rs — https://github.com/McBrideMusings/etv-station/blob/main/crates/etv-station/src/config/station.rs

**Sample configs (copy-and-adjust)**
- All samples — https://github.com/McBrideMusings/etv-station/tree/main/examples
- station.yaml — https://github.com/McBrideMusings/etv-station/blob/main/examples/station.yaml
- For You — https://github.com/McBrideMusings/etv-station/blob/main/examples/samples/foryou.yaml
- taste-engine.rhai — https://github.com/McBrideMusings/etv-station/blob/main/examples/plugins/taste-engine.rhai
- Trending Mix — https://github.com/McBrideMusings/etv-station/blob/main/examples/samples/trending-mix.yaml
- Trending Shuffle — https://github.com/McBrideMusings/etv-station/blob/main/examples/samples/trending-shuffle.yaml
- Kung Fu (pool constraints) — https://github.com/McBrideMusings/etv-station/blob/main/examples/samples/kungfu.yaml
- LotR (query) — https://github.com/McBrideMusings/etv-station/blob/main/examples/samples/lotr.yaml
- LotR theatrical (edition filter) — https://github.com/McBrideMusings/etv-station/blob/main/examples/samples/lotr-theatrical.yaml
- Dragon Ball — https://github.com/McBrideMusings/etv-station/blob/main/examples/samples/dragonball.yaml
- Ghibli — https://github.com/McBrideMusings/etv-station/blob/main/examples/samples/ghibli.yaml
- Marathon — https://github.com/McBrideMusings/etv-station/blob/main/examples/samples/marathon.yaml
- Lavfi test channel — https://github.com/McBrideMusings/etv-station/blob/main/examples/channels/lavfi-test.yaml
- Blocks — https://github.com/McBrideMusings/etv-station/tree/main/examples/blocks

**Overlays / graphics**
- Overlay renderer crate — https://github.com/McBrideMusings/etv-station/tree/main/crates/etv-overlay
- Overlay sample configs — https://github.com/McBrideMusings/etv-station/tree/main/examples/overlays
- Rhai overlay scripts — https://github.com/McBrideMusings/etv-station/tree/main/crates/etv-overlay/fixtures/scripts
