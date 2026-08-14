# Architecture

Quick reference. The full rationale lives in [PRD §Architecture](/PRD#architecture); this page exists so you don't have to scroll the PRD when you just want the picture.

## Two programs, one shared filesystem, one shared schema

```
┌──────────────────────────── one container ────────────────────────────┐
│                          ┌─────────────────────┐                      │
│                          │  /data/playout/     │                      │
│                          │    <chan>/          │                      │
│                          │      {start}_{finish}.json                 │
│                          │                     │                      │
│  ┌────────────────┐ writes                     │ reads ┌────────────┐ │
│  │  etv-station   │ ────▶│                     │◀───── │  etv-next  │ │
│  │  rules → JSON  │      └─────────────────────┘       │ JSON→HLS   │ │
│  └────────────────┘                                    └────────────┘ │
│         ▲                                                      │      │
└─────────┼──────────────────────────────────────────────────────┼──────┘
     /config (station.yaml, channels, blocks, overlays)     :8409 HLS + XMLTV
```

- **etv-station** has read/write on the playout volume. Computes "what plays when," writes JSON.
- **etv-next** has read-only on the same volume. Loads the JSON file whose `[start, finish)` covers "now," produces HLS + XMLTV.
- Coupling is exactly two things: the playout JSON schema (a Rust path-dep on the vendored ETV-next source) and the directory layout convention.
- The directory layout is single-sourced from the station config: each channel's output folder is derived as `{output_base}/{identity}` (see [schema](/schema#station-file)), and `etv-station --render-etv-next <dir>` generates ETV-next's `lineup.json` + `channelN.json` from that same config — so ETV-next reads exactly where the station writes, with no folder path authored twice. The container entrypoint runs that render at every start, so the two can only ever agree.
- A third contact point beyond playout JSON and the HLS working set (#187): the station writes cached Plex artwork under `artwork_cache_dir` (`ETV_STATION_ARTWORK_CACHE`, `/data/artwork` in the container), and ETV-next serves it back at `/artwork` — mounted from the `artwork.folder` key `etv-station --render-etv-next` writes into `lineup.json` when artwork caching is on. `<icon src>` in the generated `xmltv.xml` is always either this local path (resolved to an absolute URL against the request's own host) or absent — never a Plex URL, since a Plex artwork URL carries `X-Plex-Token` as a working credential and the guide is served over plain HTTP.

## Subtitles

Subtitles are entirely ETV-next's work — the station never opens a media file
to look for them. What the station decides is *which of the two ways* ETV-next
should do it, and it decides that for every channel at once.

The switch is one field in `etv-next/normalization.default.json`, the shared
playback block `--render-etv-next` copies into every generated `channelN.json`:

```json
"subtitle": {
  "mode": "convert"
}
```

- **`convert`** (what this station ships) has ETV-next pull the subtitle text
  out of each file, write it as a WebVTT file beside every video segment, and
  list those in a second playlist the top-level playlist points at. The viewer's
  player shows a subtitle track it can switch on and off.
- **`burn`** has ETV-next paint the words into the video picture instead. Every
  viewer sees them and no player setting can turn them off, because by the time
  the video arrives the words are part of the image.

Two things this switch does *not* control, both worth knowing before reaching
for it:

**It only governs text subtitles.** A Blu-ray or DVD rip usually carries its
subtitles as pictures — `hdmv_pgs_subtitle` or `dvd_subtitle` — and a picture
cannot be turned into WebVTT text. ETV-next composites those onto the video in
either mode. So `convert` is not a promise that nothing gets painted in; it is a
promise about the tracks that *can* be converted.

**Nothing is painted in unless a playout item asks for it.** The station writes
items that name no subtitle track, and an item that names none plays without
subtitles. That is the station's default and the reason the picture-subtitle path
above stays cold: a channel of Blu-ray rips does not silently acquire a permanent,
unselectable subtitle nobody chose. An item opts in by carrying
`tracks.subtitle` — a `stream_index` to pick a track out of its own file, or a
`source` to name a sidecar.

`subtitle.mode` is a station-wide setting today, read from
`normalization.default.json` alone. `presentation.json`, which used to carry a
per-channel deep-merge override for it, was removed outright when the channel
display name moved into the channel YAML (#158, decision 5) — no dual support,
no replacement decided for the config-override half of what it did. A channel
wanting `burn` while the rest of the station runs `convert` has no config path
to say so right now.

Two things are worth knowing about the mode itself:

- `convert` produces a selectable WebVTT track only for text subtitle formats.
  A Blu-ray or DVD rip carries its subtitles as pictures (PGS, VobSub), which
  cannot become WebVTT — so for those items the pipeline paints them onto the
  video instead, per item, the same as `burn` does. The viewer gets subtitles
  either way; what changes is whether they can be switched off
  ([#236](https://github.com/McBrideMusings/etv-station/issues/236)).
- The announced subtitle language is a channel setting, not a fixed string:
  `normalization.subtitle.language.{name,tag}` (defaulting to English / `en`)
  is what goes into `NAME=` and `LANGUAGE=` in the playlist
  ([#238](https://github.com/McBrideMusings/etv-station/issues/238)). HLS
  allows one language per subtitle rendition for a whole session, so a channel
  whose schedule mixes languages still declares the single value it advertises
  — this cannot vary per programme. The tag describes the subtitle text, not
  the audio, so Japanese audio with English subtitles is labelled correctly.
- The stream picker honours that same declaration. When a file carries subtitle
  streams in more than one language, `select_subtitle_stream()` prefers the
  probed stream whose ISO 639-2 tag matches the channel's declared tag, and
  falls back to the first non-image stream when nothing matches or the file
  carries no language tags at all
  ([#237](https://github.com/McBrideMusings/etv-station/issues/237)).

## Why ETV-next is vendored, not pinned

`etv-station` depends on `vendor/etv-next/crates/ersatztv-playout` as a Rust path dependency. The whole ETV-next source sits in this repo as ordinary tracked files, upstream plus this project's modifications to it:

- Schema drift becomes a compile-time question. If upstream renames a field, `cargo build` fails before any test runs.
- Adopting an upstream change is one merge in one repo: `git subtree pull --prefix=vendor/etv-next etv-upstream main --squash`, resolve whatever conflicts, rebuild.
- No crates.io dependency on Jason Dove (which he hasn't published).

This replaced a git submodule pointing at a fork (`McBrideMusings/etv-next-station`). The submodule made sense while the delta was small, but it grew to 3,056 added and 457 removed lines across 35 files — 30 of them files upstream actively edits. A separate repo for that meant a change to one program landing in two repos with a SHA bump between them, and the diff being invisible to anyone reading `etv-station`'s history. Vendoring puts the code and the changes in the same place; the merge conflicts are identical either way, because they come from the delta, not from where it is stored.

The cost: nothing enforces the separation any more. A single commit can now mix a station change and an ETV-next change, which makes the next upstream merge harder to read. Keep them in separate commits.

## Why plexdb-reader is vendored, not a git dependency

`etv-station` links [plex-db-ex](https://github.com/McBrideMusings/plex-db-ex)'s read-only `plexdb-reader` crate to expose enrichment tags, affinity edges, and taste vectors to a Rhai plugin whose channel config granted the datastore capability (#181, `crates/etv-station/src/score.rs`). The crate's `crates/plexdb-reader/{Cargo.toml,src}` sit in this repo, unmodified, under `vendor/plexdb-reader/`, as an ordinary workspace member — not a git dependency:

- `plex-db-ex` is a **private** repository, and neither this repo's CI (`actions/checkout@v4` checks out only `etv-station` itself) nor the Docker build (`cargo build` runs inside the image with no SSH agent forwarded in) carry any credential that could fetch it. A git dependency would build on this machine, where `pierce`'s own SSH key is already trusted, and fail everywhere else.
- The crate is small (four files, ~760 lines) and self-contained (`rusqlite` + `thiserror`, both already workspace dependencies here), so vendoring the whole `plex-db-ex` repo the way `vendor/etv-next` vendors ErsatzTV/next — with `git subtree pull` — would be pulling in a Python package and its migrations for one Rust crate. It is copied in by hand instead: `cp` the four files from a checkout of `plex-db-ex`'s `crates/plexdb-reader/{Cargo.toml,src}`, verbatim except the manifest's dependency lines, which point at etv-station's own `[workspace.dependencies]` instead of plex-db-ex's.
- `plexdb_reader::schema::SUPPORTED_SCHEMA_VERSION` still makes schema drift a compile-time-and-load-time question exactly as the etv-next vendoring does: a column the crate reads getting dropped upstream fails this build, and a store at the wrong version fails a granted channel's load naming both versions, never a panic and never a silently empty pool.

The crate's own tests are not vendored — its integration suite shells out to `uv run plexdb init` against a full `plex-db-ex` checkout to build its fixture store, which this repo does not have. `crates/etv-station/tests/datastore_capability.rs` and `config::validate::tests` cover the same ground with their own hand-built fixture stores instead.

## Why a separate program (not a fork of etv-next)

ETV-next's README is explicit: "Library and metadata management, scheduling and playout creation are not in scope for this project." Forking to add scheduling would mean eating merge conflicts on every pipeline-side PR forever. The companion-program approach keeps ETV-next's pipeline work and `etv-station`'s rule work on independent release cadences.

## Why one container

The playout folder is the entire interface between the two programs, so shipping them separately would mean sharing that folder between containers, keeping their config in step, and starting them in the right order — all to separate two processes that are useless apart. In one image the folder is just a directory both processes see, and deploying the stack is one image plus one config mount.

The failure separation that two containers used to provide is kept by the entrypoint (`docker/entrypoint.sh`), which treats the two processes differently:

- `etv-station` can crash, leak memory, get stuck on a bad rule — it is restarted in place and `etv-next` keeps streaming the window already written, which is the whole point of materializing forward. Repeated crashes (default 5) end the container rather than loop forever.
- `etv-next` exiting *is* the service being down, so it ends the container and the restart policy takes over.

What is genuinely given up: independent resource limits and independent restart cadence for the two halves.

## The media has to be mounted, not streamed

The catalog is read from Plex over HTTP, but the *media* never is. Every `entry_sources` row carries a `playback_path`, and the station hands it to the player as a local file — 86,232 of 86,232 rows in the production catalog are Plex-sourced paths under `/media`, resolved because the same share is mounted into the container at the same prefix.

So a Plex library this station carries must be reachable as a filesystem path inside the container. Where the two disagree — Plex reporting `/mnt/user/media/…` while the container sees `/media/…` — the ingester's `path_from` / `path_to` prefix remap covers it, matching only at a path boundary so `/media` never rewrites `/mediabackup`.

There is deliberately no streaming fallback. A Plex entry whose path does not resolve is not silently skipped: the duration probe fails, the item gets an error card reading "file not found", and the daemon logs `item.error_card` with a count in its probe stats. Adding an HTTP/stream `SourceConfig` for Plex content would serve a deployment where the media is not mounted, which is not the shape this runs in (#98). `SourceConfig::Http` remains for hand-authored `http` items.

## Why filesystem-only IPC

Matches ETV-next's existing process model — it already uses files (`.ready`, `.heartbeat`) for signaling between server and channel subprocesses. No new protocol surface. `ls` shows you the state. Atomic emission via `rename(2)` means the consumer can't observe a half-written file.

## Why Rust

Because ETV-next is Rust and the vendored path-dep approach is essentially free in Rust. Any other language would either re-implement the schema models (drift risk) or codegen them (added build complexity, weaker typing). Sharing serde models on the producer and consumer side is the fastest path to "schema drift is impossible at compile time."

## Determinism and reload

Generation is deterministic in `(catalog, config, resume_in)` — the same three inputs always produce the same items and the same outgoing resume state. This is what makes config reload safe. Past files are immutable; only the unaired window is touched on reload.

Every channel materializes forward: each generation writes the span after the last one and records the seam in a `.resume` sidecar, so the emitted chunk JSON is the durable timeline rather than a re-derivable rendering. A channel whose list never changes resolves the same list each pass, and those laid end-to-end are the loop — which is why there is no separate looping rule and no `.anchor` sidecar.

Where each series left off is not in that sidecar at all — it is projected from the play-history ledger, one row per scheduled airing in `history.db`: a sqlite database of its own, separate from the catalog because play history is not rebuildable (#111). Keeping the position in one place is deliberate: a second copy is a second thing to get wrong. The same table also answers where a series last aired *across every channel*, keyed by `show_id` — a query a per-channel file could never serve.

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

**Wiring status (#96).** The daemon opens the catalog once at startup when the station config sets `catalog_path`, runs a full ingest pass (local-FS over `source_roots`; Plex when `PLEX_URL`/`PLEX_TOKEN` are set — a missing/failing source is logged, never fatal), and then drops that writable handle: every write this process makes to the catalog happens inside that one function. Each channel task opens its own **read-only** handle (`Catalog::open_readonly`) and keeps it for the life of the task. The file is in WAL mode and nothing writes it after ingest, so the readers never contend — there is no station-wide lock, and a channel whose scorer plugin spends minutes ranking cannot stall anybody else's resolve. A catalog-free station (no `catalog_path`) still runs — `query` / non-`manual` channels just error at resolve. Beyond identity, this is what lets a manual `local` item **inherit** the catalog's `entry_id` for its file, so it collapses against a `query` returning the same physical file. Per-source refresh intervals, delta sync, and a manual re-ingest trigger are still follow-ups (#91/#96); today it's a startup full ingest.

### Graphics overlay cascade (Phase B)

`etv-station` emits overlay configuration in the playout JSON; **etv-next is the actual renderer** in the existing output pipeline.

The schema extension this was once planned to need is **withdrawn**. `PlayoutItem.overlay` stays singular — one `OverlaySpec`, one fifo, one `etv-overlay` process per channel — because the cascade resolves *which config that process runs*, not a set of overlays to composite. A set would cost a fifo, a writer process, and an ffmpeg overlay filter per concurrent overlay, across every channel. Layering is the script's job instead: it is handed the playing item's metadata and decides what to draw, which is also the only thing that works in a query-driven block where nothing knows in advance which show fills a slot (#48).

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
