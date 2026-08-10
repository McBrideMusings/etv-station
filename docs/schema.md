# Config schema

The reference for the YAML configuration that drives the station: the
**station** file, **channel** files, and **block** files. Every field here maps
to a serde struct under `crates/etv-station/src/config/` — that source is the
final authority; this page is the human-readable index of it.

## One data model

All three config levels — the **station** file, **channel** files, and
path-referenced **block** files — deserialize into the same serde types. This
page shows every example in **YAML**, the format the project standardizes on.

> TOML is still accepted: the loader picks the parser by file extension
> (`.yaml`/`.yml` → YAML, anything else → TOML), and the serde types are
> identical either way, so a `.toml` file with the same fields loads the same.
> New config should be YAML.

| File | Holds |
|---|---|
| `station.yaml` | timezone + list of channels |
| `channels/*.yaml` | playout window + rule that composes blocks |
| `blocks/*.yaml` | `program` defaults + `entries` list |

## Block file

The unit of reuse. A block file is program defaults + a duplicates policy + a
flat list of entries. Source: `config/block.rs` (`BlockFile`).

| Key | Required | Type / values |
|---|---|---|
| `program` | no | [`ProgramMetadata`](#programmetadata) — block-wide defaults |
| `duplicates` | no — default `collapse` | `collapse` \| `keep` |
| `entries` | **yes** | list of [`Entry`](#entry) |
| `fallback` | no | [`Fallback`](#fallback) — resolved instead of `entries` when `entries` resolves to nothing |

```yaml
# blocks/starwars-timeline.yaml
program:
  title: "Star Wars: Timeline Order"
  categories: ["Movie"]

entries:
  - kind: item
    source:
      kind: local
      path: "${ETV_TEST_MEDIA_DIR}/movies/Star Wars (1977) {imdb-tt0076759}/Star.Wars.1977.mkv"
    program:
      title: "A New Hope"
      year: 1977
```

`${ETV_TEST_MEDIA_DIR}` and other `${VAR}` references in source paths are
expanded from the environment at load time.

## Entry

Every entry is tagged by a `kind` field. Four kinds. Source: `config/entry.rs`.

### `kind: item` — an authored file

| Field | Required | Type |
|---|---|---|
| `source` | **yes** | [`Source`](#source) |
| `in_point` | no | duration — trim start (`"90s"`, `"1m30s"`) |
| `out_point` | no | duration — trim end |
| `program` | no | [`ProgramMetadata`](#programmetadata) — overrides block defaults for this item |

Identity is **derived from the `source`, never authored** — a local file from a
canonical hash of its path (root-stripped via the station `source_roots`), a
`lavfi`/`http` source from its defining field. That derived id drives within-block
duplicate collapse and the regeneration anchor, so two inline items pointing at
the same file collapse to one. (Collapsing a manual item against a catalog
`query` result for that same file is future work — it needs the catalog ingester
to assign the file a matching id.) There is no `id` field to set.

An `http` source's identity is its **`uri` alone** — `headers` and `user_agent` are
deliberately excluded. Two `http` items with the same URI collapse to one under the
default `duplicates: collapse`, even if their headers differ. This is intentional:
identity feeds the play-history ledger and the resume cursors, and headers are where
rotating credentials live, so folding them in would give an item a new identity every
time its token was refreshed and lose its position and history with it (#99).

```yaml
- kind: item
  source:
    kind: local
    path: "${ETV_TEST_MEDIA_DIR}/movies/Die Hard (1988) {imdb-tt0095016}/Die.hard.mkv"
  program:
    title: "Die Hard"
    description: "John McClane vs. terrorists at Nakatomi Plaza on Christmas Eve."
    categories: ["Movie"]
```

### `kind: query` — resolve against the catalog

Instead of listing files, a query entry resolves a CEL expression against the
catalog and expands to the matching items.

| Field | Required | Type |
|---|---|---|
| `query` | **yes** | CEL string over `item` |
| `order` | no | [`Order`](#order) — how to sort the matches |

```yaml
- kind: query
  query: 'item.title.contains("Lord of the Rings")'
  order: "release_date:asc"
```

A string comparison treats a missing value as the empty string, so a film with
no `edition` counts as theatrical: `item.edition != "Extended Edition"` matches
it, and `item.edition == ""` selects exactly the no-edition items.

#### Item fields

Each field is either a column on the catalog's `entries` table (scalar —
compared with `==`, `!=`, `in`, and for text `contains` / `startsWith` /
`matches`) or a tag namespace (multi-valued — membership only, via `contains`).
`source`, `collections`, and `fs_dir` are membership over a related table, and
take `==` only — the one comparison that reads as "has one of".

| Field | Kind | Notes |
|---|---|---|
| `title` | string | |
| `show` | string | the series name on an episode |
| `type` | string | `movie`, `episode`, `bumper`, … |
| `content_rating` | string | |
| `edition` | string | empty = theatrical |
| `studio` | string | one production company |
| `library` | string | the library the item came from, by name |
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
| `source` | membership | `item.source == "plex"` |
| `collections` | membership | by collection name |
| `fs_dir` | membership | `item.fs_dir == "bumpers"` — the folder a file sits in |

`library` is what scopes a channel to one library when a server keeps several:
`item.library == "4K Movies"` selects that library and excludes "Movies" and
"Kids", where `item.type == "movie"` matches all three together. An expression
that does not mention `library` spans every library, which stays the default.

It stores the library's **name**, not its internal id, so the expression reads as
what it is. The cost: renaming the library in Plex breaks every expression naming
the old name, with no error — the expression resolves to nothing and the channel
goes quiet. Items from a source with no library concept (everything the
filesystem scan writes) have no `library`, so no `item.library == "…"` matches
them.

`fs_dir` is the immediate folder a file sits in — `item.fs_dir == "bumpers"`
selects everything under a folder named `bumpers`, which is how a filesystem-only
station separates its bumpers from its commercials without any metadata source.
It is worked out from the item's stored file paths every time the query runs,
never recorded, so drag a file from `bumpers/` into `commercials/` and the next
scan is all it takes: the item answers to `commercials` and stops answering to
`bumpers`. An item that exists as more than one file — the same movie kept at two
resolutions in two folders — matches either folder, because both are true.

Only the last folder in the path counts: a file at `/media/library/bumpers/x.mkv`
matches `"bumpers"` and not `"library"`.

### `kind: collection` — play a catalog collection in its authored order

Emits every member of one collection in the sequence hand-arranged in the source
app (`collection_items.position`). Re-ordering is a drag plus a re-ingest; the
config does not change.

| Field | Required | Type |
|---|---|---|
| `name` | **yes** | the collection's name as its source names it |

```yaml
- kind: collection
  name: "Halloween Marathon"
```

There is no `order` here, and no `order: "collection"` anywhere. A collection's
sequence belongs to the (collection, item) pair, not to the items, so once a
block flattens its entries into a set of ids nothing can say which collection's
positions to read. The entry emits an already-ordered run instead, which the
block's default `manual` order preserves.

The entry must name exactly one collection: an ambiguous name and an empty
collection are both config errors. For membership *without* the order — a
collection as a set to filter or shuffle — use a `query` entry with
`item.collections.contains("…")`. One stored structure, two read paths.

### `kind: include` — pull in another block file

| Field | Required | Type / default |
|---|---|---|
| `block` | **yes** | path to another block file |
| `mode` | no — default `all` | [`Mode`](#mode) |
| `order` | no — unset takes the episode default (#95); see [`Order`](#order) |
| `filter` | no | [`Filter`](#filter) |

```yaml
- kind: include
  block: "../blocks/bumpers.yaml"
  mode:
    count: 1
```

## Source

The `source` on an `item` entry, tagged by `kind`. Source: `config/source.rs`.

| `kind` | Fields |
|---|---|
| `local` | `path` (string) |
| `lavfi` | `params` (string — an ffmpeg lavfi graph, e.g. `testsrc`) |
| `http` | `uri` (string), `headers` (opt list of strings), `user_agent` (opt string) |

```yaml
# local
source:
  kind: local
  path: "/data/media/movies/Example (2020)/Example.mkv"

# lavfi
source:
  kind: lavfi
  params: "testsrc=size=1280x720:rate=30"

# http
source:
  kind: http
  uri: "https://example.com/stream.mp4"
  headers: ["Authorization: Bearer TOKEN"]
  user_agent: "etv-station"
```

## ProgramMetadata

The metadata written into each playout item's `program` block (populates
ETV-next's XMLTV). Defined in the `etv-next/` submodule
(`ersatztv_playout::playout::ProgramMetadata`). Every field is optional.

| Field | Type |
|---|---|
| `title` | string |
| `sub_title` | string |
| `description` | string |
| `season` | int |
| `episode` | int |
| `categories` | list of strings |
| `content_rating` | string |
| `artwork_url` | string |
| `year` | int |

Set on a block's `program:` for defaults; set on an entry's `program:` to
override per item. Item values win over block defaults.

## Value types

### Order

A string. Source: `config/order.rs`.

| Value | Meaning |
|---|---|
| `manual` | keep authored order |
| `random` | shuffle (seeded by the channel `seed`) |
| `field:dir,...` | sort by one or more fields; `dir` is `asc` or `desc` |

`order` itself is optional everywhere it appears, and an **unset** `order` is
not the same as an authored `manual` (#95): a
[block include's or `kind: include`'s](#composing-blocks-—-rule-blocks) `order`
left unset resolves to `season:asc,episode:asc` when every item the block
resolved is catalog type `episode`, and to authored order otherwise — an
author who writes `order: "manual"` explicitly always keeps authored order,
regardless of item type. A `query` entry's or pool's `order` left unset simply
applies no sort, leaving the catalog's or plugin's own order.

Every value is computable from the items being ordered. Two former values were
not, and are rejected by name at load rather than silently read as a field sort:

- `collection` (#107) — a collection's authored sequence belongs to the
  (collection, item) pair, so it lives on
  [`kind: collection`](#kind-collection-—-play-a-catalog-collection-in-its-authored-order).
- `score` (#108) — needed a scoring plugin. Scoring landed instead as
  [a pool's `plugin`](#pool-plugin-—-items-chosen-by-a-scorer-script) (#74):
  picking the candidates and ranking them turned out to be the same judgment,
  so it replaces a pool's `expr`, not its `order`.

A bare field name defaults to ascending. Examples: `release_date:asc`,
`season:asc,episode:asc`, `year:desc`. Invalid directions are rejected at load.

### Mode

How many items the block contributes. Source: `config/mode.rs`.

| Value | Meaning |
|---|---|
| `all` *(default)* | every resolved item |
| `count: N` | first `N` items (a map under `mode:`) |

```yaml
mode: "all"
# or
mode:
  count: 3
```

### Filter

Narrow the resolved item list. Source: `config/filter.rs`. Unknown fields are
rejected.

| Field | Type |
|---|---|
| `seasons` | list of ints |
| `episode_ids` | list of strings |

```yaml
filter:
  seasons: [1, 2]
  episode_ids: ["star-trek-s01e01", "star-trek-s01e02"]
```

### Duplicates

Block-level dedupe policy, keyed on each item's **derived** source identity (see
[`kind: item`](#kind-item-—-an-authored-file)) — so two entries resolving to the
same physical file collapse regardless of how they entered the block. Source:
`config/block.rs`.

| Value | Meaning |
|---|---|
| `collapse` *(default)* | drop repeats of the same derived identity |
| `keep` | keep every occurrence |

### Fallback

Resolved **instead of** a block's `entries` when `entries` resolves to nothing
eligible — a 24/7 channel must not dead-air or error just because a query
matched nothing this generation (an empty Plex collection surfaces here as a
`query` that matches zero items). Optional and opt-in: a block with no
`fallback` still resolves to empty exactly as it did before this field
existed. Source: `config/entry.rs` (`Fallback`).

Tagged by `kind`, same as [`Entry`](#entry) — two kinds:

| `kind` | Fields |
|---|---|
| `query` | same as [`kind: query`](#kind-query-—-resolve-against-the-catalog): `query` (**yes**), `order` (no) |
| `item` | same as [`kind: item`](#kind-item-—-an-authored-file): `source` (**yes**), `in_point` / `out_point` / `program` (no) |

```yaml
entries:
  - kind: query
    query: 'item.collections.contains("Now Airing")'
fallback:
  kind: query
  query: 'item.type == "movie"'
  order: "random"
```

```yaml
entries:
  - kind: query
    query: 'item.collections.contains("Now Airing")'
fallback:
  kind: item
  source:
    kind: local
    path: "${ETV_TEST_MEDIA_DIR}/standby/please-stand-by.mkv"
```

The fallback resolves through the exact same code path as a primary entry —
`kind: query` runs the same CEL grammar and carries its own `order`, exactly
like a primary query entry; `kind: item` resolves exactly like a primary item
entry (always exactly one item). Once resolved, it goes through the block's
own `duplicates` / `order` / `mode` exactly like `entries`' output would.

Entries-block only: a pattern block (`pools` + `pattern`) already gives each
pool its own empty-pool policy via `on_short`, so a pattern block declaring
`fallback` is rejected at load.

## Station file

Top-level registry. Source: `config/station.rs`.

```yaml
# station.yaml
tz: "America/Chicago"          # IANA time zone; default "UTC"
output_base: examples/output   # base dir every channel writes under

channels:                      # literal paths or globs, relative to this file
  - channels/starwars.yaml
  - channels/diehard.yaml
  - channels/*.yaml            # a glob works too — expands to every match

source_roots:                  # optional — media mount roots, daemon's view
  - /data/media

catalog_path: /var/lib/etv-station/catalog.db   # optional — enables query channels
catalog_refresh_secs: 900      # optional — trust the catalog this long without asking Plex
full_sweep_after_secs: 86400   # optional — force a full (deletion-catching) re-read this often
```

| Field | Required | Type / default |
|---|---|---|
| `tz` | no — default `UTC` | IANA time zone string; `ETV_STATION_TZ` overrides at runtime |
| `output_base` | **yes** | path — base directory every channel writes under; `ETV_STATION_OUTPUT_BASE` overrides at runtime |
| `channels` | **yes** | list of path strings; each is a literal path or a glob (`*`, `?`, `[`) |
| `source_roots` | no — default empty | list of media mount roots (the daemon's filesystem view) used to canonicalise a local item's path when deriving its identity, so the same file under different mounts is one identity. Empty just skips root-stripping. `ETV_STATION_SOURCE_ROOTS` (colon-separated) overrides at runtime — the intended way to supply them, since mount paths are host-specific and do not belong in a committed config. |
| `catalog_path` | no — default unset | path to the sqlite catalog the daemon opens and ingests (local-FS over `source_roots`, plus Plex when `PLEX_URL`/`PLEX_TOKEN` are set) at startup. Enables `query` entries and non-`manual` order, and lets a manual `local` item path-match onto a catalog identity (so it collapses with a query for the same file). Unset keeps the catalog-free behavior — only inline-item `manual` channels resolve. `ETV_STATION_CATALOG` overrides at runtime. |

| `catalog_refresh_secs` | no — default `900` | seconds a freshly ingested catalog is trusted without contacting Plex at all. A restart inside this window reuses the sqlite file as it stands, which is what makes an edit-restart loop cheap. `0` re-checks Plex on every start. |
| `full_sweep_after_secs` | no — default `86400` | seconds before a delta ingest is escalated to a full re-read. A delta asks Plex only for records touched since the last pass and therefore cannot express a *deletion* — an item removed from the library simply stops being mentioned. Only a full pass notices those. `0` disables delta ingest: every pass is full. |

**How the three ingest modes are chosen.** At startup the daemon compares the
catalog's recorded last-ingest time against the two knobs above. Age below
`catalog_refresh_secs` → skip, no HTTP at all. Age at or beyond
`full_sweep_after_secs` (or no prior ingest, or a clock that moved backwards) →
full re-read. Anything between → delta: each library section is queried with
`updatedAt>=<last ingest>`, and a collection whose own `updatedAt` predates the
cursor skips its per-collection children request. The full-sweep check is
applied *before* the refresh window, so a constantly-restarted station still
gets its periodic deletion-catching pass. The timestamp is recorded inside the
ingest transaction and taken before the fetch begins, so a failed pass never
advances the cursor past changes it did not write.

Each entry in `channels` is resolved relative to the station file's directory. A
glob expands to every matching file (matching nothing is an error); a literal
path is taken as-is. Files matched by more than one entry appear once. A
channel's **output folder is derived** — `{output_base}/{identity}`, where
`identity` is the channel's `name` override (below) or, if unset, its config
file's stem (e.g. `diehard.yaml` → `diehard`).

## Channel file

Defines one channel's playout window and the rule that composes blocks. Source:
`config/channel.rs` (`ChannelConfig`).

| Field | Required | Type / default |
|---|---|---|
| `name` | no — default: config file stem | string — channel identity override; drives the log label, overlay handshake, and output folder leaf. Must not contain path separators. |
| `window_days` | no — default `1` | int — how far ahead the schedule is written, and the span one generation is allowed to cover |
| `chunk_hours` | no — default `6` | int — playout file size only; it does not bound a generation |
| `roll_interval` | no — default `"3600s"` | duration |
| `retention_days` | no — default `7` | int |
| `seed` | no | int — seeds `random` order |
| `overlay` | no | `{ config, fifo_path? }` |
| `rule` | **yes** | `{ blocks: [...] }` — see below |

### Composing blocks — `rule.blocks`

Each entry under `rule.blocks` is a **block include** (`config/rule.rs`,
`BlockInclude`). It either **references a block file** or **inlines the block
body**, and carries the composition fields `mode` / `order` / `filter`. Unknown
fields are rejected.

**Reference form** — body lives in a separate file:

```yaml
# channels/starwars.yaml — no output_folder; identity is the file stem "starwars",
# so it writes to {output_base}/starwars

rule:
  blocks:
    - block: "../blocks/starwars-timeline.yaml"
      mode: "all"
      order: "manual"
```

**Inline form** — body lives in the channel file:

```yaml
# channels/lotr.yaml — identity "lotr" from the file stem

rule:
  blocks:
    - mode: "all"
      order: "release_date:asc"
      program:
        title: "The Lord of the Rings"
        categories: ["Movie", "Fantasy"]
      entries:
        - kind: query
          query: 'item.title.contains("Lord of the Rings")'
```

The two forms are interchangeable: at load, a referenced file's body
(`program` / `duplicates` / `entries` / `fallback`) is copied into the include,
so a reference and an equivalent inline block resolve identically. `mode`,
`order`, and `filter` are **composition fields on the include** — they never
live in the block file body.

### Pool `plugin` — items chosen by a scorer script

A pool normally names an `expr`, a CEL expression the catalog resolves. It can
instead name a `plugin`: a Rhai script that runs its own queries, ranks what it
finds, and returns the ordered set. The two are mutually exclusive — a pool that
sets both, or neither, fails at load.

```yaml
pools:
  - name: foryou
    plugin: "../plugins/taste-engine.rhai"
    select: round_robin
    advance: resume
```

Everything else about the pool is unchanged: `select`, `rotate`, `advance`,
`on_short`, and the pattern's `take` treat the returned list exactly as they
treat a CEL-resolved one. There is no `order` on a plugin pool — the script
returns its set already ranked, and sorting it again would discard the ranking,
so the pair is rejected at load.

**Replay is the plugin's business, unless the pool claims it.** ETV computes no
replay policy of its own here. It hands the script `ctx.recent` and takes back
whatever order comes out, so whether the same title can air two generations
running is entirely a property of the script: one that suppresses what it
recently returned holds a title back for as long as its own policy says, one
written without suppression hands back its same top-ranked item every time.
With `advance: restart` nothing else
in the config stops that — the result is a valid schedule that plays one film
forever. Swapping the script swaps that behavior, and nothing in the YAML says
which kind you have.

[`constraints` on the pool](#pool-constraints-—-spacing-counted-in-a-pool-s-own-draw-order)
is the channel author's own floor. `no_repeat_within: N` is applied to the
ordered list the plugin returned, inside pool resolution and before the pattern
draws from it, so it holds whatever the script does or fails to do.

It is opt-in on purpose, and it is not a belt-and-braces addition on top of a
script that already suppresses — **the two do not layer.** A no-repeat rule
marches the pool forward through its returned set rather than letting it revisit
recent items, which leaves the script's own `ctx.recent` suppression less to hold
back over the window a generation covers. Set it on a plugin pool when
the config is the only thing guarding replay; leave it unset when the script is.
`examples/samples/foryou.yaml` ships a scorer that suppresses and therefore
declines the field; `examples/samples/kungfu.yaml` is the sample that exercises
it, on CEL pools.

The script defines four functions:

```rhai
// Which hooks this script implements. Read at config load time, before the
// catalog exists and without running anything below.
fn hooks() { ["pool_provider"] }

// Which host capabilities this script needs (#167). Read at the same time,
// alongside hooks(). Omitting capabilities() declares none.
fn capabilities() { ["catalog_read", "watch_history"] }

// Every catalog query this plugin reads, named. Run once, up front, so a
// malformed expression fails before any ranking work.
fn sources() {
    #{ movies: `item.type == "movie"` }
}

// Returns entry_ids, most-wanted first — or, per entry, a record widening
// what a bare id can say (#166): `#{ entry_id: "…", metadata: #{…}, take: 3 }`.
fn pick(ctx) { … }
```

#### The record shape — `metadata` and a per-entry `take` (#166)

Each element `pick()` returns may be a bare `entry_id` string — unchanged
since #74 — or a record naming one plus two optional extras:

```rhai
#{ entry_id: "ghost", metadata: #{ reason: "won an Oscar" }, take: 3 }
```

`metadata` is opaque, exactly like [`Pool::config`](#pool-config-—-the-scripts-own-tunables-authored-in-yaml):
the station converts it and carries it untouched to that airing's entry in
the emitted playout JSON (`PlayoutItem::metadata`), reading nothing out of
it. A non-finite float anywhere inside — `.inf`, `-.inf`, `.nan` — fails the
generation and names the key, the same refusal `Pool::config` makes at load.
An entry with no `metadata` emits no `metadata` key at all. Two plugin pools
in one block that both resolve the same id and both attach metadata collide
silently — the same 'blind across pools' limit `constraints` already has;
pools that must not collide have to be disjoint by construction.

`take` overrides the pattern step's own `take` for that entry's series — a
"For You" pool asking an unseen show to sample its first three episodes
while a watched one plays its full run. It travels through pool resolution
to the pattern draw, which reads it in place of the step's own `take` for
that series (#173) — every other id in the step still spends the step's own
`take`, unchanged. An override is only honoured under `rotate: "visit"`;
a `rotate: "slot"` draw never asks for the step's own `take` in the first
place (each slot is always one item), so there is nothing for an override
to replace there. Zero or negative fails the generation, naming the entry.

The widening is additive: a script returning only bare ids, like `taste-engine.rhai`, needs no edit. A `sequencer:` block's plugin pool record shape parses the same way as a `pattern:` block's, and its `metadata` reaches the emitted JSON identically (#201). The per-entry `take` override remains out of scope for a sequencer block: `arrange()` already decides its own order, so what a `take` override would even mean there is a separate design question, not a plumbing gap (#201).

#### `hooks()` — what a script says it can do

A plugin declares its hooks and the station wires only the declared ones
(#159). Two names exist:

| Hook | What a script implementing it does |
|---|---|
| `pool_provider` | supplies a pool's items. This is what a `plugin:` pool has always meant, so a script named there must declare it. |
| `sequencer` | takes a block's resolved pools plus the generation window and emits the block's final timeline in place of the pattern walk (#169). A block names one under `sequencer:`; see [Block `sequencer`](#block-sequencer-a-plugin-arranges-the-block-itself). |

A script may declare one or both. The declaration lives in the script, not in
the channel config, so swapping one scorer for another needs no YAML edit.

`hooks()` is read by compiling the script and calling that one function —
`sources()` and `pick()` never run — so all three refusals below happen at
config load rather than mid-generation:

- A `plugin:` pool naming a script that does not declare `pool_provider` is
  refused, naming the script and the hook it lacked:
  `pool "movies" names plugin …/taste-engine.rhai via `plugin:`, which requires
  the plugin to declare the `pool_provider` hook, but it only declares: sequencer`
- A script declaring a name outside the table is refused, and the message lists
  the names that exist: `declares unknown hook "warp_drive" — known hooks are:
  pool_provider, sequencer`
- A script declaring an empty array is refused: `declares no hooks — a plugin
  must declare at least one of: pool_provider, sequencer`

#### `capabilities()` — which host inputs a script needs (#167)

Nothing is available to a script ambiently beyond `ctx.pool`, `ctx.config`,
`ctx.target_count`, `ctx.now`, and `ctx.recent` — the inputs every plugin pool
has always received. Two more are gated behind a declaration, and a third is
opened only by name:

| Capability | Gates | Declared as |
|---|---|---|
| `catalog_read` | `ctx.sets` — the items each `sources()` query matched | `"catalog_read"` |
| `watch_history` | `ctx.history` — recent server-wide watch events | `"watch_history"` |
| a named external datastore | nothing in this slice exposes it to the script — the grant only proves at load time that its location opens (#167; what is reachable through it is #181, out of scope here) | `#{ datastore: "name" }` |

Unlike `hooks()`, `capabilities()` is optional — a script with no such function
declares nothing, which is the right answer for a plugin that only reads the
ambient inputs. `capabilities()` is read the same way `hooks()` is: compiling
the script and calling that one function alone, so `sources()` and `pick()`
never run during this check either.

The channel config grants capabilities on the pool, next to `plugin:` — see
[Pool `capabilities` / `datastores`](#pool-capabilities-datastores-—-granting-a-plugin-what-it-needs)
below. The two sides must agree exactly:

- A pool naming a script that declares a capability the pool's
  `capabilities:`/`datastores:` does not grant is refused at load, naming the
  plugin and the capability.
- A pool granting a capability the script never declares is refused at load,
  the other way round — a grant nobody asked for is a typo or a stale config,
  and silently ignoring it would hide either.
- A script that reaches for `ctx.sets` or `ctx.history` without the matching
  capability declared and granted fails the `pick()` call the moment it reads
  the field, naming the capability. This is the one check that can only happen
  at run time, because the reach is a script call — `capabilities()` and
  `pick()`'s actual body can disagree, and only running `pick()` catches it.
- A datastore grant naming a location that cannot be opened is refused at
  load, naming the datastore and the underlying error.

`examples/plugins/taste-engine.rhai` reads `ctx.sets` and `ctx.history`, so it
declares `["catalog_read", "watch_history"]`; `examples/samples/foryou.yaml`
grants both on each pool that points at it.

`ctx` carries `ctx.sets.<name>` (the items each source matched — every column on
`entries` plus genres / cast / labels / … as arrays; requires `catalog_read`),
`ctx.pool` (the name of the pool asking, so one script can serve several pools
of a channel — a `movies` pool and a `shows` pool ranked by the same taste),
`ctx.target_count` (how many items the generation needs), `ctx.history` (recent
server-wide watch events, `#{entry_id, watched_at}`; requires `watch_history`),
`ctx.recent` (what this channel aired most recently, oldest first), `ctx.now`
(unix seconds at generation time), and `ctx.config` (this pool's `config:`
block — see below).

### The determinism contract

A plugin must be a pure function of `(catalog, config, resume state, seed, external-store snapshot)` — the same inputs the station itself is pure over. Nothing in `ctx` is optional to use instead of an ambient read: `ctx.now` is unix seconds at generation time, handed in precisely so a script never has to call the clock itself, and `ctx.history` / `ctx.recent` are the only watch/replay state a script sees. A script that reads Rhai's own `timestamp()`/`elapsed()`, or ranks by anything else not reachable through `ctx`, produces a different schedule from the same inputs — and the schedule still looks completely valid, so nothing errors and nothing on screen says which run is "right".

`etv-station --check-determinism <channel>` generates the named channel twice from identical inputs (config, catalog snapshot, seed, and — the empty, stateless kind — resume state) and diffs the two resulting schedules. It reports identical, or the first airing position where they disagree and both entry ids there, so the failure names a place to start rather than a bare "not reproducible". It measures only: it does not fix a plugin that fails it, and it is a debug check, not part of normal generation.

### Writing a plugin that finishes

`pick()` runs once per generation, per pool, and the station waits for it. The
channel it belongs to airs nothing until it returns. Two things about Rhai decide
whether that is milliseconds or minutes, and neither is guessable from the
language:

**Search once, not once per item.** A loop over `ctx.sets.<name>` runs its body
for every item in the set — on a real library that is tens of thousands of times.
Anything inside it that walks `ctx.history` or `ctx.recent` looking for a match
multiplies the two: 84,722 items against 1,000 watch rows and a 200-deep aired
tail is about 101 million interpreted steps to choose four things, which measured
at 6 minutes 34 seconds. Build a map keyed by `entry_id` **before** the loop and
read it inside:

```rhai
let watched = #{};
for event in ctx.history { watched[event.entry_id] = event.watched_at; }
// …then inside the item loop: `let last = watched[item.entry_id];`
```

**A function call copies its arguments.** Rhai passes by value, so
`score_item(item, ctx)` clones the whole of `ctx` — the entire watch history and
aired tail — on every call. A few helpers per item is a few full copies per item:
the same scoring arithmetic measured 124 s split across helpers and 13 s written
inline in the loop. Keep per-item work in the loop body; keep named functions for
the things called once, like reading a tunable out of `ctx.config`.

`examples/plugins/taste-engine.rhai` is written to both rules and says so in its
comments.

### Named show groups — `groups:` on the channel and on a pool

Plex stores a franchise's spin-offs as unrelated shows — *RuPaul's Drag Race*
and *RuPaul's Drag Race All Stars* share no `show_id` — so a pool grouped or
rotated by show treats them as unrelated series. A **show group** makes their
union one rotation domain: declare it once on the channel, then have a pool
draw from it by name instead of an `expr` or `plugin`.

```yaml
groups:
  - name: rupaul
    shows:
      - "RuPaul's Drag Race"
      - "RuPaul's Drag Race All Stars"

rule:
  blocks:
    - pools:
        - name: rupaul
          groups: ["rupaul"]
          group_by: season
          bucket_order: "release_date:asc"
      pattern:
        - pool: rupaul
          take: all
```

`group_by: season` still cuts at each member show's own season boundaries —
`take: all` on the pattern step airs whichever season a visit picked, end to
end — but the series it produces now come from every member show, so
`rotate: visit` (the default) cycles the franchise: Drag Race season 9, then
All Stars season 3, then Drag Race season 10, rather than marching through
one show's whole run before starting the other's. `bucket_order` is what
decides that sequence — sort it by whatever field puts the seasons in the
order you want them to cycle; a show's own `season` number is not
comparable across different shows, so a franchise group needs a field that
is, such as a populated `release_date`. Without a `bucket_order`, seasons
come up in each show's own catalog order, one show fully before the next.

A member show is named by its Plex title, matched exactly against the
catalog's `show` column — the same string an `expr`'s `item.show` would
compare against. `groups` on a pool is a list because a pool may combine more
than one franchise (a general "Bravo" pool where RuPaul and Below Deck each
stay their own rotation domain); it is otherwise exactly like `expr` and
`plugin` — mutually exclusive with both, and a pool that sets two of the
three, or none, fails at load.

**No new field for a sibling's resume position.** Every pool's series already
seeds its resume cursor from a channel-wide ledger keyed on the item's own
`show_id` (#155) — grouping across shows changes nothing about that. A
sibling show coming back around resumes exactly where it left off; a season
nobody has started sits at its own top.

**Error cases**, both caught before the schedule is generated:

- A group naming a show with no episodes in the catalog fails, naming the
  show and the group.
- A pool combining two groups that share a member show fails, naming the
  show — its episodes would otherwise belong to two rotation domains at
  once. (A show may belong to two groups; only combining both in *one* pool
  is rejected.)

### Pool `config` — the script's own tunables, authored in YAML

A plugin pool may carry a `config:` block. **The station passes it to the script
verbatim and never reads it.**

```yaml
pools:
  - name: movies
    plugin: "../plugins/taste-engine.rhai"
    config:
      affinity_window_days: 30
      weights:
        affinity: 3.0
        nested: [1, 2.5, true, "mixed"]
```

Any YAML shape is accepted — maps, arrays, strings, numbers, booleans, nested to
any depth — and it arrives as `ctx.config` with its structure and scalar types
intact. No key is reserved.

This is what lets two channels share one algorithm with different numbers, and
it is deliberately opaque: nothing here is validated, no key is known to ETV, no
default is injected, and an unrecognised key is not an error. **A key means
whatever the script decides it means.** `affinity_window_days` is not a concept
etv-station has — it exists because `examples/plugins/taste-engine.rhai` looks it
up, and a different scorer would read entirely different keys. That is the same
argument that put taste in the plugin at all ([ADR 0002](./adr/0002-scorer-plugin-replaces-a-pool-expr.md)):
a station that validated these keys would be a party to the taste it exists not
to hold, and its list would need updating for every script anyone writes.

An absent `config:` arrives as an empty map rather than a missing key, so a
script can read `ctx.config.anything` unconditionally and get unit back. Which
is also the catch: **a mistyped key is silent.** `afinity_window_days` reads as
unset and the script falls back to its own default, exactly as if it had been
omitted — there is no warning, because there is nothing that knows the correct
spelling. A script that wants strictness has to declare and check its own
expected keys. On an `expr` pool `config:` is ignored, on the same terms — except
for the one refusal below, which happens while the file is being parsed and so
does not know whether the pool has a script.

The worked example reads its two tunables this way, each falling back to the
value written in the script:

```rhai
fn tunable(ctx, key, fallback) {
    let v = ctx.config[key];
    if v == () { fallback } else { v }
}

fn affinity_window_days(ctx) { tunable(ctx, "affinity_window_days", 14) }
fn replay_ttl_days(ctx)      { tunable(ctx, "replay_ttl_days", 30) }
```

**This is the project-wide rule for script tunables, not a scorer quirk.** The
overlay renderer's TOML takes a `config` table on the same terms — arbitrary
nesting, nothing validated, absent means an empty map, typos silent — reaching
its Rhai script as a `config` constant rather than as `ctx.config`, because an
overlay script is evaluated against flat scope constants while a scorer receives
one `ctx` map. That is the only difference, and it follows from how each engine
is invoked rather than from a decision either side made.

```toml
# the overlay TOML named by a channel's `overlay.config`
script = "lower-third.rhai"

[config]
corner = "bottom-left"

[config.font]
family = "Inter"
size = 42
```

Both surfaces carry the bag in one and the same value type internally, so the
two cannot drift apart on what a shape means. That has one visible consequence:
**a TOML datetime reaches an overlay script as the text the author wrote.**
`date = 2026-07-28` arrives as `"2026-07-28"`, and offset, local, and time-only
values likewise arrive in TOML's own spelling. Nothing is dropped, and it is the
same string a channel YAML's `date: 2026-07-28` already hands a scorer plugin. A
script wanting a moment rather than a label parses it — the meaning of a key is
the script's, here as everywhere else in the bag.

It has one other consequence, and it is the single thing in the bag that can
fail: **a float that is not a finite number is refused at load, naming the key.**
`weight: .inf` in a pool's `config:` — or `-.inf`, `.nan`, and `inf`/`nan` in an
overlay's `[config]` — cannot be carried at all, and would otherwise reach the
script as unit while the author believed they had written a number. So the
channel or the spec fails to load instead, with an error like:

```
`config.weights.affinity` is `inf`, but a script config can only carry finite
numbers. Write a large finite number instead — `inf` and `nan` have no meaning
here, and a script would receive nothing at all.
```

The key is named in full, including through arrays and sub-tables
(`config.steps[1]`, `config.fade.weight`). Both surfaces refuse it in the same
words. This is not the station judging what a key means — the value has no
representation, so an author writing "never decays" writes a large finite number
and the script's own comparison does the rest.

Any future scripting surface follows the same shape: the station carries a bag
of values it does not understand, and the script decides what they mean.

The station computes no score of its own — it supplies those inputs and takes
back an ordered list, so swapping one script for another changes nothing in
etv-station. Why this rides on `expr` rather than on `order` is
[ADR 0002](./adr/0002-scorer-plugin-replaces-a-pool-expr.md).

A `plugin:` path is relative to the **channel config file's** directory, the
same as a `block:` include — never to wherever the daemon was launched from.
Absolute paths are used as written.

Four knobs sit on the channel, under `scoring:`, all optional:

| Field | Default | Meaning |
|---|---|---|
| `recent_depth` | `200` | How many recently-aired entries reach `ctx.recent`. A channel with a deep library wants a long memory; a narrow one would starve on the same setting. |
| `nominal_item_secs` | `1800` | Nominal seconds per item, used only to size `ctx.target_count`. A channel of half-hour episodes and one of three-hour films need different numbers to ask for a sensible amount. |
| `taste_scope` | `all_users` | Whose watch history `ctx.history` carries. `all_users` pools every Tautulli account with no user dimension; `single_user` narrows it to the one account named in `user`. |
| `user` | — | The account `single_user` follows: a Tautulli username (`"bob"`) or a numeric user id (`"1234567"`). Which one it is is inferred — a value made entirely of digits is sent as `user_id`, anything else as `user`. |
| `attribution` | `false` | Name who has been watching each item, in the guide and on screen. Off by default; see below. |

`target_count` is sized to **one chunk** (`chunk_hours`), not to the whole
window — a generation lays the returned list end-to-end, so a hint covering 30
days would push a single generation to materialize the whole month at once.

Watch history comes from Tautulli, configured by the `TAUTULLI_URL` and
`TAUTULLI_API_KEY` environment variables and never by tracked config. When
either is unset or Tautulli is unreachable, `ctx.history` arrives empty and the
generation proceeds — a script still has release dates, tags, and `ctx.recent`
to rank on, so an outage degrades the ranking rather than stopping the channel.

#### `taste_scope` — one channel per audience, not per user

`taste_scope` is a property of the **channel**, so a personal For You channel
sits beside the house one on the same station and neither knows about the other.
There is no fan-out: one config file still produces exactly one channel writing
to one `output_folder`. Two people means two channel files.

`single_user` requires `user`, and `user` is rejected under `all_users` — both
directions fail at load rather than at air, because both mistakes are silent
otherwise. A `single_user` channel with nobody named would quietly fall back to
the pooled history and rank a personal channel against everyone's viewing while
looking perfectly healthy.

What is *not* checked at load is whether the named account exists — that needs
the network, and this is a config pass. Tautulli answers an unknown user with an
empty history, which shows up at runtime as `rows=0` on that scope's
`tautulli.history` log line.

#### `attribution` — naming who has been watching

With `attribution: true`, each item the channel schedules gets a line appended to
its guide description:

```
A hobbit sets out.

Watched recently by bob, carol and dave
```

Up to three names, then `and N others`. A person who rewatched something is named
once — the line says who has been watching, not how many plays there were — and
within an item the most recent watchers are the ones that survive the cut. An
item nobody on record has watched gets no line at all, and an item's existing
synopsis is never replaced, only appended to.

It reaches two surfaces from that one field:

- **The guide.** `ProgramMetadata.description` is what ETV-next turns into XMLTV,
  so the line shows up as the programme description in Plex.
- **On screen.** The overlay parses the same playout JSON, so a Rhai overlay
  script gets `description` (the whole thing) and `watched_by` (the credit line
  on its own, empty when there isn't one). Gate a lower third on
  `watched_by != ""`.

Two things it cannot do. It says **"recently"** and never "this week", because
Tautulli's `get_history` has no `since` parameter — the rows reach back as far as
the last thousand reach. And it is only as fresh as the generation that wrote the
chunk: a chunk written now and aired in six hours carries six-hour-old
attribution, because there is no path from ETV-next back to the station to
refresh a chunk in place. A channel that wants it current wants a short
`window_days` and `chunk_hours`, as the For You sample already does for ranking.

`attribution` is opt-in rather than derived because turning it on publishes every
viewer's activity to everyone else watching that channel. On a shared server that
is a privacy decision, not a formatting one.

It is also a second reader of the watch history, alongside a scorer plugin — so a
channel with `attribution: true` and no `plugin:` pool still fetches history,
where before only a plugin could cause a fetch (#131).

History is fetched **once per distinct scope per refresh window**, not once per
channel. Three channels following the same person share one `get_history` call
and one catalog join between them; a station running the pooled channel plus one
each for two people makes three calls per window however many channels are
pointed at those three audiences. Each scope ages on its own clock, so a personal
channel refreshing does not drag the pooled one along with it.

### Pool `capabilities` / `datastores` — granting a plugin what it needs

A plugin declares the host inputs it needs with `capabilities()`; the channel
config grants exactly that set on the pool that names the script (#167).
Nothing is ambient — a pool that grants nothing gets a script with no
`ctx.sets` and no `ctx.history`.

```yaml
pools:
  - name: movies
    plugin: "../plugins/taste-engine.rhai"
    capabilities: [catalog_read, watch_history]
```

| Field | Type | Meaning |
|---|---|---|
| `capabilities` | list of strings | the simple capabilities granted — `catalog_read`, `watch_history` |
| `datastores` | list of `{ name, path }` | named external stores granted, one entry per `#{ datastore: "name" }` the script declares |

The check is **symmetric, and both directions fail the load**: a capability the
script declares that the pool does not grant, and a capability the pool grants
that the script never declares. The second one matters as much as the first —
an unasked-for grant is either a typo or a stale config, and quietly ignoring
it hides both.

A `datastores` entry's `path` is written as an env-var reference and expanded
at load, per the project rule that a location never appears literally in a
committed config:

```yaml
    datastores:
      - name: taste
        path: "${ETV_TASTE_STORE}"
```

The file is opened at load to prove it is reachable; a path that cannot be
opened fails the load naming the datastore and the underlying error, rather
than producing an empty pool that looks like a scheduling result. A station
whose channels grant no datastore never opens one at all.

Reaching for `ctx.sets` or `ctx.history` without the grant fails the `pick()`
call the moment the script reads the field, naming the capability — that one
can only be caught at run time, because the reach is a script call.

### Block `sequencer` — a plugin arranges the block itself

A pattern block interleaves its pools by walking the authored `pattern`. A
block can instead name a `sequencer:` script, which receives the block's
already-resolved pools plus the generation window and returns the block's
final ordered timeline (#169).

```yaml
rule:
  blocks:
    - pools:
        - name: prime
          expr: 'item.type == "movie"'
        - name: latenight
          expr: 'item.type == "episode"'
      sequencer: "../plugins/foryou-sequencer.rhai"
```

**`sequencer` and `pattern` are mutually exclusive** — a block authors one or
the other, and a block declaring both fails validation naming the block. This
matches the exclusivity rules already in the schema: a block is `entries` or
`pattern`, and a pool names exactly one of `expr`, `plugin`, or `groups`.

The station resolves the pools exactly as it does for a pattern block — each
pool's `expr` or `plugin`, then its `order`, `bucket_order`, and `constraints`
— and only then hands them over. The script sees:

| `ctx` field | What it holds |
|---|---|
| `ctx.pools.<name>` | that pool's resolved items, in order, as full item maps — `entry_id`, title, duration, show, season, tags. Not bare ids: a daypart script has to prefer items that fit the time left before its next boundary, and ids carry no durations. |
| `ctx.pool_config.<name>` | that pool's own `config:` block, passed through unread — the same generic carrier a scorer's `ctx.config` reaches (ADR 0002), not a new schema field. A daypart script's "which hours/weekdays does this pool claim" table lives here (#14, ADR 0004). Empty map when the pool declares no `config:`. |
| `ctx.window.from` | the instant this generation begins airing — the hour a daypart script asks about, not the hour the daemon happens to be computing in |
| `ctx.resume.<pool>.next` | the series whose turn is next in that pool's rotation |
| `ctx.cursor` | the per-show cursors from the play-history db |

`local_time(unix_secs)` is a **global function**, not a `ctx` field — registered on the engine every `arrange()` call runs under, so a script can re-read the clock at any instant it computes (a running cursor advanced by summed item durations), not only at `ctx.window.from`. It resolves against the station's configured `tz` (`UTC` at the stateless entry point, which carries no station config) and returns `#{ weekday, hour, minute }`, where `weekday` is one of `"mon"`.."sun"` and `hour` is `0`-`23` — both rolling on the same local-midnight grid `chunk_hours` does.

Resume state is **read-only input**. The script returns only a timeline, and
the station derives the new state from which items actually came back — the
same split ADR 0002 chose for scorers. That is how one script gives two pools
in one block different advance behaviour: a pool it draws from the top of
ignores the cursor it was handed, a pool it starts at the cursor resumes.

Error cases:

- An item in the returned timeline that is in none of the block's pools fails
  the generation, naming the item. The pools are the script's entire universe;
  returning outside them means it invented an airing.
- A timeline that does not fill the window falls through to the existing
  short-channel handling rather than leaving dead air.
- A timeline that overruns the window is truncated at the boundary, matching
  how the pattern walk is bounded.

Worked sample: `examples/plugins/foryou-sequencer.rhai`.

#### Dayparting (#14)

`examples/plugins/daypart-sequencer.rhai` is the reusable dayparting
sequencer — a network-mirror channel is **one block** whose pools are its
dayparts plus a default pool, all pointing `sequencer:` at this one script.
Nothing about which hours a pool claims lives in the schema; it is authored
entirely in that pool's `config:`:

```yaml
rule:
  blocks:
    - pools:
        - name: latenight
          expr: 'item.show in ["Rick and Morty", "The Boondocks"]'
          config:
            hour_start: 21   # local hour, 0-23
            hour_end: 6      # < hour_start wraps past local midnight
            weekdays: [sun, mon, tue, wed, thu, fri, sat]  # default: every day
        - name: default
          expr: 'item.type == "movie"'
          # No hour_start/hour_end: this is the pool that fills every hour
          # no daypart claims. Exactly one pool in the block must be it.
      sequencer: "../plugins/daypart-sequencer.rhai"
```

Boundaries drift rather than truncating an item or leaving a gap (#14
decision 3, ADR 0004): the script prefers, from the active daypart's own
pool, whichever item fits the time left before the next boundary; when
nothing fits, the next item plays anyway and the following daypart starts
late by that overrun. The exact grid is not the goal — see ADR 0004 for why
holding it exactly would need filler the library does not have. Two pools
whose declared hours and weekdays overlap fail `arrange()`, naming both.

### Pool `constraints` — spacing counted in a pool's own draw order

`constraints` (`no_repeat_within`, `separate_by`, `separate_min_gap`) is the same
table on a pool as on a block, but it is enforced in a different place. On an
**entries** block the pass runs last, over the channel's whole flattened list. On
a **pattern** block it is enforced entirely inside each pool, never over the
interleaved result:

```yaml
pools:
  - name: pile
    expr: 'item.genres.contains("Martial Arts")'
    constraints:
      no_repeat_within: 3     # three *draws from this pool*, not three aired items
  - name: jackie
    expr: 'item.cast.contains("Jackie Chan")'
```

**`no_repeat_within` has two spellings, and they mean different things.** A bare
number (`no_repeat_within: 3`) is **positional** — three list positions (on a
pool, three of that pool's own draws), full stop, whatever runs in between. A
quoted duration (`no_repeat_within: "24h"`) is **temporal** — the same
`entry_id` may not recur within that much wall-clock time, measured against the
emitted schedule's item runtimes rather than counted as items. `separate_by` /
`separate_min_gap` stay positional only; #185 gave the temporal spelling to
`no_repeat_within` alone.

The positional spelling means what it says on a uniform pool — a show whose
episodes all run the same length — where ten positions is also a fixed span of
time. It stops meaning that on a pool mixing durations: 22-minute episodes and
3-hour films in one pool give `no_repeat_within: 10` a real span anywhere from
three and a half hours to thirty, decided by whatever gets drawn. "Not twice in
a day" wants the temporal spelling instead:

```yaml
pools:
  - name: mixed
    expr: 'item.type == "movie" || item.type == "episode"'
    constraints:
      no_repeat_within: "24h"   # not twice in a 24-hour span, however that plays out in items
```

The temporal form is measured against each item's *estimated* runtime — the
same catalog-derived estimate (falling back to the mean of what is known, then
the channel's nominal item length) the station already uses to size a
generation — not a value probed to the second. It holds across the generation
seam the same way the positional form does, via the play-history ledger's
tail.

**The pattern's shape cannot move.** A pass over the finished list knows item ids
and gaps and nothing else — in particular not which pattern step an item came
from — so repairing a repeat swapped an episode into a movie slot and silently
destroyed the `2 + 3` shape the pattern was written to build. Keeping the rule
inside the pool means the interleave is never reordered at all.

**It is enforced at two moments,** because a pool makes repeats in two ways. Its
resolved list is ordered under the rule before the pattern runs — that settles
which item opens a window, the order the series rotate in, and the generation
seam. Then every draw is checked against what the pool just emitted. The second
is the one that usually bites: a query returns each item once, so the list has no
repeats to fix, while the draw loop makes them freely — the rotation keeps its
place on a series that only half-filled a visit, and a series played to its end
loops back to its start.

**The gap counts that pool's draws.** `no_repeat_within: 3` means three draws
from this pool, however much other content the pattern lays between them. The
seam is read the same way: the aired tail is narrowed to items this pool could
have supplied, and the station sizes the history it keeps to cover the
conversion.

**A pool is blind to every other pool.** Each pool is constrained against its own
draws alone, so if two pools in one block resolve the same `entry_id`, neither
pool's constraint sees the collision. Pools that must not collide have to be
disjoint by construction — `examples/samples/kungfu.yaml` does it with the pools'
own expressions, `examples/samples/foryou.yaml` with a plugin that splits on
`item.type` — because the pool contract does not guarantee it.

**Mind the window size.** A no-repeat rule on a pool the pattern draws heavily
from marches that pool forward instead of letting it revisit, so the wider
`window_days` is the further through its set one generation gets. That is the
rule doing its job — but on a channel whose replay policy lives elsewhere, such
as a scorer plugin suppressing what recently aired, it leaves that policy less to
hold back. `examples/samples/foryou.yaml` declines the field for exactly this
reason. An **authored** `cycles` is the case to watch: it is not bounded by the
window, so a large number there really can march a pool through its whole set in
one pass.

When no item can be drawn without a clash, one is drawn anyway and the shortfall
is logged as `constraints.unsatisfied` — a pool that cannot satisfy its own
constraint still has to put television on the air.

A block-level `constraints` on a pattern block stays legal and becomes the
**default every pool that declares none inherits**. A pool declaring its own
table **replaces** the block's wholesale rather than merging field by field, so a
pool's table always reads as the complete rule for that pool.

## Sample configs

The committed samples under `examples/` are authored in YAML:

| Sample | File |
|---|---|
| Station manifest | `examples/station.yaml` |
| Test channel (three lavfi items) | `examples/channels/lavfi-test.yaml` |
| The Lord of the Rings (query channel) | `examples/samples/lotr.yaml` |
| Trending Mix (pools + pattern interleave) | `examples/samples/trending-mix.yaml` |
| For You (taste-scored via a plugin) | `examples/samples/foryou.yaml` |
| Worked example scorer plugin | `examples/plugins/taste-engine.rhai` |
| Star Wars timeline block (8 items, manual order) | `examples/blocks/starwars-timeline.yaml` |
| Die Hard block (1 item) | `examples/blocks/diehard.yaml` |

Copy one and adjust the paths and metadata to author a new channel or block.
