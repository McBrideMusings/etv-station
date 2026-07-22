# Architecture

Quick reference. The full rationale lives in [PRD §Architecture](/PRD#architecture); this page exists so you don't have to scroll the PRD when you just want the picture.

## Two programs, one shared filesystem, one shared schema

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
```

- **etv-station** has read/write on the playout volume. Computes "what plays when," writes JSON.
- **etv-next** has read-only on the same volume. Loads the JSON file whose `[start, finish)` covers "now," produces HLS + XMLTV.
- Coupling is exactly two things: the playout JSON schema (pinned via the `etv-next` submodule + Rust path-dep) and the directory layout convention.
- The directory layout is single-sourced from the station config: each channel's output folder is derived as `{output_base}/{identity}` (see [schema](/schema#station-file)), and `tools/render-etv-next.py` generates ETV-next's `lineup.json` + `channelN.json` from that same config — so ETV-next reads exactly where the station writes, with no folder path authored twice.

## Why a submodule

`etv-station` depends on `etv-next-private/crates/ersatztv-playout` as a Rust path dependency through a git submodule pinned to a specific commit:

- Schema drift becomes a compile-time question. If upstream renames a field, `cargo build` fails before any test runs.
- Adopting an upstream schema change is a deliberate two-step: pull `origin/main` into the submodule, bump the submodule SHA in `etv-station`, rebuild.
- No vendoring (which would re-introduce drift), no crates.io dependency on Jason Dove (which he hasn't published).

## Why a separate program (not a fork of etv-next)

ETV-next's README is explicit: "Library and metadata management, scheduling and playout creation are not in scope for this project." Forking to add scheduling would mean eating merge conflicts on every pipeline-side PR forever. The companion-program approach keeps ETV-next's pipeline work and `etv-station`'s rule work on independent release cadences.

## Why two containers

Honest separation of failure domains:

- `etv-station` can crash, leak memory, get stuck on a bad rule — `etv-next` keeps streaming the materialized window.
- Independent restart cadence, resource limits, images, CI.
- Cost is one extra container; benefit is graceful degradation when something goes wrong on the planning side.

## Why filesystem-only IPC

Matches ETV-next's existing process model — it already uses files (`.ready`, `.heartbeat`) for signaling between server and channel subprocesses. No new protocol surface. `ls` shows you the state. Atomic emission via `rename(2)` means the consumer can't observe a half-written file.

## Why Rust

Because ETV-next is Rust and the submodule + path-dep approach is essentially free in Rust. Any other language would either re-implement the schema models (drift risk) or codegen them (added build complexity, weaker typing). Sharing serde models on the producer and consumer side is the fastest path to "schema drift is impossible at compile time."

## Determinism and reload

Generation is deterministic in `(catalog, config, resume_in)` — the same three inputs always produce the same items and the same outgoing resume state. This is what makes config reload safe. Past files are immutable; only the unaired window is touched on reload.

Every channel materializes forward: each generation writes the span after the last one and records the seam in a `.resume` sidecar, so the emitted chunk JSON is the durable timeline rather than a re-derivable rendering. A channel whose list never changes resolves the same list each pass, and those laid end-to-end are the loop — which is why there is no separate looping rule and no `.anchor` sidecar.

Where each series left off is not in that sidecar at all — it is projected from the play-history ledger (`.history`), one line per scheduled airing. Keeping the position in one place is deliberate: a second copy is a second thing to get wrong.

Reload still reaches them. A wholesale wipe-and-re-emit is not available, because the output depends on pool state that is consumed as it goes. So the sidecar also carries **checkpoints** — the pool state entering each not-yet-aired generation. On startup the channel rewinds to the earliest unaired checkpoint, deletes exactly the files from that instant forward, and regenerates them from the current config. What has aired, or is airing, is left alone. Without this a config or overlay edit wouldn't land until the whole written window had played out (#53).

## Time zones

Configurable station-wide via `tz` in the station config (or `ETV_STATION_TZ` at runtime). Affects chunk-boundary alignment only — the persisted UTC timestamps don't move. See [PRD §Time zone](/PRD#time-zone).

## v2+ additions (planned, not yet implemented)

The shape of the v2+ work is locked in [PRD §Scope evolution beyond v1](/PRD#scope-evolution-beyond-v1) and phased in [Roadmap §Next](/roadmap#next-three-sequential-phases-of-v2-scope-expansion). Three architectural additions land in order:

### Unified catalog (Phase C)

A normalized **sqlite catalog** (via `rusqlite`, WAL mode) feeds the query language. Two ingesters at v2:

- **Plex** — primary. Pulls show / movie / collection / playlist metadata from a configured Plex Media Server. Kometa-fed dynamic collections are referenceable but not assumed; most channels express ordering in TOML (`[[entries]]` sequencing) rather than relying on Plex playlists, since Kometa can't autogenerate ordered playlists.
- **Local-FS scan** — narrow purpose: bumpers, commercials, station idents, and errata not in Plex. Walks a configured root with filename + directory metadata + ffprobe.

Sonarr/Radarr ingesters deferred until a concrete Plex gap appears. LAVFI / HTTP / single-path items remain inline-only (declared, not catalogued).

The query language (Phase A picks the off-the-shelf option, candidate: CEL) translates to indexed sqlite reads. Channel TOML carries live queries; the daemon resolves at boot, snapshots the resulting item list for the chunk window, and refreshes the catalog on a per-source interval (24h Plex, 1h local-FS by default). Stateless determinism is preserved — the snapshot is the durable list; the catalog itself is the deterministically-rebuildable substrate. WAL mode means the refresh task can write while query reads stay consistent.

**Wiring status (#96).** The daemon opens the catalog once at startup when the station config sets `catalog_path`, runs a full ingest pass (local-FS over `source_roots`; Plex when `PLEX_URL`/`PLEX_TOKEN` are set — a missing/failing source is logged, never fatal), and shares the open handle (`Arc<Mutex<Catalog>>`) into each channel task, which locks it only for the synchronous resolve. A catalog-free station (no `catalog_path`) still runs — `query` / non-`manual` channels just error at resolve. Beyond identity, this is what lets a manual `local` item **inherit** the catalog's `entry_id` for its file, so it collapses against a `query` returning the same physical file. Per-source refresh intervals, delta sync, and a manual re-ingest trigger are still follow-ups (#91/#96); today it's a startup full ingest.

### Graphics overlay cascade (Phase B)

`etv-station` emits overlay configuration in the playout JSON; **etv-next is the actual renderer** in the existing output pipeline. This requires a deliberate `PlayoutItem` schema extension on the etv-next side — the only planned submodule change in v2+.

Cascade: channel default → block override → item override. Declarative primitives (corner watermark, time-interval fade, lower-third text) compose with [Rhai](https://rhai.rs/) scripts for dynamic behavior. Rendering uses [Vello](https://github.com/linebender/vello). Lottie / `velato` is a deferred side project.

#### Scripted overlays (current implementation)

The `etv-overlay pipe` subprocess (one per channel, supervised by `overlay_supervisor.rs`) renders RGBA frames to a fifo that etv-next reads through its `overlay` filter input. Per frame it evaluates an optional [Rhai](https://rhai.rs/) script whose returned map drives layer visibility, opacity, text content, and corner. Scope exposed to the script:

| Name             | Type    | Source                                          |
|------------------|---------|-------------------------------------------------|
| `time`           | float   | process-elapsed seconds (good for fade curves)  |
| `frame`          | int     | frame index since process start                 |
| `title`          | string  | currently-airing item's `program.title`         |
| `sub_title`      | string  | currently-airing item's `program.sub_title`     |
| `next_title`     | string  | next item's `program.title`                     |
| `next_sub_title` | string  | next item's `program.sub_title`                 |
| `item_elapsed`   | float   | seconds since current item's `start` (`-1.0` if unknown) |
| `item_remaining` | float   | seconds until current item's `finish` (`-1.0` if unknown) |

Schedule access is read-only against the chunked playout JSON the station already writes (`{start}_{finish}.json`). No sidecar files — the supervisor passes `--playout-folder` to the overlay process, which scans on a 1Hz mtime poll and binary-searches per frame.

The script's returned map applies global keys (`visible`, `opacity`) plus an optional `layers` array of per-index overrides:

```rhai
#{
  layers: [
    #{},  // leave layer 0 at TOML defaults
    #{ visible: item_elapsed >= 0.0 && item_elapsed < 10.0,
       content: "Now playing: " + title },
    #{ visible: item_remaining >= 0.0 && item_remaining < 10.0,
       content: "Up next: " + next_title },
  ],
}
```

Per-layer keys: `visible` (bool), `opacity` (float, composed with global), `content` (string — Text layers only, truncated at 512 chars), `corner` (`"top_left"` | `"top_right"` | `"bottom_left"` | `"bottom_right"`). Sample scripts live in `crates/etv-overlay/fixtures/scripts/`.

### Block / channel composition (Phase C)

The current `[rule] type = "loop_forever"` with `[[rule.items]]` is replaced by:

- **Blocks** — reusable, content-agnostic ordered collections. A block = optional `[program]` defaults + `[[entries]]` (item / query / include).
- **Channels** — runtime config + `[[rule.blocks]]` composing blocks with `mode` (`all` or `count = N`), `order` (`chronological` or seeded `random`), and structured `filter`.

A migration script translates legacy configs into the new schema.
