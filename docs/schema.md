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

### Unknown keys

An unrecognised key is **ignored, not refused** — the config still loads and
the file parses as though the key did not exist.

Each unrecognised path is logged once per file at WARN with
`event = "config.unknown_keys"`, fields `path = <config file>` and
`keys = <comma-joined dotted paths>`.

Most paths come from a single `serde_ignored` pass over the whole file, so
they track the structs and cannot drift. `entries:` and `fallback:` are the
exception: each is an internally-tagged enum (`kind: item` / `kind: query` /
…), and serde buffers an internally-tagged variant's content before choosing
one, so that one pass cannot see inside it. A second, best-effort pass
re-checks every `entries:` element and `fallback:` map against its own
concrete struct (`ItemEntry`, `QueryEntry`, `CollectionEntry`, `IncludeEntry`)
and feeds anything it finds into the same warning. Those paths carry the
entry's position, e.g. `rule.blocks.0.entries.2.id` — enough to find which
entry, not just which file.

**Why the strictness was traded away:** Config lives in a bind-mounted volume
that outlives the container while the parser ships inside the image, so
rolling the image back across a config-schema addition put a newer config in
front of an older parser. A field like `attribution: true` (added to the schema
after a deployment) would crashloop an older image 11 times on every start.
`deny_unknown_fields` made the rollback path useless exactly when it was
reached for — the scenario that should be the safest was the worst failure mode.

**What it costs and where to look:** A misspelling (`selekt: random` instead
of `select: random`) reads as the field being unset, so the pool silently takes
the `round_robin` default and airs something wrong without saying why. When a
channel does not behave as written, grep the station log for
`config.unknown_keys` — the warning names the file and the unrecognised keys.

## Block file

The unit of reuse. A block file is program defaults + a duplicates policy + a
flat list of entries. Source: `config/block.rs` (`BlockFile`).

