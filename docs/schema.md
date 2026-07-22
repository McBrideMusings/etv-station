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

Every entry is tagged by a `kind` field. Three kinds. Source: `config/entry.rs`.

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

### `kind: include` — pull in another block file

| Field | Required | Type / default |
|---|---|---|
| `block` | **yes** | path to another block file |
| `mode` | no — default `all` | [`Mode`](#mode) |
| `order` | no — default `manual` | [`Order`](#order) |
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
| `manual` *(default)* | keep authored order |
| `random` | shuffle (seeded by the channel `seed`) |
| `collection` | catalog collection order |
| `score` | by relevance score |
| `field:dir,...` | sort by one or more fields; `dir` is `asc` or `desc` |

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
[`kind: item`](#kind-item-an-authored-file)) — so two entries resolving to the
same physical file collapse regardless of how they entered the block. Source:
`config/block.rs`.

| Value | Meaning |
|---|---|
| `collapse` *(default)* | drop repeats of the same derived identity |
| `keep` | keep every occurrence |

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
```

| Field | Required | Type / default |
|---|---|---|
| `tz` | no — default `UTC` | IANA time zone string; `ETV_STATION_TZ` overrides at runtime |
| `output_base` | **yes** | path — base directory every channel writes under; `ETV_STATION_OUTPUT_BASE` overrides at runtime |
| `channels` | **yes** | list of path strings; each is a literal path or a glob (`*`, `?`, `[`) |
| `source_roots` | no — default empty | list of media mount roots (the daemon's filesystem view) used to canonicalise a local item's path when deriving its identity, so the same file under different mounts is one identity. Empty just skips root-stripping. |
| `catalog_path` | no — default unset | path to the sqlite catalog the daemon opens and ingests (local-FS over `source_roots`, plus Plex when `PLEX_URL`/`PLEX_TOKEN` are set) at startup. Enables `query` entries and non-`manual` order, and lets a manual `local` item path-match onto a catalog identity (so it collapses with a query for the same file). Unset keeps the catalog-free behavior — only inline-item `manual` channels resolve. `ETV_STATION_CATALOG` overrides at runtime. |

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
| `window_days` | no — default `30` | int |
| `chunk_hours` | no — default `24` | int |
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
(`program` / `duplicates` / `entries`) is copied into the include, so a
reference and an equivalent inline block resolve identically. `mode`, `order`,
and `filter` are **composition fields on the include** — they never live in the
block file body.

## Sample configs

The committed samples under `examples/` are authored in YAML:

| Sample | File |
|---|---|
| Station manifest | `examples/station.yaml` |
| Test channel (three lavfi items) | `examples/channels/lavfi-test.yaml` |
| The Lord of the Rings (query channel) | `examples/samples/lotr.yaml` |
| Trending Mix (pools + pattern interleave) | `examples/samples/trending-mix.yaml` |
| Star Wars timeline block (8 items, manual order) | `examples/blocks/starwars-timeline.yaml` |
| Die Hard block (1 item) | `examples/blocks/diehard.yaml` |

Copy one and adjust the paths and metadata to author a new channel or block.