| Key | Required | Type / values |
|---|---|---|
| `program` | no | [`ProgramMetadata`](#programmetadata) — block-wide defaults |
| `duplicates` | no — default `collapse` | `collapse` \| `keep` |
| `entries` | **yes** | list of [`Entry`](#entry) |
| `fallback` | no | [`Fallback`](#fallback) — resolved instead of `entries` when `entries` resolves to nothing |

| `align` | no — default `none` | `none` \| a duration (`30m`, `1h`): the local-clock grid this block's programmes end on |
| `min_fill` | no — default `2s` | a gap shorter than this is black with the channel logo, not a bumper |
| `fill` | when `align` is set | list of [fill entries](#align-and-fill--bumpers-up-to-the-grid), the bumper scripts that fill a gap |

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

### Align and fill — bumpers up to the grid

`align`, `min_fill` and `fill` sit on a block (a block file or an inline
`rule.blocks` entry) and apply to `entries`, `pattern` and `sequencer` blocks
alike: the pass reads only where each programme ends.

```yaml
align: 30m            # grid lines every 30 minutes from local midnight; `none` turns the pass off
min_fill: 2s          # optional, default 2s
fill:
  - script: bumpers/test_card.rhai   # relative to the channel file; must declare the `bumper` hook
    weight: 1                        # optional, default 1
    when: 'next.type == "movie"'     # optional CEL over next, prev, gap_ms, channel
    min_gap: 5s                      # optional: only while the remaining gap is at least this
    max_gap: 15s                     # optional: only while the remaining gap is at most this
    capabilities: []                 # what the script's capabilities() declares, exactly
```

After a generation's programmes are laid out and probed, and before they are cut
into chunks, the gap between each programme's end and the next grid line is
filled, so the next programme starts on the line. `align` must divide 24 hours
evenly and needs a `fill` list; `fill` and `min_fill` need an `align`.

- A gap always fills to the line, chaining bumpers as long as it takes.
- A gap under `min_fill` is one black item with the channel's own overlay (its
  logo). The playout schema has no fade filter on an item, so that black item is
  the dip out of the programme and into the next.
- Each draw is a weighted pick among the entries whose `when` holds and whose
  `min_gap` admits the remaining gap. If some entry's `max_gap` covers the rest of
  the gap, that draw is the last: an entry whose timeline can stretch (a looped
  segment, or flex room up to the gap) is chosen over one that would leave a black
  filler. If none covers it, the draw is any admitted entry, fitted to its
  `max_gap`. An entry with no `max_gap` covers any gap.
- Each draw logs `event="bumper.fill"` with the channel, script, remaining gap,
  each segment's fitted length and source, and the fallback taken (`none`, or why
  black aired).
- Bumper items carry the `bumper:` id prefix. The airings ledger's programme rows
  are the same with and without the pass.
- A script that fails, or returns nothing, is replaced by black for its draw.
- A generation that starts off the grid (a restart, or a refresh that cut at a
  programme's end) fills up to the first line before its first programme.

`examples/plugins/bumpers/test_card.rhai` is the integration fixture: black with a
text card naming the next programme, looped so it can take any gap.
`examples/channels/align-test.yaml` uses it; run it with
`STATION_CONFIG=examples/station-align.yaml ./tools/dev-run.sh`.

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
canonical hash of its path (root-stripped via the station `identity_roots`), a
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
`source`, `collections`, and `fs_dir` are membership over a related table. `source`
and `fs_dir` take `==` only — the one comparison that reads as "has one of".
`collections` also takes `contains` (exact membership by collection name),
`startsWith` and `matches` (both against the collection's NAME, not the item's),
so a curated list split across several similarly-named collections is reachable
without naming each one — see `catalog/query.rs`'s `method_predicate`.

| Field | Kind | Notes |
|---|---|---|
| `title` | string | |
| `show` | string | the series name on an episode |
| `type` | string | `movie`, `episode`, `other_video`, `bumper`, … |
| `content_rating` | string | |
| `edition` | string | empty = theatrical |
| `studio` | string | one production company |
| `library` | string | the library the item came from, by name |
| `year` | int | |
| `season` | int | |
| `episode` | int | |
| `absolute_episode` | int | franchise-wide episode number: Plex's own when it numbered every episode of the show, otherwise the regular episodes (season ≥ 1 with an episode number) renumbered 1..N by (season, episode); season 0 specials and episodes missing a season or episode get no number |
| `duration_ms` | int | |
| `genres` | tags | |
| `labels` | tags | |
| `cast` | tags | |
| `directors` | tags | |
| `tags` | tags | every namespace at once |
| `source` | membership | `item.source == "plex"` |
| `collections` | membership | by collection name |
| `fs_dir` | membership | `item.fs_dir == "bumpers"` — the folder a file sits in |

`type` says what the item *is*; `library` says which shelf it sits on. They are
separate questions and the schema keeps them separate — which matters most for
**`other_video`** (#214).

A Plex **Other Videos** library reports itself as a movie library: its section
`type` is `movie` and every item inside reports `type = "movie"`, whether it
holds feature films, concert recordings, or home videos. Taken at face value
that makes `item.type == "movie"` match a concert rip and a home video, which is
not what any channel asking for movies means. So the ingester reads the
section's metadata *agent* — the only thing that distinguishes them — and gives
every item from a movie-type library matched against no agent
(`tv.plex.agents.none`) the type `other_video` instead:

```yaml
query: 'item.type == "movie"'          # feature films only
query: 'item.type == "other_video"'    # concerts, home videos, party clips
query: 'item.library == "Concerts"'    # one such library by name
```

The same word ErsatzTV uses for the same idea — its search surface has
`other_video` as a media kind alongside `movie` and `episode`. The consequence
to know: `item.type` is this station's vocabulary derived from Plex's, not a
passthrough of Plex's own `type` attribute. A `show`-type library matched
against no agent is *personal TV* and keeps its episodes, so `other_video` never
swallows something a show channel needs.

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

#### `keyword_score` — count matches against a keyword set (#161)

`keyword_score(["a", "b", …])` returns how many of the listed keywords an item
matches. Compare it to a threshold with the ordinary operators — `>=`, `>`, `==`,
`!=`, `<=`, `<` — so a pool can ask for "enough of these that I mean it" where a
single `genres.contains(...)` is one shared tag.

A keyword matches when it is a case-insensitive substring of the title, or equals
(ignoring case) one tag value: any genre, label, cast or director, as `item.tags`
does. A keyword repeated in the list counts once. The station ships no keyword
sets; the list is the channel author's.

Plex genre is multi-valued, so `item.genres.contains("Comedy")` admits both
*Beverly Hills Cop* (`Action`, `Comedy`, `Crime`) and *Shaun of the Dead*
(`Comedy`, `Horror`). A Sitcoms-style pool that wants comedies and not
action-comedies counts the action keywords and requires none:

```yaml
pools:
  - name: comedies
    expr: 'item.genres.contains("Comedy") && keyword_score(["action", "crime", "thriller"]) == 0'
```

That admits *Shaun of the Dead* and *Airplane!* (`Comedy` only) and drops *Beverly
Hills Cop*, which scores 2. Loosen the pool to `< 2` to let a single-genre
crossover back in. These three films are the fixture of the test
`keyword_score_counts_matched_keywords_against_a_threshold` in `catalog/query.rs`;
the scores above are what that test asserts.

A pool whose `expr` can never match fails when the config loads, naming the pool:
an empty keyword list, or a threshold the list cannot reach (`>= 3` over two
keywords). `etv-query-test` evaluates the function against its own item fields (title and
categories), so a set can be tuned there first; it does not apply the load-time
threshold check, and it folds case with Unicode rules where the station folds
ASCII only.

#### `category("name")` — a keyword set defined once (#170)

A category is a keyword set plus its threshold, declared in a file outside any
channel and referenced by name. `category("kung-fu")` in a pool `expr` (or a
pool's `sources` expression) means
`keyword_score([…the category's keywords…]) >= <its threshold>`, so it is a
yes/no test that combines with `&&` and `||` like any other:

```yaml
# station.yaml
categories: ["categories/*.yaml"]   # literal paths or globs, relative to this file

# categories/martial.yaml
categories:
  - name: kung-fu
    keywords: ["kung fu", "martial arts", "shaolin"]
    threshold: 2          # matches at least 2 of the 3

# channels/kungfu.yaml and channels/action.yaml, each in a pool
expr: 'category("kung-fu") && item.type == "movie"'
```

Both channels resolve the same item set, because the station expands the
reference to the same inline form at load. Edit the file and reload, and every
channel naming the category changes with it. The inline `keyword_score([...]) >= n`
form keeps working exactly as before.

**A category used by one pool is not worth declaring.** Naming it adds a file to
look in and a name to remember, and buys nothing over writing the set inline in
that pool. Reach for a category when a second pool wants the same set: the reuse
is the whole payoff. The station ships no categories; what "kung fu" means is
the channel author's.

These fail at load, not at air time:

- a reference to a category nobody declared, naming it and listing the declared
  ones (the channel is dropped, as for any bad channel config);
- two declarations of one name, naming both files and entry numbers (the whole
  station refuses to load, since the category is shared);
- an empty keyword set, or a threshold outside `1..=` the number of distinct
  keywords.

The reference is found by text, so a `category("…")` written inside a string
literal of the expression is expanded too.

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
ETV-next's XMLTV). Defined in `vendor/etv-next/`
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
| `credits` | object — `{director: [string], actor: [{name, role?}], writer: [string]}` |
| `country` | list of strings |
| `star_rating` | string, e.g. `"4 / 5"` |

`credits` is emitted as XMLTV `<credits>` with its children in the order that
format requires (director, actor, writer) whatever order they are written in
here. `country` becomes one `<country>` element per entry. `star_rating` is
passed through verbatim into `<star-rating><value>`.

### Bumpers in the guide

A bumper's playout item id starts with `bumper:`
(`ersatztv_playout::playout::BUMPER_ID_PREFIX`). ETV-next's XMLTV route folds
each bumper into the programme before it: the bumper gets no `<programme>` and
that programme's `stop` moves out to the bumper's end, so the guide shows one
row per programme and no gap. A bumper with no programme before it keeps its
own row.

`guide.show_bumpers: true` lists every bumper as its own row instead. It is set
in `station.yaml` under `guide:` (default `false`) and overridden per channel in
`channel.yaml` under `guide:`. The station writes the resolved value into each
`channelN.json` as `guide.show_bumpers`, and the XMLTV route reads it from there.

Set on a block's `program:` for defaults; set on an entry's `program:` to
override per item. Item values win over block defaults.

For an item resolved against the catalog (a `query`/`collection` entry, or an
`item` entry with no inline `program:`), `credits` and `country` default from
the catalog's `director`/`cast`/`writer`/`country` tags — no config needed,
same as `description` (catalog `summary`) and `categories` (catalog genre
tags). Catalog tags carry no character name, so every `credits.actor[].role`
from this path is unset. `star_rating` has no catalog-backed source at all;
it is populated only by an inline `program:` block.

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
ignored and warned about; see [Unknown keys](#unknown-keys).

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

# source_roots left empty on purpose here: a full-library scan is unaffordable
# at startup on this deployment, so the catalog is Plex-only. identity_roots is
# still set — it's pure string manipulation and costs nothing at startup — so a
# local item's derived identity still agrees with a different store reading the
# same library. The two are independent; setting one does not imply the other.
identity_roots:
  - /data/media

catalog_path: /var/lib/etv-station/catalog.db   # optional — enables query channels
catalog_refresh_secs: 900      # optional — trust the catalog this long without asking Plex
full_sweep_after_secs: 86400   # optional — force a full (deletion-catching) re-read this often
seed: 1138                     # optional — every channel with no seed: of its own inherits this, salted per channel
temperature: 0                 # optional — every channel with no temperature: of its own inherits this
```

| Field | Required | Type / default |
|---|---|---|
| `tz` | no — default `UTC` | IANA time zone string; `ETV_STATION_TZ` overrides at runtime |
| `output_base` | **yes** | path — base directory every channel writes under; `ETV_STATION_OUTPUT_BASE` overrides at runtime |
| `channels` | **yes** | list of path strings; each is a literal path or a glob (`*`, `?`, `[`) |
| `categories` | no — default empty | list of category files, literal or glob like `channels`; each declares named keyword sets pools reference as `category("name")` (see [`category`](#category-name-a-keyword-set-defined-once-170)) |
| `source_roots` | no — default empty | list of directories the daemon walks to populate the catalog when `catalog_path` is set — an operational choice about what this deployment scans, not used for identity. Empty skips the filesystem scan entirely. `ETV_STATION_SOURCE_ROOTS` (colon-separated) overrides at runtime. |
| `identity_roots` | no — default empty | list of media mount roots (the daemon's filesystem view) used to canonicalise a local item's path when deriving its identity, so the same file under different mounts — or a different store reading the same library — is one identity. Empty skips root-stripping. Pure string manipulation; touches no disk. `ETV_STATION_IDENTITY_ROOTS` (colon-separated) overrides at runtime — the intended way to supply it, since mount paths are host-specific and do not belong in a committed config. Deliberately not defaulted from `source_roots` (#243): one is an operational choice, the other a property of the media layout. |
| `catalog_path` | no — default unset | path to the sqlite catalog the daemon opens and ingests (local-FS over `source_roots`, plus Plex when `PLEX_URL`/`PLEX_TOKEN` are set) at startup. Enables `query` entries and non-`manual` order, and lets a manual `local` item path-match onto a catalog identity (so it collapses with a query for the same file). Unset keeps the catalog-free behavior — only inline-item `manual` channels resolve. `ETV_STATION_CATALOG` overrides at runtime. |
| `overlay` | no | station-wide overlay default, the least specific level of the cascade every channel inherits from unless it says otherwise — see [Overlay cascade](#overlay-cascade) |
| `tracks` | no | the station level of the [track-selection](#track-selection) rules: which audio and subtitle stream of each file plays |
| `seed` | no — default unset | a number, or a rotation — see [Seed and temperature](#seed-and-temperature). Every channel that sets no `seed:` of its own inherits it, **each number salted with that channel's `name`**. Unset means every unseeded channel draws a new seed each local day from a fixed base. |
| `temperature` | no — default `0` | a number ≥ 0, or a rotation — see [Seed and temperature](#seed-and-temperature). Every channel that sets no `temperature:` of its own inherits it. |
| `hooks` | no — default empty | list of [event hooks](#event-hooks) that run for the whole station |
| `catalog_refresh_secs` | no — default `900` | seconds a freshly ingested catalog is trusted without contacting Plex at all. A restart inside this window reuses the sqlite file as it stands, which is what makes an edit-restart loop cheap. `0` re-checks Plex on every start. |
| `full_sweep_after_secs` | no — default `86400` | seconds before a delta ingest is escalated to a full re-read. A delta asks Plex only for records touched since the last pass and therefore cannot express a *deletion* — an item removed from the library simply stops being mentioned. Only a full pass notices those. `0` disables delta ingest: every pass is full. |
| `idle_grace_seconds` | no — default `90` | seconds a channel's worker stays warm after its last viewer leaves, so surfing back to it skips the cold start. Written into every `channelN.json`; ETV-next's worker reads it. `90` is also the floor — a smaller value is raised to it, because the overlay renderer exits after 60s without a reader and must stay under the worker's lifetime. A warm channel keeps its ffmpeg encoding for nobody, so raise it only as far as the encoder has headroom. |

**How the three ingest modes are chosen.** At startup the daemon compares the
catalog's recorded last-ingest time against the two knobs above. Age below
`catalog_refresh_secs` → skip, no HTTP at all. Age at or beyond
`full_sweep_after_secs` (or no prior ingest, or a clock that moved backwards) →
full re-read. Anything between → delta: each library section is queried with
`updatedAt>=<last ingest>` OR `addedAt>=<last ingest>` — the OR reaches an item
Plex reports with no `updatedAt` at all, which would otherwise fail the
comparison identically to a stale one and be invisible to every delta until the
next full sweep. A collection whose own `updatedAt` predates the cursor skips
its per-collection children request. The full-sweep check is
applied *before* the refresh window, so a constantly-restarted station still
gets its periodic deletion-catching pass. The timestamp is recorded inside the
ingest transaction and taken before the fetch begins, so a failed pass never
advances the cursor past changes it did not write.

Each entry in `channels` is resolved relative to the station file's directory. A
glob expands to every matching file (matching nothing is an error); a literal
path is taken as-is. Files matched by more than one entry appear once. A
channel's **output folder is derived** — `{output_base}/{name}`, where `name`
is the channel's required `name:` (below), never its config file or folder name.

## Channel file

Defines one channel's playout window and the rule that composes blocks. Source:
`config/channel.rs` (`ChannelConfig`).

| Field | Required | Type / default |
|---|---|---|
| `number` | **yes** | int — the channel's dial number. See [Channel numbers](#channel-numbers). |
| `name` | **yes** | string — the channel's identity: the key its play history and series resume cursors are filed under, the salt on an inherited seed, the log label, overlay handshake, and output folder leaf. Must not be blank or contain path separators or control characters. See [Channel identity](#channel-identity). |
| `window_days` | no — default `1` | int — how far ahead the schedule is written, and the span one generation is allowed to cover |
| `chunk_hours` | no — default `6` | int — playout file size only; it does not bound a generation |
| `roll_interval` | no — default `"3600s"` | duration |
| `retention_days` | no — default `7` | int |
| `seed` | no — inherited from the station `seed` when unset | a number, or a rotation — seeds `random` order, a plugin's `ctx.seed` and every other seeded draw on the channel. Set here it wins outright; unset it falls back to the station `seed` salted with this channel's `name`, then to a new seed each local day — see [Seed and temperature](#seed-and-temperature). |
| `temperature` | no — inherited from the station when unset, else `0` | a number ≥ 0, or a rotation — how far a plugin's draw may stray from its own ranking, handed to scripts as `ctx.temperature`. See [Seed and temperature](#seed-and-temperature). |
| `overlay` | no | `clear` \| `{ file: <path> }` \| an inline overlay spec — see [Overlay cascade](#overlay-cascade) |
| `tracks` | no | the channel level of the [track-selection](#track-selection) rules; blocks, pools and items refine it |
| `subtitle` | no | `{ default_on: true \| false }` — announce this channel's subtitle track as `DEFAULT=YES`; unset leaves the station's `normalization.subtitle.default_on` |
| `hooks` | no — default empty | list of [event hooks](#event-hooks) scoped to this channel |
| `rule` | **yes** | `{ blocks: [...] }` — see below |

### Event hooks

`hooks:` appears in `station.yaml` and in `channel.yaml`. Each entry runs code
when something happens; it cannot change the schedule (ADR 0021). Source:
`config/hooks.rs`, `hooks.rs`.

```yaml
hooks:
  - on: cron
    every: 15m                 # humantime; at least 1s; the first run is one interval after start
    run: hooks/notify.sh       # any executable: the event JSON arrives on stdin
  - on: cron
    every: 1h
    run: hooks/refresh.rhai    # a .rhai script runs on a worker thread
    grants: [http, exec]       # only a .rhai script takes grants
```

| Field | Required | Meaning |
|---|---|---|
| `on` | **yes** | the trigger: `cron`, or one of the events below |
| `every` | for `cron` only | interval between runs; refused on every other trigger |
| `run` | **yes** | path, relative to the declaring file's directory. A name ending `.rhai` is a script; anything else is executed as a program. |
| `grants` | no | `exec` adds `exec(program, args)` -> `#{ code, stdout, stderr }` (each stream capped at 1 MiB; the program is killed and the call errors after 600 seconds); `http` adds `http(url, method, body)` -> body text, waiting at most 30 seconds. A script with neither can compute and call `log(text)` and `parse_json(text)`. Refused on an executable, which runs with the daemon's own permissions. |

| Trigger | Fires | `channel` | `data` |
|---|---|---|---|
| `startup` | once, when the daemon process starts | the declaring channel, else null | `{}` |
| `config_reload` | once per config reload that parsed, as the generation under the config in force begins | the declaring channel, else null | `{}` |
| `generation_end` | after a channel writes one generation of schedule | that channel | `{phase, from, to, items, files}` |
| `chunk_written` | for each playout file a generation writes | that channel | `{path, phase}` |
| `schedule_item_change` | when the station clock passes an item boundary on a channel, watched or not; the item airing when the daemon starts is the baseline and fires nothing | that channel | `{item, previous}`, each `{id, title, start, finish}` |
| `playback_item_change` | when ETV-next's channel worker begins encoding an item (it leads what the viewer sees by the worker's transcode buffer); only while someone is watching. Items already logged when the daemon starts fire nothing | that channel | `{at, item}`, `item` is `{id, title, start, finish}` |
| `viewer_start` | when a viewer tunes in and ETV-next starts the channel's worker | that channel | `{at}` |
| `viewer_stop` | when the worker ends: the last viewer left, or the run failed. A worker killed by a signal writes nothing, so no `viewer_stop` follows | that channel | `{at}` |

The three playback triggers read `.playback-events.jsonl` in the channel's
playout folder, one JSON line per event, written by the vendored ETV-next
worker (`ersatztv-channel`'s `playback_events.rs`). The worker drops the file at
a session start once it passes 256 KiB. The station polls it every 500 ms and
logs each line it fires as `playback.event`.

A hook in `station.yaml` runs for every channel's events; a hook in a
`channel.yaml` runs only for that channel's. `startup` and `config_reload` reach
hooks of both scopes.

The event is `{hook, trigger, scope, channel, fired_at, data}`: stdin JSON for an
executable, the `event` constant for a script. A `cron` run still going when its interval
elapses again makes that interval skip (`hook.skipped`); firings of the other triggers queue behind a run in flight and run one at a time (`hook.dropped` past 1000 waiting). A run is killed after 10 minutes (a script is also stopped after 100 million operations). A reload drops firings still queued and kills a run in flight (`hook.run` reports `killed by a config reload`), so it never waits on a hook (a script blocked in `http` finishes that call, at most 30 seconds, before it stops); daemon shutdown kills runs the same way; the next generation's hooks then start, and every interval restarts from zero. A hook that must survive a reload has to hand its work to a process of its own. A failed run's `hook.run` line carries the last 1000 bytes of the program's stderr. The station keeps no record
of executions, so a script that starts outside work owns its own deduplication.

Each run logs one `hook.run` line with `hook` (`station#0`, `channel:<name>#1`),
`trigger`, `duration_ms` and `exit` (the exit code; `0` for a script that
finished; `-1` when no code exists). `etv-station --config <station.yaml>
--list-hooks` (or `admin hooks`) prints every configured hook.

### Channel numbers

`number:` is the channel's dial number (#263). It is required, and it is
**never derived** from a channel's position in the sorted folder list — that
derivation was the bug: adding, renaming, or removing any channel renumbered
every channel after it, silently invalidating every saved favourite,
remembered number, and Plex's own cached lineup. Numbers need not be
contiguous — `1, 2, 5, 100` is a valid lineup, and gaps are not warned about.

Two failure behaviors, both scoped to the offending channel(s) rather than the
whole station — one bad channel config costs one channel, not the station
(#156):

- **A channel config with no `number:` fails to load.** An ERROR is logged
  naming the config file and the missing field; the channel does not appear
  in the lineup, and every other channel still serves.
- **Two channels declaring the same `number:` are both refused.** An ERROR is
  logged naming both config files and the number; neither is silently
  preferred, and every channel with a unique number still serves.

### The airings ledger

`history.db` holds one `airings` row per playout item, programmes and bumper
segments alike. `kind` is `programme` or `bumper`; a bumper row also carries
`source` (the clip or still path, render path, or lavfi graph), `in_ms` and
`out_ms` (the slice of a clip played) and `produced_by` (the plugin script that
built it). The resume cursor, the adjacency tail and the scorer's recency input
read `kind = programme` rows only, so a bumper segment never moves a series or
counts as a repeat. A rewind deletes rows at and after the rewind instant, so
bumper rows leave with the schedule they belonged to.

A bumper script reads the ledger with `ctx.airings(filter)`, which needs no
capability grant. It returns an array of maps `#{ channel, entry_id, show_id,
start_ms, kind, source, in_ms, out_ms, produced_by, error_card }`, past and
scheduled rows alike. Filter keys, all optional: `channel` (default the
bumper's own; `"*"` for every channel), `kind`, `entry_id`, `show_id`,
`source`, `produced_by`, `from_ms` and `to_ms` (epoch milliseconds bounding the
start; inclusive, exclusive), `order` (`"asc"` default, or `"desc"`) and
`limit` (default 100, at most 1000). An unknown key is an error.

### Channel identity

`name:` is the channel's identity (#414). It is required, and it is **never
derived** from the config's file or folder name. It is the `channel` column on
every `airings` row in `history.db` and the key every series resume cursor is
read back by, so deriving it from the folder was the bug: renaming a channel's
directory filed its whole play history under a string nothing queried again,
and every show restarted at episode 1 without an error. With `name:` declared,
the folder is free to be renamed or renumbered; changing `name:` itself is the
one edit that starts a channel from an empty history.

The same two failure behaviors as `number:`, scoped to the offending
channel(s):

- **A channel config with no `name:` fails to load.** An ERROR
  (`channel.load_failed`) is logged naming the config file and the missing
  field; the channel does not appear in the lineup, and every other channel
  still serves.
- **Two channels declaring the same `name:` are both refused**, compared
  ignoring case (`Foo` and `foo` share one folder on macOS and SMB). An ERROR
  (`channel.name_collision`) is logged naming both config files and the name;
  neither is silently preferred, and every other channel still serves. Both
  collision checks read the whole roster before either drops anything.

### Channel logo — `logo:`

```yaml
logo: logo-guide.png   # relative to this channel file's directory
```

`logo:` is the channel's tile in a guide (#342). When ETV-next's config is
rendered, the station copies the image beside `lineup.json` as
`channel-logos/<number>.<ext>` and names `./channel-logos/<number>.<ext>` as the
lineup channel's `logo`. ETV-next serves that file at `/logo/<number>.<ext>`
and publishes the URL, resolved against the requesting host, as the XMLTV
`<channel><icon src>` and the M3U `tvg-logo`. The URL carries a `?v=` taken
from the image's bytes, so a guide client that caches icons by URL picks up a
new one. ETV-next reads the lineup when it starts, so a changed logo appears
on the next container start.

It is separate from the overlay: the on-screen mark is usually white on
transparent, which disappears in a light-themed guide, so a channel commonly
names a different image here. An unreadable image publishes the channel with
no logo, and a `lineup.logo.skipped` WARN names the file and the reason. Each
logo is written to a temporary name and renamed into place, and a render
removes only the files no channel names any more, so a logo in use is never
missing from the folder ETV-next serves.

### Composing blocks — `rule.blocks`

Each entry under `rule.blocks` is a **block include** (`config/rule.rs`,
`BlockInclude`). It either **references a block file** or **inlines the block
body**, and carries the composition fields `mode` / `order` / `filter`. Unknown
fields are ignored and warned about; see [Unknown keys](#unknown-keys).

**Reference form** — body lives in a separate file:

```yaml
# channels/starwars.yaml — no output_folder; identity is the declared name
# "starwars", so it writes to {output_base}/starwars

number: 24
name: starwars
rule:
  blocks:
    - block: "../blocks/starwars-timeline.yaml"
      mode: "all"
      order: "manual"
```

**Inline form** — body lives in the channel file:

```yaml
# channels/lotr.yaml — identity "lotr" from its declared name

number: 26
name: lotr
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
(`program` / `guide` / `overlay` / `duplicates` / `entries` / `fallback`) is
copied into the include, so a reference and an equivalent inline block resolve
identically. `mode`, `order`, and `filter` are **composition fields on the
include** — they never live in the block file body.

### Schedules — `rule.slots` and `rule.schedules`

`rule.blocks` are the channel's **any-time rules**: they air in every minute
nothing below claims. A channel can also declare **slots**, named blocks with
the same shape as a `rule.blocks` entry, and **schedules** that place those
slots over ranges of the local day, optionally only on some dates or weekdays
(ADR 0004).

```yaml
rule:
  blocks:                       # the any-time rules
    - pools: [{ name: classics, expr: 'item.collections.contains("Comedy Classics")' }]
      pattern: [{ pool: classics, take: 1 }]

  slots:
    halloween:
      pools: [{ name: horror, expr: 'item.collections.contains("Elevated Horror")' }]
      pattern: [{ pool: horror, take: 1 }]
    prime:
      pools: [{ name: prime, expr: 'item.type == "movie"' }]
      pattern: [{ pool: prime, take: 1 }]
    cartoons:
      pools: [{ name: cartoons, expr: 'item.collections.contains("Saturday Morning Cartoons")' }]
      pattern: [{ pool: cartoons, take: 1 }]

  schedules:
    - name: halloween
      dates: ["10-25..10-31"]
      hours: { "00..24": halloween }
    - name: weekend
      weekdays: [sat, sun]
      hours: { "06..12": cartoons }
    - name: every-day
      hours: { "20:00..23:30": prime }
```

| Key | Meaning |
|---|---|
| `name` | Required, unique among the channel's schedules. Logged with every generation it decides. |
| `dates` | Optional list of calendar spans. `"MM-DD"` or `"MM-DD..MM-DD"` repeats every year, and an end before the start crosses December 31st (`"12-20..01-05"`). `"YYYY-MM-DD"` or `"YYYY-MM-DD..YYYY-MM-DD"` is a one-off, for a date that moves each year such as Thanksgiving. Both ends are inclusive. Empty means every day. |
| `weekdays` | Optional list of `mon`..`sun`. Empty means every day. With `dates`, a day must match both. |
| `hours` | Required map of clock range → slot name. `"HH..HH"` or `"HH:MM..HH:MM"`, 24-hour, local to the station's `tz`. The end is exclusive and may be `24`; `"21..02"` wraps past midnight and belongs to the day it starts on, so a Friday-only `"21..02"` covers Saturday 01:00. `"00..24"` is the whole day. |

**What airs at an instant.** The station walks the schedules from highest to
lowest and airs the slot of the first one whose day matches and whose range
covers that instant. A schedule with `dates` is highest, then one with
`weekdays`, then one with neither; written order breaks ties. An hour the top
schedule leaves open falls to the next one down — a holiday that claims only
20:00-23:00 keeps the weekend's morning cartoons — and when no schedule claims
the instant, the any-time `rule.blocks` air.

**Where a generation stops.** A generation airs one answer: one slot, or the
any-time blocks. It stops laying items at the next minute a different block
would be on, and what is airing then finishes before the next block starts,
never cut short and never followed by dead air. For an entries block that is
the one item airing at the boundary. A pattern block stops at the first visit
(pattern step) that would start at or after the boundary, so its boundary lands
later by at most the visit that was airing: one movie, one `take: 3` run of
episodes, a whole season for `take: all`. The rest of the cycle is not aired;
the `.resume` sidecar records the cycle and the step the generation stopped
before (`cycle`, `step`) and a hash of the pattern it walked (`pattern`), and
the next time the slot airs it opens on that step. If the pattern was edited
in between, the stored step no longer means the same visit, so the cycle opens
at step 0. A block with a `restart` pool has no rotation to carry across the stop,
so it still ends on a cycle boundary.

**State.** Each slot continues where it stopped the last time a schedule
placed it: pool rotation is keyed by pool name, which must be unique across
`rule.blocks` and `rule.slots` together, and an entries slot keeps its own list
position in the `.resume` sidecar's `slot_positions`.

**Load errors.** A schedule with no name, a repeated name, no `hours`, a range
that is not a time (`"25..26"`, `"20..20"`), two ranges in one schedule that
overlap, a date no calendar has (`"02-30"`), a span that mixes the yearly and
dated forms, a slot name `rule.slots` does not declare, a slot no schedule
places, and a `cycles:` on any block of a channel with schedules (an authored
count would run past the boundary) all fail the channel's load, naming the
schedule, slot or block. `rule.blocks`
must still hold at least one block, which is what airs when no schedule
claims an instant.

**Observing it.** Every generation of a channel with schedules logs
`event="calendar.select"` with the window start, the station `tz`, the slot and
schedule that won (empty for the any-time blocks), and `cut`, the instant the
generation stopped. Each airing's `block` index counts `rule.blocks` first,
then `rule.slots` in name order; the web UI's Review names a slot block by its
slot name when it has no `program.title`.

Worked samples: `examples/samples/seasonal.yaml` (dated slots) and
`examples/samples/adult-swim.yaml` (nightly and Saturday slots).

### Seed and temperature

Every random choice on a channel is computed from its **seed**, which is why a
pinned config and date reproduce the same schedule. **Temperature** says how
far a plugin's draw may stray from its own ranking. Each can be fixed, or
**rotate**: change value on a cadence, so the same channel airs a different
set of answers each period while every period still reproduces.

Both fields take the same three shapes:

```yaml
seed: 34                                   # fixed — one seed, for ever
seed: { rotate: 2d }                       # a new seed every 2 days
seed: { rotate: 3d, cycle: [34, 99, 7] }   # 34, then 99, then 7, 3 days each, repeating

temperature: 0.5                                     # fixed
temperature: { rotate: 2d, cycle: [0.7, 0.7, 0.25] } # one value per period, repeating
temperature: { rotate: 2d, between: [0.25, 0.7] }    # a seeded draw from the range each period
```

| Form | `seed` | `temperature` |
|---|---|---|
| a number | that seed, for ever | that temperature, for ever (≥ 0) |
| `{ rotate: <span> }` | a new seed each period, drawn from a fixed base and salted with the channel name (and the pool name, at pool level) — reproducible, nothing to pick | refused: nothing says which value a period gets |
| `{ rotate, cycle: [...] }` | walks the list, one entry per period, wrapping | walks the list, one entry per period, wrapping |
| `{ rotate, between: [low, high] }` | refused | a value drawn uniformly from the range, seeded by the period, so a rerun draws it again |

**Cadence.** `rotate` is a duration (`4h`, `2d`, `1w`; at least `1h`).
Periods count from 1970-01-01 00:00 in the station's local time, so a
whole-day cadence rolls over at local midnight — and a `7d` one on a Thursday,
the epoch's weekday. `seed` and `temperature` each run on their own clock; a
pool's list turns over whenever either one rolls over.

**Levels.** Station → channel → pool, the lowest level that sets a field
winning outright — a pool's `seed:` or `temperature:` replaces the channel's
for that pool alone (its plugin, its `order: random` shuffle, its
`select`/`from: random` draws, and the `chance:` of steps naming it). Nothing
set at any level means a new seed each local day and temperature `0`: a
channel nobody tuned still varies its exploration picks and shuffles daily,
reproducibly, and ranks exactly as its plugin does.

**What reads which.** Temperature reaches only scripts — nothing in the core
ranks, so nothing in the core has an order to loosen:

| Where | Rotating seed | Temperature |
|---|---|---|
| plugin pool ranking (`ctx.seed`, `ctx.temperature`) | yes | yes — the script decides what it means; `taste-cosine.rhai` reads it as a softmax temperature over its scores |
| sequencer script (`ctx.seed`, `ctx.temperature`) | yes | yes — the script's to interpret |
| `order: random` (block, pool, query entry, fallback), `bucket_order: random` | yes — a new shuffle each period | no: a shuffle is already as random as it gets |
| `select: random`, `from: random`, a step's `chance:` | yes | no, same reason |
| field sorts (`year:asc`, `season:asc,episode:asc`), `manual`, a collection's order, CEL filters, rotation/take/constraints | no | no — chronological stays chronological |

**A rotating plugin pool walks one list per period.** The first generation of
a period ranks the pool's list and stores it in the `.resume` sidecar
(`lists`); every later generation in that period walks the same list instead of
re-ranking — a re-ranking would have moved under the walk, since a plugin's
inputs (`ctx.recent`, the watch history) change as the channel plays. With
`advance: resume` the walk continues down that list generation after
generation; with `advance: restart` every generation reopens at its top. A list
that runs out mid-period is extended by the plugin (the same refill a drained
pool always gets) and the extension is stored with it.

**At a rollover** the pool's list is ranked again under the new seed and
temperature, and a resuming pool opens on **the first series of the new list
that its current lap has not aired** — a lap being the series visited since
the walk last covered every series in its list. A list that changed completely
therefore opens on its top; a list that barely changed (a shows pool at
temperature `0`, where only the exploration pick moves) carries on past what it
already aired instead of replaying its top every period and never reaching its
tail. A rollover lands on **the first visit that airs after the boundary**,
inside a generation if need be: a pattern walk re-checks its dice at every
visit. A block that lays a seeded list in one pass — an entries block with an
`order: random`, or a sequencer — cannot re-draw partway, so a generation
holding one is cut at the next rollover and the next generation opens in the
new period.

A fixed seed and a fixed temperature never roll over: such a pool is ranked
afresh every generation, holds no stored list, and resumes at the series it
was up to — exactly as before rotation existed.

**Reading it back.** Every drawn item's `select` audit stage records the
`seed` and `temperature` its visit drew with, the `seed_period` /
`temperature_period` when either rotates, and — on a pool whose list rotates —
its `list_position` of `list_length` in that period's ranked list: positions
climb through one period and start over at a rollover. `taste-cosine.rhai`'s
own stage adds `score_rank` (where the pick stood by score alone) beside `rank`
(where the temperature put it). The daemon logs `variety.ranked` each time a
pool ranks a period's list and `variety.rollover` when one rolls over
mid-generation. the web UI's review API carries all of it per airing as
`ranking`.

**Why an inherited number is salted.** Nothing in the seeded draw mixes in
anything channel-specific — the shuffle consumes the seed as raw PRNG state,
and a pattern roll mixes only `(seed, cycle, step, nonce)`. Handing every
channel the same station number verbatim would make any two channels with the
same candidate multiset produce the *identical* order. So a station `seed:`
reaches each channel salted with the channel's declared **`name`** — its
identity — and deliberately not its channel number, which is a presentation
detail, nor its folder name; renumbering a channel or renaming its folder must
not reshuffle it. A fixed seed is salted, and so is every entry of a station
`cycle`; a derived rotation is salted when it is drawn.

The derivation is a fixed algorithm (FNV-1a over the name, then the SplitMix64
finalizer), not `DefaultHasher`, so one pinned station seed reproduces the same
per-channel seeds on every machine and every release.

### Overlay cascade

`overlay:` is one key, spelled the same way at three levels — the station
file, a channel file, and a `rule.blocks` entry (or the block file it
references) — that together resolve to *which whole config* the channel's
single `etv-overlay` process runs at a given moment (#48). Source:
`config/overlay.rs` (`OverlayDecl`, `resolve_decl`, `resolve_channel`).

```yaml
# a value at any of the three levels
overlay: clear

overlay:
  file: shared/pierce-overlay.yaml   # reuse a spec file, relative to this file's directory

overlay:                             # or write the spec inline
  width: 1280
  height: 720
  framerate: 30
  graphics:
    - layers:
        - type: image
          path: logo.png
          corner: bottom_right
          height: 56

overlay:                             # or add to what the level above resolved
  extend:
    graphics:
      - layers:
          - type: image
            path: logo.png           # relative to THIS file's directory
            corner: bottom_right
            height: 56
```

**Resolution replaces by default and appends when asked; deepest declared
level wins.** Walking station → channel → block: a level that says nothing
inherits its ancestor's resolved config unchanged; `clear` means *this level
draws nothing*, discarding whatever an ancestor resolved; a `file:` reference
or an inline spec is that whole config, in full, discarding whatever an
ancestor resolved; and `extend:` keeps the ancestor's config and appends its
graphics to the end.

`extend` is the only composition there is — it adds whole graphics (see
[Graphics](#graphics)) after the inherited ones, and that is all. It cannot
reach inside an inherited graphic and carries no geometry, so unlike
`guide:`'s field-by-field cascade there is no per-field override. It
exists because the station has graphics every channel shares (a Now/Next
snipe, a block card) plus one thing only the channel knows (its own logo, at
its own size), and replacement alone forced all 60 channels to restate the
shared half. Each level's paths resolve against its own file's directory
before its graphics are appended, so a station graphic naming
`shared/logo.png` and a channel graphic naming `logo.png` are both correct in
one resolved spec. See ADR 0008.

An `extend` with nothing above it to extend — no ancestor declared a complete
config, or the nearest one said `clear` — fails at config load naming that,
rather than resolving to a spec with no geometry.

**The station → channel resolution is the spawn config.** It is what sizes
the canvas and the fifo the `etv-overlay` process is spawned with, and its
`width`/`height`/`framerate`/`pixel_format` are what `channel{N}.json`'s
`overlay` block reports to ETV-next. A block-level declaration may not
disagree with the channel's on those four fields — they are baked into the
fifo's byte layout, so a block that wants different ones fails to load,
naming the mismatch, rather than being silently ignored or attempting a live
resize. Only the graphics may differ per block; see
`docs/architecture.md`'s Graphics overlay cascade section for how that swap
reaches the already-running process without a respawn (ADR 0007).

#### Graphics

An overlay spec is its canvas (`width`, `height`, `framerate`,
`pixel_format`) plus a `graphics:` list. A **graphic** is one script, the
layers it drives, and the `config` it reads:

```yaml
width: 1280
height: 720
framerate: 30
graphics:
  - name: now-next               # optional; named in errors and logs
    script: shared/now-next-snipe.rhai
    config: { interval_secs: 300 }
    layers:                      # the script addresses these by index, from 0
      - { type: scrim, edge: bottom, size: 150, color: [0, 0, 0, 175] }
      - { type: text, content: "", corner: bottom_left, margin: 40, font_size: 14.0 }
  - layers:                      # no script: drawn as declared
      - { type: image, path: logo.png, corner: bottom_right, height: 56 }
```

Graphics are independent. Each script sees only its own layers (index 0 is
its own first layer) and its own `config`, so adding or removing a graphic
cannot re-target another's. They draw in declaration order, a later graphic on
top. A script that hides itself (`visible: false`) or throws affects only its
own graphic: a failing script falls back to its graphic's declared layers and
the others keep drawing. A graphic with no `script:` is static.

Unknown keys are refused. A spec still written with top-level `script:`,
`config:` or `layers:` fails to load rather than parsing as an overlay that
draws nothing; move those three keys under a `graphics:` entry.

**What a script can read.** `time`, `frame`, the current item's `title`,
`sub_title`, `description`, `season`, `episode`, `year`, `content_rating`
(the catalog's rating for the item, `""` when it has none), the `next_*`
equivalents, `item_elapsed`, `item_remaining`, `item_duration` (the two summed, `-1.0`
when either is unknown), its own `config`, and the item's opaque blob as
`metadata` (an empty map when the item attached none, so `metadata.foo == ()`
works on any item). `prev_metadata` is the blob of the item that
aired before it, so a script can notice a change between consecutive items;
it is `()` for a channel's first item and when the earlier item carried none.
Test for a map with `type_of(prev_metadata) == "map"` before reading a key
from either.

**An item's own overlay.** A playout entry may carry `overlay`, which the
station writes and the overlay process obeys while that item airs:
`{script, vars}` runs that script instead of the channel's overlay (and the
cascade is skipped), `"none"` draws nothing, and an entry without the field
keeps the channel overlay. `script` is read as written, so the station writes
an absolute path. The script sees `vars` as a map, empty when the entry named
none, so `vars.title` reads as `()` rather than failing; a script under
`shared/bumpers/` must tolerate every key being unset. An item overlay has no
declared layers, so its script draws only the layers it declares in its
result (below).

**A script declares layers in its result.** Each entry of the `layers` array a
script returns addresses the layer at its index. An entry with a `type` key is
a whole layer, written exactly as a spec's `layers:` entry — it replaces the
declared layer at that index, or adds one past the last. An entry without
`type` overrides the declared layer at its index (`content`, `corner`,
`visible`, `opacity`, `offset_x`, `offset_y`); one past the end is ignored.
The per-frame keys work beside `type` too. A relative image `path` resolves
against the script's own directory. A declaration that does not parse (an
unknown `type`, a missing `content`) is a script error: the graphic falls back
to its declared layers for that frame, which for an item overlay means it
draws nothing.

```rhai
#{ layers: [
    #{ type: "text", content: vars.title ?? "", corner: "bottom_left", font_size: 40.0 },
    #{ type: "image", path: "card-logo.png", corner: "top_right", height: 64, opacity: 0.8 },
] }
```

**Text layers.** A `text` layer draws one block of text. `\n` in `content` breaks
a line, and the block is as wide as its widest line:

| Key | Default | Meaning |
|---|---|---|
| `align` | `left` | `left`, `center` or `right`: where a shorter line sits against the widest one. |
| `bold` | `false` | the whole block in the bundled Inter Bold. |
| `italic` | `false` | the whole block slanted. Inter has no italic cut, so this is a synthesized oblique. |
| `markup` | `false` | read `*italic*` and `**bold**` spans in `content`; the markers are not drawn, and an unpaired one stays literal and spans do not nest. |
| `visible_lines` | all | draw only the first N lines. The block is still measured and placed whole, so a line appearing never moves the lines already up. Settable per frame on a declared layer. |
| `below` | none | the index (in the frame's layer list) of an earlier text layer to hang this one under: top edge on its bottom edge, and the `align` edge (left, center, right) flush with the same edge of that layer's box. `corner` and `margin` are ignored; `offset_x`/`offset_y` still apply. The other layer is measured from its whole content, so a credit sits under the widest line even while later lines are still hidden. A block whose `below` target is not drawn this frame is not drawn. |

`corner: center` centers the block on the frame, for text, images and the
watermark alike. `crates/etv-overlay/fixtures/scripts/text_card.rhai` is a
working card built from these (centered lines revealed from `item_elapsed`, an
italic span, a right-aligned credit); preview it with
`etv-overlay render-still --config crates/etv-overlay/fixtures/text_card.yaml
--time 1 --item-elapsed 20 --item-remaining 10 --output card.png`.

### Pool `label` / `advisory` — naming the block on screen

A pool can say what it is called on air, and warn about what it contains:

```yaml
pools:
  - name: zombies
    expr: item.collections.contains("Zombies")
    label: "The Zombies Block"
    advisory: "Contains graphic violence"
```

| Key | Type | Meaning |
|---|---|---|
| `label` | string | display text for the block. Blank is refused at load. |
| `advisory` | string | a short content warning. Needs a `label`; blank is refused at load. |

Every item the pool draws carries both in its playout `metadata`, under two
fixed keys:

```json
"metadata": { "pool_label": "The Zombies Block", "pool_advisory": "Contains graphic violence" }
```

`pool_advisory` is absent when the pool sets none; both are absent on a pool
with no `label`. They sit beside the `audit` array and any keys a scorer
plugin attached — a plugin that returns its own `pool_label` loses it to the
pool's. This works the same on `pattern:` and `sequencer:` blocks. It is
display text for an overlay script, not part of the audit trail: nothing
parses `audit` to find the block. Metadata is filed by `entry_id`, so two
pools that draw the same entry share one label — including a labelled pool and
an unlabelled one: every airing of that entry, whichever pool drew it, carries
the labelled pool's label. The same limit `guide:` and `tracks:` have; keep
pools that must stay apart disjoint. Editing a label
rolls the schedule from the next item boundary, like any pool edit.

**On screen.** `deploy/appdata/shared/block-card.rhai` reads the two keys and
draws a card at the top-left of the frame:

```
THIS IS
The Zombies Block
Contains graphic violence
```

It shows in the first seconds of any item whose label differs from the
previous item's (a block starting), and again every `interval_secs` so a viewer
who tuned in mid-block still learns which one it is. The advisory row appears
only when the pool has one, and the card holds longer to give it time to be
read. It sits top-left so it can share the screen with the Now/Next snipe at
the bottom-left; the two are separate graphics. Declare it once at the station
level and every channel gets it, drawing nothing for items from an unlabelled
pool:

```yaml
overlay:
  width: 1280
  height: 720
  framerate: 30
  pixel_format: rgba8
  graphics:
    - name: now-next
      script: shared/now-next-snipe.rhai
      layers: [...]              # scrim, label, value, sub
    - name: block-card
      script: shared/block-card.rhai
      config:
        interval_secs: 300       # seconds between interval cards
        offset_secs: 150         # where in that interval the card starts
      layers:
        - { type: scrim, edge: top, size: 150, color: [0, 0, 0, 175] }
        - { type: text, content: "", font_family: Helvetica, font_size: 14.0, letter_spacing: 1.3,
            color: [255, 255, 255, 150], corner: top_left, margin: 40 }   # eyebrow
        - { type: text, content: "", font_family: Helvetica, font_size: 30.0,
            color: [255, 255, 255, 245], corner: top_left, margin: 40 }   # label
        - { type: text, content: "", font_family: Helvetica, font_size: 16.0,
            color: [255, 205, 90, 235], corner: top_left, margin: 40 }    # advisory
```

The script's `config` keys (all optional): `interval_secs` (300),
`offset_secs` (150), `hold_secs` (5.0), `advisory_hold_secs` (8.0),
`rise_secs`, `fall_secs`, `stagger_secs`, `slide_distance_px`, `overshoot`,
`row_px`, `eyebrow` ("THIS IS"), `max_chars` (40), `announce_after_unlabelled`
(true).

An item from an unlabelled pool has an empty label, so by default it counts as
a change from a labelled neighbour: in a pattern of `films x1, bumpers x1` where
only `films` is labelled, every film follows a bumper and draws the card. Set
`announce_after_unlabelled: false` to compare only against a labelled previous
item; the card then does not land on a block that follows unlabelled
programming, and the interval card is the only one there. Preview it with
`METADATA='{"pool_label":"The Zombies Block"}' PREV_METADATA='{"pool_label":"The Kaiju Block"}' ITEM_ELAPSED=2 admin overlay-watch <channel>`.

### Rating badge

`deploy/appdata/shared/rating-badge.rhai` is a graphic on its own: it draws the
airing item's content rating — the MPAA code on a film, the TV Parental
Guideline on an episode — as the small boxed icon a broadcaster puts in a
corner when a programme starts. It reads one thing, the `content_rating` the
catalog took from Plex (already in each playout item's `program`), and knows
nothing about any other graphic, so it layers with the snipe, the block card or
a channel logo like any other entry in `graphics:`.

It shows for the first 15s of each item (0.5s fade in, 0.8s out), and again
every `repeat_secs` of a long item when that is set. It draws a badge for the
eleven ratings it has art for and nothing for anything else — `Not Rated`,
`Approved`, `16+`, or no rating at all. A country prefix (`us/TV-MA`) is
ignored.

```yaml
graphics:
  - name: rating-badge
    script: shared/rating-badge.rhai
    config:
      hold_secs: 15          # all optional: hold_secs, fade_in_secs, fade_out_secs, repeat_secs
    layers:                  # one image per rating, in exactly this order
      - { type: image, path: shared/ratings/tv-y.png,  corner: top_right, margin: 32, height: 56 }
      - { type: image, path: shared/ratings/tv-y7.png, corner: top_right, margin: 32, height: 56 }
      - { type: image, path: shared/ratings/tv-g.png,  corner: top_right, margin: 32, height: 56 }
      - { type: image, path: shared/ratings/tv-pg.png, corner: top_right, margin: 32, height: 56 }
      - { type: image, path: shared/ratings/tv-14.png, corner: top_right, margin: 32, height: 56 }
      - { type: image, path: shared/ratings/tv-ma.png, corner: top_right, margin: 32, height: 56 }
      - { type: image, path: shared/ratings/g.png,     corner: top_right, margin: 32, height: 56 }
      - { type: image, path: shared/ratings/pg.png,    corner: top_right, margin: 32, height: 56 }
      - { type: image, path: shared/ratings/pg-13.png, corner: top_right, margin: 32, height: 56 }
      - { type: image, path: shared/ratings/r.png,     corner: top_right, margin: 32, height: 56 }
      - { type: image, path: shared/ratings/nc-17.png, corner: top_right, margin: 32, height: 56 }
```

The script addresses the eleven layers by index, so the order above is a
contract. Move the badge by changing `corner` and `margin` on every layer; it
sits top-right by default so it does not share a corner with the block card
(top-left) or the snipe (bottom-left). The art is `deploy/appdata/shared/ratings/*.png`,
drawn by `tools/make-rating-icons.sh` and committed; restyle it by editing that
script and running it, or replace a PNG with any 8-bit RGBA image of the same
name. The plates are plain boxes in the shape of the on-screen US icons, not the
MPA's or the TV Parental Guidelines board's own artwork. Preview one with
`etv-overlay render-still --content-rating TV-MA --item-elapsed 3`.

### Pool `plugin` — items chosen by a scorer script

A pool normally names an `expr`, a CEL expression the catalog resolves. It can
instead name a `plugin`: a Rhai script that runs its own queries, ranks what it
finds, and returns the ordered set. The two are mutually exclusive — a pool that
sets both, or neither, fails at load.

```yaml
pools:
  - name: foryou
    plugin: "../plugins/taste-cosine.rhai"
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
`examples/samples/foryou.yaml`'s scorer (#254) holds no replay policy of its own
at all — a legitimate choice under this ADR, not a gap — and declines the field
for the reason the sample's own comments give: `cycles` already drains the
largest pool once per window, so a no-repeat rule here would just force the
whole movie library into one window. `examples/samples/kungfu.yaml` is the
sample that exercises the field, on CEL pools.

The script defines five functions:

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

// Returns an envelope: `picks`, most-wanted first — a bare entry_id string
// or, per entry, a record widening what a bare id can say (#166):
// `#{ entry_id: "…", metadata: #{…}, take: 3 }` — plus `workspace`, an
// opaque value the station carries unread to audit() below, for the length
// of this one generation (#389, ADR 0013). A script with nothing to hand
// forward still returns the key, e.g. `workspace: ()`.
fn pick(ctx) {
    #{ picks: […], workspace: … }
}

// Called once per generation, immediately after pick(), handed back the
// list pick() returned plus the workspace it produced (#389, ADR 0011).
// Required of every pool_provider — a script that ranks but cannot explain
// itself fails to load. Returns one stage record per entry_id it wants to
// explain, keyed by that id; an id audit() names that pick() never
// returned is refused, naming the entry.
fn audit(ctx, picks, workspace) {
    #{ "ghost": #{ stage: "pool", by: "plugin:example", verdict: "ranked highest" } }
}
```

#### Pool `sources` — the channel says which items the script may rank (#210)

A pool can write the candidate queries itself, instead of taking whatever the
script's `sources()` declares:

```yaml
pools:
  - name: movies
    plugin: "../plugins/taste-cosine.rhai"
    sources:
      movies: 'item.type == "movie" && item.library == "4K Movies"'
    capabilities: [catalog_read, watch_history]
```

Each key is a set name the script reads back as `ctx.sets.<name>`; each value is
a CEL expression resolved against the catalog exactly like a pool's `expr`. The
table **replaces** the script's `sources()` wholesale — that function is not
called at all — on the same terms as a pool's `constraints` replacing the
block's: what the pool wrote is the whole list of what it offers its scorer.

This is what lets a channel say which slice of the library a taste pool draws
from without editing the script. `taste-cosine.rhai` declares
`item.type == "movie"`, which spans every movie library on the server; a channel
that should only surface one of them cannot say so any other way. The script
cannot narrow it for one channel without narrowing it for every channel pointed
at it, and which libraries a station keeps is a fact about the station, not about
how taste is computed. Picking the candidates is the config's half everywhere
else (`expr`, `groups`); this is that half restored for a plugin pool, leaving
the script the ranking (ADR 0002).

Reach for it when the narrowing really is station-specific. It is the wrong tool
for a distinction the catalog should be drawing itself: concerts and home videos
were once excluded here by name, and the fix was to stop calling them movies
(#214, see [`type` above](#item-fields)), not to repeat an exclusion list in
every query.

The set **names** belong to the script, not to the schema: nothing checks that a
name means anything to it, and one it does not recognise simply arrives as
another entry in `ctx.sets`. `taste-cosine.rhai` offers two, `movies` and
`shows`, and a pool picks which one it ranks with `unit: "movie"` (the
default) or `unit: "show"` in its `config:` — so an override written for it
keeps whichever of those two names that pool uses.

There is a third value, `unit: "season"`. It ranks series exactly as
`"show"` does and returns exactly the same episodes — it differs only in the
order the blocks are laid down: every picked show's season 1 in ranked order,
then every show's season 2, and so on. Pick it when a visit is a whole season
(`take: all` with `group_by: season`), where show-major ordering would work
through every season of the top-ranked show before the rotation reached the
second show at all. `001-for-you` uses it; `002`/`003` use `"show"`, which
suits their `take: 3`.

A `unit: "show"` pool ranks the **series**, not the episode. An episode
carries no `keywords` of its own, so the script resolves each one to its
show through the catalog's GUID-derived `show_id` (#274 — the change that
made this possible; before it, `show_id` came from the show's title and
joined to nothing) and scores the show, then hands back the top `show_count`
shows with every episode in season/episode order. Because the station groups
a returned list into series by first appearance, that order becomes the
rotation order: such a pool wants `select: round_robin` and `group_by: show`,
where `random` would discard the ranking at the last step and `season` would
cut a show into per-season series. Narrow `sources:` to the one set the pool
uses, or it resolves both — 74,815 episodes loaded for a movies pool that
never looks at them.

Two pools naming the same script with different `sources` each resolve their own
set, so narrowing one pool never narrows its sibling. Two that wrote the same
table share one resolution.

Rejected at load: `sources` on a pool with no `plugin` (an `expr` or `groups`
pool has no script to hand sets to — narrow it with `expr` instead), an empty
table, an empty set name, and an empty expression. A CEL expression that fails to
resolve fails the channel's generation naming the pool and the set, and says
`channel-authored source` rather than naming the script, so the error points at
the file the expression is actually written in.

#### Pool `profile`, `profile_files` and `ignore_keywords` — a taste profile

A plugin pool can carry a **taste profile**: signed weights on keywords, catalog
tag values, single items and CEL-defined sets, plus exclusions that remove a tag
value's or an item's titles from the pool. The station resolves every reference
and hands the weighted ones to the script as `ctx.profile`. The script does all
the weight math (ADR 0002).

```yaml
- name: movies
  plugin: "../plugins/taste-cosine.rhai"
  profile_files: ["../profiles/pierce.yaml"]
  profile:
    - { keyword: heist,            weight:  2.0 }
    - { genre: Horror,             weight: -1.0 }
    - { director: Michael Mann,    weight:  1.0 }
    - { item: "imdb:tt0113277",    weight: -1.0 }
    - { item: "Grown Ups (2010)",  weight: -1.0 }
    - { set: 'item.collections.contains("Guilty Pleasures")', weight: 3.0 }
    - { genre: Animation,          exclude: true }
    - { item: "Cats (2019)",       exclude: true }
  ignore_keywords: [duringcreditsstinger]
```

**An entry** names exactly one reference and one of a `weight`, `exclude: true` or `require: true`:

| Key | Resolves to |
|---|---|
| `keyword` | the value lowercased, trimmed, whitespace collapsed, then looked up in the pool's first granted datastore's `keyword_forms` — its stored, stemmed form (`Heists` resolves because the store recorded that spelling, not because the station stems it) |
| `genre`, `label`, `cast`, `director`, `writer`, `producer`, `country`, `studio`, `content_rating` | the value lowercased, under the item-map key it is read from (`genres`, `labels`, `cast`, `directors`, `writers`, `producers`, `countries`, `studio`, `content_rating`) |
| `item` | one catalog entry, by external id (`imdb:…`, `tmdb:…`, `tvdb:…`, or an `entry_id` of that shape) or by exact `Title (Year)` among non-episodes |
| `set` | every entry a CEL expression matches, resolved like `sources` |

`weight` must be finite and non-zero; its sign says favor or disfavor.

**`exclude: true`** in place of a `weight` removes every title the reference
matches from the pool's candidates. It takes a catalog tag (matched against the
catalog's own value, ignoring case for ASCII letters only — `horror` matches
`Horror`, `élodie` does not match `Élodie`), an `item`, or a `keyword` — never a
`set`. A keyword resolves its spelling through `keyword_forms`, then matches
every title whose movie or show carries that keyword in the pool's first
granted datastore; an episode matches by its show's keywords, since it has none
of its own, so `{ keyword: christmas, exclude: true }` removes Christmas films
and every episode of a show tagged `christmas`. The station drops the matched entry ids from each of the
pool's resolved `sources` sets before the script runs, so an excluded title is
absent from `ctx.sets` and can never be picked — where a negative `weight` only
scores it down and leaves it a candidate. An exclusion is not in `ctx.profile`.
**`require: true`** is the reverse: it keeps only the titles the reference
matches. It takes the same references as `exclude` (a keyword, a catalog tag or
an `item`, never a `set`), and several `require` entries combine as "matches at
least one of them". A themed pool lists its subjects and the scorer ranks only
what carries one, by house taste as usual:

```yaml
profile:
  - { keyword: halloween, require: true }
  - { keyword: haunted house, require: true }
  - { keyword: witch, require: true }
  - { keyword: halloween, weight: 3.0 }   # optional: rank the most on-theme first
```

A title both required and excluded is excluded. A `require` entry is not in
`ctx.profile` either.

Two pools naming one script share resolved sets only when they narrow them the
same way. Each resolved exclusion logs `profile exclusion resolved` and each
requirement `profile requirement resolved` (pool, reference, origin, `matched`)
at info, and each set they narrow logs `profile narrowing removed candidates`
(`candidates`, `removed`, `remaining`) at debug; `taste-debug` prints each
exclusion and its match count under the profile.

**`profile_files`** are YAML lists of entries, relative to the channel file.
Their entries arrive first, in list order, then the inline `profile`. A file
holds a person's standing likes and dislikes, and several pools can name it.

**`ignore_keywords`** go through the `keyword` rule and reach the script as
`ctx.ignore_keywords`. A keyword ignored this way stops counting toward the
score; the titles carrying it stay candidates. To remove those titles, write
`{ keyword: …, exclude: true }` in `profile:` instead.

**`ctx.profile`** is one map per entry: `kind` (`keyword`, `tag`, `item`,
`set`), `namespace` and `value` for a keyword or tag, `items` (full item maps,
the same shape as `ctx.sets`) for an item or set, `weight`, `reference` (as
written) and `origin` (the profile file's path as written, or `inline`). A pool
with no profile reads empty arrays. Reading `ctx.profile` needs the
`catalog_read` capability, the same as `ctx.sets`, since `item` and `set`
entries carry catalog item maps; `ctx.ignore_keywords` is not gated.

Rejected at load, naming the pool, origin and position: any of the three on a
pool with no `plugin`; an entry naming zero or two references, an empty value,
an unknown key, a zero or non-finite weight, or an `item` in neither shape;
`exclude: true` beside a `weight` or on a `set`, or `exclude: false`; `require: true` beside a `weight` or `exclude`, or on a `set`, or `require: false`;
an unreadable profile file; an empty `ignore_keywords` value. Failing the
generation, naming the entry and the spelling: an `item` matching no entry or
several (listed); a `set` that fails to resolve or matches nothing; an
excluded tag value no catalog entry carries; a
`keyword` or `ignore_keywords` spelling `keyword_forms` has never seen, or a
`keyword`/`ignore_keywords` entry on a pool that grants no datastore.

#### How `taste-cosine.rhai` scores a taste profile

`taste-cosine.rhai` turns `ctx.profile` into two weight maps and adds them to
the watch-history taste term:

```text
score = (taste_weight · taste + favor) / (1 + unusual_weight · house + disfavor)
```

**Netting.** Every entry adds its weight to one or more `(namespace, value)`
pairs. A `keyword` or tag entry adds its whole weight to its one pair. An
`item` or `set` entry spreads its weight over every keyword and genre its
members carry, each pair getting `weight × (members carrying it / member
count)` — so a 20-film set and a 200-film set with the same shares weight
every candidate identically. The pairs are summed across entries: a net above
zero goes to the **favor** map, below zero its size goes to the **disfavor**
map, and exactly zero is dropped. `{ keyword: heist, weight: 2 }` plus
`{ item: "Heat (1995)", weight: -1 }`, where heist is Heat's one keyword,
leaves heist favored by 1.

**Scoring.** `favor` is the sum over namespaces of a candidate's cosine
against the favor map in that namespace: the idf-scaled weights of the
candidate's own values that the map holds, divided by the square root of how
many values the candidate has in that namespace. idf (`1 + ln(documents /
documents carrying the value)`) is computed per namespace over the pool's
candidates. `disfavor` is the same against the disfavor map. Disfavor sits
under the fraction, so a profile of nothing but negative weights can pull a
score toward zero and never below it.

A candidate's keywords are its `keywords` in the granted datastore; its
tag values come from its catalog item map, lowercased. On a `unit: show` pool
a show's tag values are the union over its episodes — every episode carries
its show's genres.

**`config.taste_weight`** (default `1.0`, refused below `0`) scales the taste term. The taste
vector is divided by its own strongest weight first, so its top keyword weighs
1.0 on every account and a profile weight of `1` means "as strong as my
strongest taste". At `0` the taste vector is never read and the pool ranks on
its profile alone — a pool built around a seed list with no watch history
involved.

Every pick's audit detail carries `taste_weight`, `favor_score`,
`disfavor_score`, `favor_matches` and `disfavor_matches` (the five
`namespace: value` pairs that weighed most on each side), and `profile_entries` — each entry
that fed either term, with its `kind`, `reference` as written, `weight` and
`origin`. The verdict gains `, tilted toward the taste profile's favor` and
`, held back by the taste profile's disfavor` when the pick matched that side,
and a `taste_weight: 0` pool's verdict reads `ranked by the taste profile
alone`. The playout `metadata` carries `favor_score` and `disfavor_score`.

#### `seen` and `unusual_weight` — two pools, two pitches (#411)

`taste-cosine.rhai` takes two more `config` keys, both aimed at one channel
running a *comfort* movie pool and a *discovery* movie pool side by side:

```yaml
- name: comfort
  config: { seen: only }
- name: discovery
  config: { seen: exclude, unusual_weight: 0.1 }
```

**`seen`** partitions the candidates on whether the channel's account has ever
played the unit — `only`, `exclude`, or `any` (the default). It partitions;
it does not score, so #254's rule that Plex watch state never lowers a score
is untouched.

This is what makes two movie pools legal in one block. [Pools are blind to
each other](#pool-constraints-spacing-counted-in-a-pools-own-draw-order), so
two that can resolve the same `entry_id` may both air it in one day and
neither one's `no_repeat_within` will notice. `only` and `exclude` cannot
overlap, so the two are disjoint by construction.

Watch state comes from the granted datastore's whole `plays` table
(`watched_units_for`, plex-db-ex#60), not from `ctx.history`, which is the
newest 1000 Tautulli rows and would call a film watched two years ago unseen.
A pool with no `ctx.account_id` — a pooled channel — partitions on what the
*house* has played, which is the closest true answer available to it; the
script logs a `score.log` line saying so each generation. On a
`unit: show` pool the partition keys on the series, matching the unit ranked.

**`unusual_weight`** divides the score by `1 + unusual_weight × house cosine`,
discounting a title the whole house has a habit of watching. It divides rather
than subtracts: a subtraction drives scores negative, and a negative score
both sorts below a title carrying no keywords at all and inverts the recency
damping, which is a multiply by a number in (0, 1]. Clamping at zero is worse
still — it collapses every penalised title into one bucket.

It is scale-free, a ratio against 1.0 rather than against the house
vector's magnitude, so useful values are small. Measured on
`002-for-pierce`, watching for the failure mode where titles the account's own
taste says nothing about float up purely because the house dislikes them:
`0.05`–`0.25` are clean, `0.5` puts 8 such titles in 42 picks, `1.0` puts 22
in 43. On a pooled channel the taste vector already is the house vector, so a
nonzero value there fails the generation with an error instead of doing nothing.

Every pick's audit detail carries `house_score`, `unusual_weight` and `seen`,
and the verdict names the half the pick came from — so `admin audit` says
which of two sibling pools aired something without the reader holding the
config.

#### `why` — an on-screen reason, worded by the pool

Three more `config` keys give each pick a short reason an overlay can draw.
The wording lives in the channel file, so each block says it its own way:

```yaml
- name: new-to-everyone
  config:
    seen: exclude
    why: "Nobody here has seen it"        # a ranked pick
    why_random: "Something different"     # an exploration draw (default: why)
    why_random_detail: "picked at random" # under it (this is the default)
```

A pool that sets `why` writes two keys into each pick's playout `metadata`:
`why`, the line, and `why_detail`, the text under it. A ranked pick's detail
is its two heaviest matching keywords joined with ` · `, with ` film` and
` television series` trimmed off and any keyword over three words skipped
(`comedy · drama`); an exploration draw's is `why_random_detail`. A pool that
sets no `why` writes neither. The station carries both keys through untouched
and never reads them; `deploy/appdata/shared/corner-sequence.rhai` is one
overlay that draws them.

**A reason can be a watched title.** On a movies pool that sets `why`, the
scorer also looks, once per generation, for the most recently watched movie in
`ctx.history` that carries one of the pick's matched keywords (heaviest first).
When one exists, `why_detail` leads with that title and follows with the
keyword that linked them (`Blade Runner · neo-noir`), and the pick's metadata gains
`why_bridge`: `{ title, year, watcher, via }`, where `watcher` is who watched
it (empty on a pooled history row that names nobody) and `via` the shared
keyword. A channel that wants to say the name reads `why_bridge.watcher`; one
that doesn't ignores it. A watch qualifies only inside `bridge_window_days`
(pool `config:`, default `30`). A pick with no qualifying title, an
exploration draw, and a title that would cite itself keep the keyword detail
and carry no `why_bridge`. The overlay is unchanged.

#### `ignore_keywords` — tags that describe packaging, not subject

```yaml
ignore_keywords: [duringcreditsstinger]
```

A pool field beside `profile` (see above), reaching `taste-cosine.rhai` as
`ctx.ignore_keywords`. The script refuses an `ignore_keywords` written under
`config:`. Each entry matches one whole keyword exactly:
`duringcreditsstinger` drops that tag and nothing else, never a substring such
as `stinger`.

The keyword is dropped everywhere it enters the score: the taste, house and
profile maps, and each candidate's own keyword list. That last part matters. A
keyword removed from the maps but kept on candidates still counts toward a
candidate's length in the cosine divisor, which lowers the score of every film
that carries it. A candidate left with no keywords at all is treated like one
that never had any: it is not counted in the document-frequency pass, and the
exploration slot cannot draw it.

It exists for tags like `duringcreditsstinger`, which marks a mid-credits
scene. That tag appears alongside `superhero` and `comic`, so leaving it in
counts the same taste twice. Empty by default.

#### The record shape — `metadata` and a per-entry `take` (#166)

Each element `pick()`'s `picks` array holds may be a bare `entry_id` string —
unchanged since #74 — or a record naming one plus two optional extras:

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

The widening is additive: a script returning only bare ids needs no edit. A `sequencer:` block's plugin pool record shape parses the same way as a `pattern:` block's, and its `metadata` reaches the emitted JSON identically (#201). The per-entry `take` override remains out of scope for a sequencer block: `arrange()` already decides its own order, so what a `take` override would even mean there is a separate design question, not a plumbing gap (#201).

#### `pick()`'s envelope and `workspace` (#389, ADR 0013)

`pick()` returns `#{ picks: [...], workspace: <opaque> }`, not a bare array —
there is no compatibility path, and a script returning one fails to load
naming the shape it must return instead. `picks` is the array documented
above; `workspace` is a value the station holds for the length of one
generation and hands back, unread, as `audit()`'s third argument. A script
with nothing to say puts `()` there.

A scorer plugin may carry its working set from `pick()` to `audit()` this
way, and that value lives for exactly one generation — nothing carries over
to the next, and `--check-determinism` diffs the resulting audit trail
alongside the schedule.

#### `audit(ctx, picks, workspace)` — a plugin explains its picks (#389, ADR 0011)

Every scheduled item carries an `audit` array in its `metadata` — one record
per *selection stage* (`docs/CONTEXT.md`) that acted on it, in the order they
acted. A `pool_provider` plugin produces its own record from this
fifth contract function, called once per generation, immediately after
`pick()`, and handed back the list `pick()` returned plus the `workspace` it
produced. Required of every `pool_provider`: a script declaring the hook but
implementing no `audit()` fails to load, naming the missing function — the
same load-time refusal `hooks()` itself gets.

`audit()` returns a map keyed by `entry_id`, one stage record per id it wants
to explain:

```rhai
#{ "ghost": #{ stage: "pool", by: "plugin:example", verdict: "ranked highest", detail: #{ score: 0.82 } } }
```

- `stage` — one of a closed, station-owned set: `pool`, `select`,
  `constraint`, `pattern`. A `pool_provider`'s own `audit()` only ever emits
  `"pool"` today; the other three name built-in machinery not yet
  instrumented against this trail. An unknown value is refused at pick time,
  naming the entry.
- `by` — which instance acted, e.g. `"plugin:taste-cosine"`. A plain string,
  never inspected.
- `verdict` — one line in the producer's own words. Never read by station
  code — a script that returns `"picked"` satisfies the contract just as
  well as one that explains itself in detail; the station cannot tell a
  lazy record from a thorough one.
- `guide` — optional, a non-empty string of at most 200 characters. Unlike
  `verdict` and `detail`, the station **does** read this one: it is the
  viewer-facing "why this was picked" sentence shown in the guide when the
  channel turns on `scoring.explain` (see below). Refused at pick time if
  present but empty or too long, naming the entry and the length it got.
- `detail` — opaque, the same treatment `metadata` gets: converted and
  carried untouched, refusing a non-finite float anywhere inside and naming
  the key. Optional.

An `entry_id` `audit()` names that `pick()` never returned is refused, the
same "cheaper to reject than explain" posture as a duplicate pick. Each
record lands under that item's `metadata.audit`, as the first element of an
ordered array — sibling to a viewer-facing `reason_set` key, never merged
with it (ADR 0012). Both shipped scripts now fill `detail` in their own
vocabulary (#392): `taste-cosine.rhai` stashes each pick's score, rank,
candidate-set size and draw kind (`"ranked"` or `"exploration"`) in
`workspace` inside `pick()` and reports them back in `audit()`. It reports
the score twice over — `base_score` is the raw keyword cosine and
`recency_factor` the multiplier it damped that by, alongside `aired_rank`,
the pick's distance back in `ctx.recent` or null for a title outside the
tail. Both halves are there because a title that fell twenty places for
having aired last night only reads as a decision when the cosine and the
factor are visible apart; one number alone reads as the scorer changing its
mind. Meanwhile,
`endless-distance.rhai` does the same with the distance it walked by. The
station reads none of these keys — it still only enforces the shared
non-finite-float refusal — so a script adding another key to `detail` needs
no station change to have it appear.

`detail` also carries a bounded `near_misses` array (#393): the candidates
that placed above or near a pick and did not air, each `#{ id, score |
distance, reason }`. The bound is a number the script itself states — a
tunable with a default, overridable per pool via `config:`, the same
mechanism `exploration_fraction` and `drift` already use — not a station
limit, so `near_misses` can never grow a chunk file without bound on a large
library. The `reason` strings are the script's own vocabulary
(`taste-cosine.rhai`: "already placed by an earlier slot in this
generation" or "passed over: this slot drew the seeded exploration coin
flip"; `endless-distance.rhai`: "inside the band but lost the seeded
tie-break" or "outside this step's distance band") — the station neither
supplies nor validates them, the same opaque treatment as the rest of
`detail`. Naming every rejected candidate remains out of scope; a full
candidate dump stays `taste-debug`'s job.

#### `hooks()` — what a script says it can do

A plugin declares its hooks and the station wires only the declared ones
(#159). These names exist:

| Hook | What a script implementing it does |
|---|---|
| `pool_provider` | supplies a pool's items. This is what a `plugin:` pool has always meant, so a script named there must declare it. |
| `sequencer` | takes a block's resolved pools plus the generation window and emits the block's final timeline in place of the pattern walk (#169). A block names one under `sequencer:`; see [Block `sequencer`](#block-sequencer-a-plugin-arranges-the-block-itself). |
| `bumper` | `build(ctx)` returns a `timeline()` of segments for one gap. `ctx` holds `gap_ms`, `next` and `prev` (item maps; `next` is `()` at the generation frontier), `channel` (`name`, `number`, `logo`, `vocabulary`) and `rng`, seeded from the channel, `next.entry_id` and the air date so one airing always resolves the same way. The builder (`segment()`, `clip(path, in_ms, out_ms)`) takes seconds, `_ms` variants take milliseconds, and `looped()` stands in for `loop`, a Rhai keyword. An empty timeline is an error and the host substitutes black. `ctx.rng` offers `int(lo, hi)`, `float()`, `chance(p)`, `pick(array)` and `array.random(rng)`. Segments take `black()`, `color("#rrggbb")`, `clip(c)`, `still(path)`, `render(path)`, `fixed`, `flex`, `looped`, `optional`, `priority`, `overlay(script, vars)`, `own_audio`, `bed_audio`, `mute`; a timeline takes `push(segment)` and `audio_bed(source, opts)`. `ducked_bed()` errors until implemented. No channel config names a bumper script yet; `bumper::hook` in `crates/etv-station/src/bumper/hook.rs` runs one. A bumper plugin may declare `probe` (`ctx.probe(path)`), `read` (`ctx.read(path)`, a file in its own folder), `http` (`ctx.http(url, method, body)`, the same call and 10-second limit a pool's `http` has) and `exec` (`ctx.exec(program, args)` -> `#{ code, stdout, stderr }`, no stdin, killed and an error after 10 seconds; a nonzero `code` is the script's to read); `ctx.http` and `ctx.exec` together may spend 30 seconds per `build()` call, after which each further call errors at once, naming the budget; each must be both declared and granted. `parse_json(text)` needs no grant. |

A script may declare any of them. The declaration lives in the script, not in
the channel config, so swapping one scorer for another needs no YAML edit.

`hooks()` is read by compiling the script and calling that one function —
`sources()` and `pick()` never run — so all three refusals below happen at
config load rather than mid-generation:

- A `plugin:` pool naming a script that does not declare `pool_provider` is
  refused, naming the script and the hook it lacked:
  `pool "movies" names plugin …/taste-cosine.rhai via `plugin:`, which requires
  the plugin to declare the `pool_provider` hook, but it only declares: sequencer`
- A script declaring a name outside the table is refused, and the message lists
  the names that exist: `declares unknown hook "warp_drive" — known hooks are:
  pool_provider, sequencer`
- A script declaring an empty array is refused: `declares no hooks — a plugin
  must declare at least one of: pool_provider, sequencer`

#### Changing the plugin contract is a breaking change for deployed scripts

Every function a hook makes mandatory is declared once, in
`score::REQUIRED_HOOK_FNS` (`crates/etv-station/src/score.rs`). Today the list
holds one entry: a `pool_provider` must implement
`audit(ctx, picks, workspace)`.

**Adding an entry to that list reaches outside this checkout, and it takes the
whole station off the air when it lands unnoticed.** A pool naming a script
that lacks a required function fails config load; the container entrypoint
renders that config before ETV-next starts; so an old script plus a new binary
is a crash loop with every channel dark, not one degraded channel. That is what
happened on 2026-08-31: `audit()` became required, `examples/plugins/` was
updated, and the gitignored `deploy/appdata/plugins/` copy the host actually
loads was not. All 64 channels were off air for 27 minutes, 13:48–14:15 UTC
(9:48–10:15am ET), across 35 restarts. Every test passed the whole time,
because the test suite only ever sees `examples/`.

A change to the list therefore has to reach three sets of scripts:

| Where | How it is checked |
|---|---|
| `examples/plugins/*.rhai` | `cargo test` — `crates/etv-station/tests/plugin_dirs_satisfy_contract.rs` |
| `deploy/appdata/plugins/*.rhai` | the same test, which skips when the directory is absent (it is gitignored, so a worktree and a fresh clone have no copy) |
| whatever else is already on the host | `admin plugin-check` — nothing else can see these |

That third row is the one that matters. A host can carry scripts with no
counterpart here at all. `admin deploy files` removes them now — `[deploy]`
declares `owns = ["channels", "plugins", "shared", ...]` with `delete = true`,
so the sync prunes orphans inside those paths and leaves everything else on the
volume (`catalog.db`, `history.db`, the HLS working set) untouched. It did not
always: the station's plugin directory held `taste-engine.rhai` — a
`pool_provider` with no `audit()`, existing nowhere in this repo, and harmless
only because its single reference sat inside a YAML comment.

Pruning does not make `admin plugin-check` redundant. A deploy reconciles the
host to this checkout; the check answers what is on the host *right now*,
before a deploy, which is the state a running station is actually loading. No check that compares local files to their counterparts can express
that case, so `admin plugin-check` copies the host's whole plugin directory
down and walks it with `etv-station --check-plugins`, using the binary on your
branch rather than the one already deployed.

Run `admin plugin-check` before `admin deploy image` whenever the contract
changed — a new required function, a renamed hook, a new required capability.

#### `capabilities()` — which host inputs a script needs (#167)

Nothing is available to a script ambiently beyond `ctx.pool`, `ctx.config`,
`ctx.target_count`, `ctx.now`, `ctx.recent`, `ctx.seed`, and `ctx.account_id`
— the inputs every plugin pool receives with no declaration at all, plus the
global `parse_json(text)`, which turns JSON text into maps and arrays (`null`
becomes `()`). Three more are gated behind a declaration, and a fourth is
opened only by name:

| Capability | Gates | Declared as |
|---|---|---|
| `catalog_read` | `ctx.sets` — the items each `sources()` query matched; `ctx.entry_id_for(source, id)` — the `entry_id` the catalog filed under that `(source, source_id)` provenance row (`source` is `"plex"` or `"local_fs"`), or `()` when none matches, so a script joining its own external history (a Tautulli `rating_key`) resolves it without the daemon owning the join (ADR 0014) | `"catalog_read"` |
| `watch_history` | `ctx.history` — recent server-wide watch events | `"watch_history"` |
| `http` | `ctx.http(url, method, body)` — one HTTP call returning the response body as text; a 4xx/5xx, a transport failure or 10 seconds without a response is an error; one `pick()` call's `ctx.http` calls together may spend 30 seconds, after which each further call errors at once, naming the budget (ADR 0014) | `"http"` |
| a named external datastore | `ctx.datastore("name")` — a handle onto the vendored plex-db-ex reader's `enrichment_for`, `enrichment_for_many`, `edges_from`, `edges_to`, `tag_neighbours`, `taste_vector_for`, `pooled_taste_vector`, `watched_units_for`, and `watched_units` accessors (#181, #271, #411; `tag_neighbours(kind, tag)` returns `[#{ tag, shared, df }]` — the tags co-occurring with `tag` on the plex-db-ex tag network, ordered by `shared` descending; read `shared` as `n["shared"]`, since `shared` is a reserved word in Rhai; etv-station-xm8p.2); the grant also proves at load time that the store's location opens, at the schema version the reader crate understands | `#{ datastore: "name" }` |

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

`examples/plugins/taste-cosine.rhai` reads `ctx.sets` and reaches for a named
datastore, so it declares
`["catalog_read", "watch_history", #{ datastore: "taste" }]` (#254's interface
list — `pick()` reads `ctx.history` only to name a watched title as a
pick's reason, see `why` above); `examples/samples/foryou.yaml` grants
all three on the one pool that points at it.

`ctx` carries `ctx.sets.<name>` (the items each source matched — every column on
`entries` plus genres / cast / labels / … as arrays; requires `catalog_read`),
`ctx.pool` (the name of the pool asking, so one script can serve several pools
of a channel with different judgment per pool), `ctx.target_count` (how many
items the generation needs), `ctx.history` (recent server-wide watch events,
`#{entry_id, watched_at}`; requires `watch_history`), `ctx.recent` (what this
channel aired most recently, oldest first), `ctx.now` (unix seconds at
generation time), `ctx.config` (this pool's `config:` block — see below),
`ctx.seed` (the channel's resolved seed mixed with this pool's name — see
below), `ctx.temperature` (this pool's temperature for the period — see
below), and `ctx.account_id` (the numeric account id a `single_user`-scoped
channel resolved, or unit `()` on a pooled channel — see below).

#### `ctx.seed` — reproducible randomness (ADR 0005)

`ctx.seed` is an integer: the pool's resolved `seed` for the period being
ranked (see [Seed and temperature](#seed-and-temperature)) mixed with the
asking pool's name. It is the source of entropy meant for a script to shuffle with. Rhai's
own `timestamp()`/`elapsed()` are legal to read too — they answer a clock
fixed for this generation (seeded from `window_start`, never the real wall
clock, #357), not a source of nondeterminism — but they carry no seed of their
own, so a script wanting reproducible randomness should still derive it from
`ctx.seed` (see "The determinism contract").

Two pools of one channel naming the same script get different `ctx.seed`
values — mixing the pool's own name into the channel's seed is what keeps two
such pools from drawing the same sequence, which would put their exploration
slots in lockstep on air, forever. The same channel, seed and period produce
the identical `ctx.seed` across two generations, which is what lets a script
pick randomly from it and still pass `--check-determinism`; a rotating seed
hands the script a new value at each rollover.

`ctx.seed` is a plain integer, not a PRNG object — a script derives its own
pick from it with ordinary arithmetic (mix it further, take it modulo a
candidate count, and so on), the same way `examples/plugins/taste-cosine.rhai`
(#254) derives a seeded coin flip per output slot for its exploration draw.

#### `ctx.temperature` — how far the draw may stray from the ranking

`ctx.temperature` is a float ≥ 0: the pool's resolved `temperature` for the
period being ranked, `0` when nothing set one. The station gives it no meaning
of its own — loosening a ranking is ranking logic, so it is the script's.
`taste-cosine.rhai` reads it as a softmax temperature: each candidate's key is
`ln(score) / T` plus Gumbel noise drawn from `ctx.seed`, and the walk order is
those keys sorted — a draw without replacement where each next pick's chance is
proportional to `score^(1/T)`. At `0` that order is the score ranking exactly;
at `1` a title's chance is proportional to its score; higher still approaches a
uniform shuffle. Because the noise comes from `ctx.seed`, one period's order
reproduces and the next period's differs.

How much a given number moves depends on how scores spread in the pool. On
034 SyFy's films pools, whose candidate sets run to hundreds of titles, a
14-day web UI review put the median pick at score rank 7 at `0.1`, 15 at
`0.25`, and 21 at `0.5`.

#### `ctx.account_id` — the account a personal channel ranks against (#278, ADR 0005)

`ctx.account_id` is the numeric account id `single_user`-scoped watch history
resolved to that generation, or unit `()` on a pooled (`all_users`) channel. A
script tests presence with `ctx.account_id != ()`, the same idiom `ctx.config`
lookups already use for an absent key.

The plugin never resolves a username to an id itself — it has no Tautulli
access and no accessor for the store's account table. The station already
does: it narrows the same Tautulli fetch `ctx.history` comes from by exactly
this account (`taste_scope: single_user`, `user:`), so handing the resolved id
across is a value the station already holds, not work done on the script's
behalf (ADR 0005's own test).

`examples/plugins/taste-cosine.rhai` is what this exists for: it ranks against
`ds.taste_vector_for(ctx.account_id)` when the id is present and
`ds.pooled_taste_vector()` otherwise — one script, not two, since the cosine,
the exploration slot, and the eligibility rule are identical either way.

Not gated behind a capability, the same as `ctx.seed`: every `pool_provider`
script receives it, whether or not the channel is `single_user`-scoped.

### The determinism contract

A plugin must be a pure function of `(catalog, config, resume state, seed, external-store snapshot)` — the same inputs the station itself is pure over. Nothing in `ctx` is optional to use instead of an ambient read: `ctx.now` is unix seconds at generation time, handed in precisely so a script never has to call the clock itself, and `ctx.history` / `ctx.recent` are the only watch/replay state a script sees. Rhai's own `timestamp()`/`elapsed()` are pure too as of #357 — they read a clock fixed to this generation's `window_start`, not the real wall clock, so two generations of the same channel see the same value and a script reading either produces the same schedule both times. A script ranking by anything else not reachable through `ctx` — an unordered iteration whose order Rhai does not guarantee, for instance — can still produce a different schedule from the same inputs, and the schedule still looks completely valid, so nothing errors and nothing on screen says which run is "right".

`etv-station --check-determinism <channel>` generates the named channel twice from identical inputs (config, catalog snapshot, seed, and — the empty, stateless kind — resume state) and diffs the two resulting schedules. It reports identical, or the first airing position where they disagree and both entry ids there, so the failure names a place to start rather than a bare "not reproducible". It is a debug check, not part of normal generation.

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

`examples/plugins/taste-cosine.rhai` is written to both rules and says so in its
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
    plugin: "../plugins/taste-cosine.rhai"
    config:
      exploration_fraction: 0.2
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
whatever the script decides it means.** `exploration_fraction` is not a concept
etv-station has — it exists because `examples/plugins/taste-cosine.rhai` looks it
up (#254), and a different scorer would read entirely different keys. That is the
same argument that put taste in the plugin at all ([ADR 0002](./adr/0002-scorer-plugin-replaces-a-pool-expr.md)):
a station that validated these keys would be a party to the taste it exists not
to hold, and its list would need updating for every script anyone writes.

An absent `config:` arrives as an empty map rather than a missing key, so a
script can read `ctx.config.anything` unconditionally and get unit back. Which
is also the catch: **a mistyped key is silent.** `exploraton_fraction` reads as
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
overlay renderer's spec takes a `config` mapping on the same terms — arbitrary
nesting, nothing validated, absent means an empty map, typos silent — reaching
its Rhai script as a `config` constant rather than as `ctx.config`, because an
overlay script is evaluated against flat scope constants while a scorer receives
one `ctx` map. That is the only difference, and it follows from how each engine
is invoked rather than from a decision either side made. Both surfaces read
`config:` through the same deserializer (`config_carrier::deserialize_config`),
so they cannot drift on what a shape means the way two independent conversions
could.

```yaml
# the overlay spec named by a channel's overlay: { file: ... }
graphics:
  - script: lower-third.rhai
    config:
      corner: bottom-left
      font:
        family: Inter
        size: 42
```

The single thing in the bag that can fail: **a float that is not a finite
number is refused at load, naming the key.** `weight: .inf` — or `-.inf`,
`.nan` — in a pool's `config:` or an overlay's `config:` cannot be carried at
all, and would otherwise reach the script as unit while the author believed
they had written a number. So the channel or the spec fails to load instead,
with an error like:

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

Five knobs sit on the channel, under `scoring:`, all optional:

| Field | Default | Meaning |
|---|---|---|
| `recent_depth` | `200` | How many recently-aired entries reach `ctx.recent`. A channel with a deep library wants a long memory; a narrow one would starve on the same setting. `taste-cosine.rhai` damps a candidate against its position in this tail, so raising it lengthens how long an aired title stays suppressed and lowering it shortens that. |
| `nominal_item_secs` | `1800` | Nominal seconds per item, used only to size `ctx.target_count`. A channel of half-hour episodes and one of three-hour films need different numbers to ask for a sensible amount. |
| `taste_scope` | `all_users` | Whose watch history `ctx.history` carries. `all_users` pools every Tautulli account with no user dimension; `single_user` narrows it to the one account named in `user`. |
| `user` | — | The account `single_user` follows: a Tautulli username (`"bob"`) or a numeric user id (`"1234567"`). Which one it is is inferred — a value made entirely of digits is sent as `user_id`, anything else as `user`. |
| `explain` | `false` | Show a one-line "why this was picked" explanation in the guide, built from each item's `metadata.audit` trail. Off by default; see below. |

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

`single_user` also requires `PLEX_URL` and `PLEX_TOKEN` in the daemon's
environment, whatever the catalog ingests from and whatever datastores a scorer
plugin is granted. Tautulli's user id is translated to a Plex account id through
Plex, because the two differ only for the server owner and only Plex can say
which account that is. A missing variable fails at load.

What is *not* checked at load is whether the named account exists — that needs
the network, and this is a config pass. Tautulli answers an unknown user with an
empty history, which shows up at runtime as `rows=0` on that scope's
`tautulli.history` log line.

A channel with no scorer plugin stops there — `ctx.history` just arrives empty
and the generation proceeds, same as an unreachable Tautulli (#74). A channel
that names a scorer plugin goes further (#278): resolving `user` to the
numeric id `ctx.account_id` carries needs that same live fetch, and a name
that resolves to nobody fails that channel's generation loudly, naming the
user, every tick until it is fixed — never a silent fall back to the pooled
vector, which would make a channel called "For Pierce" rank against everyone's
viewing while looking perfectly healthy. See `ctx.account_id` above.

#### `explain` — why each item was picked

With `explain: true`, each item the channel schedules gets a "why" line
appended to its guide description, built from that item's own
`metadata.audit` trail (ADR 0011) — the same trail `admin audit` already
reads:

```
A hobbit sets out.

Because you loved The Wire
```

The line prefers the first `guide` sentence any audit stage record carries —
a plugin's own viewer-facing reason, written for this purpose — and falls
back to the first `pool`-stage record's `verdict`, capitalized and prefixed
`Why: `, when no record has a `guide`:

```
A hobbit sets out.

Why: Ranked highest
```

An item with no audit trail at all — nothing wrote one, or the channel has no
`plugin:` pool and no built-in stage instrumented yet — gets no line, and its
existing synopsis is never replaced, only appended to. `detail` is never read
to build this line; it stays opaque per ADR 0002.

`explain` is opt-in — a `verdict` is
written for `admin audit`'s operator reader, not a viewer, so surfacing it
(or a plugin's `guide`) to everyone watching the channel is a call made per
channel, in writing, not derived automatically.

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
    plugin: "../plugins/taste-cosine.rhai"
    capabilities: [catalog_read, watch_history]
```

| Field | Type | Meaning |
|---|---|---|
| `capabilities` | list of strings | the simple capabilities granted — `catalog_read`, `watch_history`, `http` |
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
        path: "${PLEXDB_SNAPSHOT_PATH}"
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
| `ctx.pools.<name>` | that pool's resolved items, in order, as full item maps — `entry_id`, title, duration, show, season, tags. Not bare ids: a script that arranges against the clock has to know how long each item runs. |
| `ctx.pool_config.<name>` | that pool's own `config:` block, passed through unread — the same generic carrier a scorer's `ctx.config` reaches (ADR 0002), not a new schema field. Empty map when the pool declares no `config:`. |
| `ctx.window.from` | the instant this generation begins airing, not the instant the daemon happens to be computing in |
| `ctx.resume.<pool>.next` | the series whose turn is next in that pool's rotation |
| `ctx.cursor` | the per-show cursors from the play-history db |
| `ctx.seed` | the channel's seed for this window's period, a non-negative integer — the script's source of reproducible randomness |
| `ctx.temperature` | the channel's temperature for that period — the script's to interpret |

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

### Channel `annotate` — a plugin attaches metadata to every item

A channel can name an `annotate:` script beside its `rule:`, with the
capabilities the channel grants it. After the whole
schedule resolves, the station calls the script's `annotate(ctx, item)` once per
item, in schedule order. The script never sees or changes ordering, selection
or timing; it only returns metadata to add.

```yaml
annotate:
  plugin: ratings.rhai          # relative to this channel.yaml
  capabilities: [http]          # must equal the script's capabilities()
```

```rhai
fn hooks() { ["annotate"] }
fn capabilities() { ["http"] }   // optional; `http` is the only capability offered

fn annotate(ctx, item) {
    // ctx.channel — the channel's name; ctx.http(url, method, body) with `http`
    // item        — the catalog item map, plus `block` and `metadata`
    #{ rating: 8 }               // keys to add, or () to add nothing
}
```

The returned map merges into the item's metadata key by key. A key the item
already carries (a `pool_provider`'s own `pick()` metadata, for one) keeps its
value, and the station logs `annotate.key_collision`. A script error leaves that
item unannotated and logs `annotate.failed`; the channel keeps airing. The
channel grants exactly the capabilities the script declares (ADR 0015), as a
pool does: a script declaring `http` with no `http` in `annotate.capabilities`
is refused at load, naming the script and the field, and so is a grant the
script never declares. `examples/plugins/attribution.rhai` is a worked
example: it asks Tautulli who has recently watched each item and returns
`#{ attribution: "Watched recently by bob and carol" }`, which an overlay script
reads as `metadata.attribution`. `ctx.http` calls across the whole pass share one 30-second budget; once it is spent, further calls error and those items stay unannotated.

### Pattern step `take` — how many items one visit draws

A step in a block's `pattern:` has four fields:

| Field | Required | Default | Meaning |
| --- | --- | --- | --- |
| `pool` | yes | none | name of a pool declared in the same block's `pools:` |
| `take` | yes | none | how many items one visit draws (forms below) |
| `from` | no | `start` | where in the series the visit cuts its items |
| `chance` | no | `1.0` | probability the step fires on a pass through the pattern |

`take` has three forms:

| Form | Example | One visit draws |
| --- | --- | --- |
| count | `take: 3` | exactly 3 items |
| range | `take: "2-4"` or `take: {min: 2, max: 4}` | a whole number from `min` to `max`, rolled per visit |
| `all` | `take: all` | every remaining item of the series the visit picked |

A range is rolled when the visit starts, from the channel seed, the pattern
cycle and the step's position. Each number from `min` to `max` is equally
likely, and a pinned `seed:` reproduces every visit's size. Two steps with the
same range roll independently, because the step's position is part of the roll.
A range is not `chance`: the visit always airs, only its length varies.

A range follows the same rules as a count wherever a count is checked:

- `min` must be at least 1 and `max` at most the engine's per-visit maximum
  (`MAX_TAKE`); `min` above `max` fails at load.
- `min` equal to `max` is a count (`"3-3"` loads as `take: 3`).
- Under `rotate: slot` the rolled number is how many slots the visit fills.
- A plugin's per-entry `take` override replaces the rolled number for that
  series, the same way it replaces a count.
- Adjacency `constraints` size their seam for the widest cycle the range can
  produce: every range counted at `max` for the cycle length and at `min` for
  how often the pool is drawn.

#### Pattern step `from` — where in the series a visit cuts

`from` takes one of three values and defaults to `start`:

| Value | One visit draws |
| --- | --- |
| `start` | the next `take` items from where the series left off: its resume point, or its top on a first visit. The only value that continues a series across visits |
| `end` | the last `take` items of the series, in list order. The position is absolute and ignores the resume point, so every visit airs the same final episodes |
| `random` | `take` consecutive items starting at a seeded random offset, in list order. Absolute like `end`, and re-rolled on every visit; a pinned `seed:` reproduces it |

`from: end` is not `order: "episode:desc"`: the order sort would also air the
episodes backwards, while `from` moves only the cut.

#### Pattern step `chance` — a step that fires only some passes

`chance` is a number from `0.0` to `1.0`, default `1.0`, which always fires. At
`0.25` the step fires on about one pass in four. The roll comes from the channel
`seed` and the step's position, so a pinned `seed:` reproduces the whole
skip/fire sequence. A skipped step airs nothing and does not advance the pool's
resume point. This differs from a `take` range, where the visit always airs and
only its length varies.

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
`item.type` — or the channel has to say so with `rule.no_repeat_within` (below),
because the pool contract does not guarantee it.

**`rule.no_repeat_within` spaces the whole channel.** Set on `rule`, beside
`blocks` and `slots`, it takes the same two spellings and counts **channel**
positions or wall-clock time across every pool, block and slot:

```yaml
rule:
  no_repeat_within: "7d"   # no film twice in a week, whichever pool draws it
  blocks:
    - pools: [ … ]
```

The pattern walk checks it at every draw, beside the drawing pool's own rule,
against one history: the play-history ledger's tail — every pool and slot that
aired — then what earlier blocks of the same generation laid (an earlier entries
block counts only the items its window airs), then each draw as it is made. A series whose next item fails it is passed over exactly as a
pool-rule clash is: a round-robin pool slides to the next series, a random pool
rerolls. When nothing passes both, the channel rule gives way first, so the
pool's own rule holds as it would without it; a draw that airs against the
channel rule is logged as `constraints.unsatisfied` with `scope: channel`, and
every generation it moved a draw logs `channel_repeat.applied` with how many
draws it moved off their first choice (`blocked`) and how many aired against it
anyway (`forced`).

It does not ask a pool's source for more: a pool that has aired everything it
holds refills as it always did, and a plugin pool already sees the channel's tail
in `ctx.recent`. Entries and sequencer blocks are counted in the history but
not constrained by it — a flat authored list and a script both own their order
— and a channel with no pattern block
fails its load if it sets the key. The tail the station keeps is sized to cover
it, so a `"7d"` rule holds across generation boundaries too.

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

## Track selection

`tracks:` says which audio stream and which subtitle stream of a file an item
plays. The station probes each local file, applies the rules, and writes the
winners into the playout item as `tracks.audio.stream_index` and
`tracks.subtitle.stream_index`. ETV-next receives integers and holds no policy
of its own. See [architecture](/architecture#subtitles-and-audio-tracks) for how
the pieces fit.

```yaml
tracks:
  audio:
    exclude:
      - { title: "commentary|director|isolated score" }
    prefer:
      - { lang: "^en", channels: 2 }     # English stereo first
      - { lang: "^en" }                  # then any English
  subtitle:
    exclude:
      - { title: "signs|songs only" }
    prefer:
      - { image: false, lang: "^en" }    # a text track: the viewer can switch it off
      - { image: true, lang: "^en",      # a picture track: burned in, cannot be switched off
          when: { audio_lang_not: "^en" } }   # ...only when the film's audio isn't English
```

`audio` and `subtitle` are separate lists. Each has:

| Key | Meaning |
|---|---|
| `exclude` | Streams matching any entry are removed first. |
| `prefer` | An ordered list. The first entry that matches a surviving stream wins; later entries are not read. Several streams matching the winning entry go to the one with the fewest channels, then the lowest index. |
| `inherit` | Default `true`. `false` discards every rule from the levels above this one, for this list only. |

An entry is a set of fields that must all hold. A field left out matches anything.

| Field | Matches | Form |
|---|---|---|
| `id` | the stream's index in the file | `2`, or `">=3"`, `"<8"`, `"!=0"` |
| `title` | the stream's title tag | regex, case-insensitive, unanchored |
| `lang` | the stream's language tag as stored (`eng`, `jpn`, `chi`) | regex. A stream with no tag, or `und`, has no language and never matches. |
| `codec` | the codec name (`ac3`, `subrip`, `hdmv_pgs_subtitle`) | regex |
| `channels` | audio channel count (audio lists only) | `2`, or `">=6"` |
| `default`, `forced` | the container's disposition flags | `true` / `false` |
| `image` | subtitle lists only: a picture subtitle (PGS, VobSub) | `true` / `false` |
| `when` | subtitle lists only: a condition on the audio stream that was selected | `audio_lang: <regex>`, `audio_lang_not: <regex>` |

`audio_lang_not` needs the audio stream to carry a language: an unknown
language is not foreign, so the entry does not match. The regex engine has no
lookahead, so "not English" is written with `audio_lang_not`, not `(?!en)`.

A bad regex or comparison fails config load, naming the level, the list, the
entry and the pattern: `block #2: tracks.subtitle.prefer[0].lang: invalid regex "(" ...`.

### What happens when nothing matches

- **Audio** falls back to the file's default-flagged audio stream, or its first
  one. If `prefer` entries exist and none matched, the station logs a
  `tracks.audio_unmatched` warning naming the item and file. If every audio
  stream matches an `exclude` entry, the station chooses among all of them and
  logs `tracks.audio_all_excluded`, because an item with no sound is worse than
  one with the wrong track.
- **Subtitles** have no fallback: no match means no subtitle track. A picture
  subtitle is never burned in because nothing else matched.

With no `tracks:` anywhere, the only built-in rule is a subtitle `prefer` of
`{ image: false }` at the bottom of the cascade: any text subtitle. A level can
drop it with `subtitle: { inherit: false }`.

### The cascade

Rules are written at five levels, most general first: the **station file**, the
**channel**, a **block**, a **pool** inside a pattern or sequencer block, and an
inline **item**.

```yaml
# station.yaml, channel.yaml: top level
tracks: { ... }

# channel.yaml
rule:
  blocks:
    - tracks: { ... }            # every item in this block
      pools:
        - name: anime
          tracks: { ... }        # items drawn from this pool
      entries:
        - kind: item
          tracks: { ... }        # this one item
```

A more specific level adds to the rules above it:

- `exclude` entries from every level apply.
- `prefer` entries from a more specific level come first, then the inherited ones.
- `inherit: false` on a list drops every rule from the levels above it, for that list only.

A catalog `query` or `collection` entry has no `tracks:` of its own; it takes its
block's. A pool and an inline item are never both the source of one item, so the
two levels do not stack on the same airing.

### Burning a picture subtitle

A picture subtitle is burned in only where a rule says `image: true`. The usual
shape puts it on a block or pool that holds foreign-language films:

```yaml
rule:
  blocks:
    - tracks:
        subtitle:
          prefer:
            - { image: false, lang: "^en" }                              # a text track wins when the file has one
            - { image: true, lang: "^en", when: { audio_lang_not: "^en" } }
      pools: [ { name: anime, ... } ]
```

List the `image: false` entry first. A block's entries go ahead of the inherited
ones, so a block that lists only the `image: true` entry would burn a picture
subtitle even in a file that also carries a text track the viewer could have
switched off.

### Subtitle `default_on`

```yaml
# station.yaml
normalization:
  subtitle: { mode: convert, default_on: false }

# a channel.yaml
subtitle:
  default_on: true       # this channel announces its subtitle track as DEFAULT=YES
```

`DEFAULT=YES` tells a player to start with subtitles showing. The viewer can
still turn them off. It is a setting of the channel, because HLS attaches it to
the whole session's subtitle rendition and it cannot vary per item.

## Sample configs

The committed samples under `examples/` are authored in YAML:

| Sample | File |
|---|---|
| Station manifest | `examples/station.yaml` |
| Test channel (three lavfi items) | `examples/channels/lavfi-test.yaml` |
| The Lord of the Rings (query channel) | `examples/samples/lotr.yaml` |
| Trending Mix (pools + pattern interleave) | `examples/samples/trending-mix.yaml` |
| For You (taste-scored via a plugin) | `examples/samples/foryou.yaml` |
| Worked example scorer plugin | `examples/plugins/taste-cosine.rhai` |
| Test pattern block (3 lavfi items, tracked) | `examples/samples/blocks/lavfi-block.yaml` |
| Star Wars timeline block (8 items, manual order, local-only) | `examples/blocks/starwars-timeline.yaml` |
| Die Hard block (1 item, local-only) | `examples/blocks/diehard.yaml` |

Copy one and adjust the paths and metadata to author a new channel or block.
