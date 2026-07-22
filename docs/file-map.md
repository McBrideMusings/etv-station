# File map

Concise repo navigation. See [PRD §Architecture → Repository layout](/PRD#repository-layout) for the rationale.

## Top-level

| Path | What |
|---|---|
| `Cargo.toml` | Workspace manifest. Members: `crates/etv-station`, `crates/etv-query-test`, `crates/etv-overlay`. Excludes `etv-next/`. |
| `Cargo.lock` | Workspace lockfile. |
| `package.json` | VitePress devDep + `docs:dev` / `docs:build` scripts. Docs-only; no runtime JS. |
| `bun.lock` | Bun lockfile for the docs `package.json`. |
| `rustfmt.toml` | Workspace formatting config. |
| `CLAUDE.md` | Repo-level agent guidance: build/run, submodule rules, docs convention. |
| `Dockerfile` | Multi-stage build: a Rust builder compiles release `etv-station` + `etv-overlay`; a `debian-slim` runtime carries the two binaries plus ffmpeg/ffprobe and a software-Vulkan stack for headless overlay rendering. |
| `.dockerignore` | Keeps `target/`, `.git/`, docs build output, and scratch out of the Docker build context. |
| `.env.example` | Template for the deploy-related env vars (`APP_IMAGE`, `UNRAID_HOST`, …). Real `.env` is gitignored. |
| `LICENSE` | Boilerplate. |
| `README.md` | Project intro: tagline, status (what exists vs. planned), clone-with-submodules, build/run commands, docs links. |

## Source

| Path | What |
|---|---|
| `crates/etv-station/` | The daemon binary crate. |
| `crates/etv-query-test/` | Phase A CEL feasibility harness. Queries Plex + FS catalogs with CEL; see `./tools/query.sh`. |
| `crates/etv-query-test/src/cel_eval.rs` | CEL compile + per-item eval. Case-insensitive; custom helpers: `season_in`, `icontains`, `in_collection`, `has_category`, `shorter_than`, `longer_than`. |
| `crates/etv-query-test/src/plex.rs` | Read-only Plex HTTP client: section/show/collection ingestion, show-collection enrichment, path translation, `type_from_section`. |
| `crates/etv-query-test/src/fs_catalog.rs` | FS scanner: glob + ffprobe, `type_from_path` (dir-name → semantic type), `ETV_FS_ROOTS` config, path-keyed result map. |
| `crates/etv-query-test/src/normalize.rs` | `NormalizedItem`: unified record shape for both Plex and FS items. `sources: Vec<String>`, `media_type`, all CEL-bound fields. |
| `crates/etv-query-test/src/cache.rs` | Disk cache for full-section Plex ingest (`target/cache/plex-all.json`, 1 h TTL). |
| `crates/etv-query-test/cases/` | Six committed CEL fixture cases for Phase A (TOS marathon, multi-Trek, TNG s3-5, bumper-block, Dragon Ball, Trek in-universe). |
| `crates/etv-query-test/fixtures/bumpers/` | Committed synthetic MP4s (~100 KB) for the bumper-block fixture case. |
| `crates/etv-station/src/main.rs` | Binary entry point. Parses CLI flags, inits tracing, loads config, drives `daemon::run`. |
| `crates/etv-station/src/lib.rs` | Library entry point. Re-exports the modules so integration tests and the binary share one surface. |
| `crates/etv-station/src/config/` | TOML config parsing (Phase C block/channel/entries schema): `station`, `channel`, `rule` (`[[rule.blocks]]` includes), `block` (`[program]`/`duplicates`/`[[entries]]`), `entry` (kind-tagged item/query/include), `source`, `mode`, `order`, `filter`, `load`, `validate`. |
| `crates/etv-station/src/resolve.rs` | Resolve pipeline (#71): flattens `[[rule.blocks]]` into an ordered `ResolvedItem` list — resolve entries (inline items direct; `query` entries via `Catalog::resolve_query`, each `entry_id` → item) → `duplicates` (collapse before order) → `order` (`manual` keeps authored, else `Catalog::resolve_order`) → `mode`. Takes `Option<&Catalog>`; with one it builds a canonical-path index so a manual `local` item inherits the catalog `entry_id` for its file (manual∩query collapse). Still rejects `include` entries, block `filter`, and `fallback` (later issues). |
| `crates/etv-station/src/catalog/` | Unified sqlite-backed catalog (#47) — the durable store query-based channels resolve against, plus the ingesters that populate it (`ingest/`: local-FS + Plex). |
| `crates/etv-station/src/catalog/ingest/mod.rs` | Ingest module root; owns the shared `canonical_index` (canonical-path → `entry_id` over all provenance rows, non-`fs:` id wins) that both ingesters use for path-match inherit. |
| `crates/etv-station/src/catalog/mod.rs` | `Catalog` handle over a `rusqlite` connection (WAL, `foreign_keys=ON`). Upsert/read API for entries, provenance sources (incl. `all_sources` for ingest path-match), external ids, tags, and collections; two provenance rows on one `entry_id` = a deduped item. |
| `crates/etv-station/src/catalog/ingest/fs.rs` | Local-FS catalog ingester (#92). `ingest_roots` globs media roots + `ffprobe`s durations; `ingest_files` is the pure catalog-writing core with ingest-time path-match inherit (canonical path reuses an existing `entry_id` — Plex or prior scan — else `fs:<fnv1a>`), `local_fs` provenance keyed by canonical path (idempotent re-scan), `fs_dir` tag from the parent directory. |
| `crates/etv-station/src/catalog/ingest/plex.rs` | Plex catalog ingester (#91, core). `ingest_items` is the pure core: `entry_id` from the strongest Plex GUID (`imdb→tmdb→tvdb→plex`, `fs:` fallback) with path-match inherit onto a prior FS entry; Plex is authoritative so it always upserts metadata (incl. promoted `edition`/`studio`/`absolute_episode` columns) + records every external id + a `plex` provenance row (ratingKey) + genre/label/cast/director/writer/producer/country tags. `ingest_collections` populates `collections`/`collection_items` (members resolved ratingKey→`entry_id`, in Plex order). `ingest_from_env`/`PlexClient` (ureq) is the thin HTTP layer. Collections/playlists, delta sync, and the refresh TTL are deferred to the daemon wiring (#96). |
| `crates/etv-station/src/catalog/schema.rs` | DDL + up-only migrations (version table, appended-never-edited) for the six locked tables: `entries`, `entry_sources`, `entry_external_ids`, `tags`, `collections`, `collection_items`. |
| `crates/etv-station/src/catalog/model.rs` | Typed model: `Entry` plus `Source` / `ExternalNs` / `TagNs` enums for the fixed-set discriminators (open-ended `type` stays a string). |
| `crates/etv-station/src/catalog/identity.rs` | Deterministic `entry_id` derivation: GUID priority `imdb → tmdb → tvdb → plex`, `fs:<fnv1a>` path fallback, and the pure string half of canonical-path (root-strip + separator-normalize). |
| `crates/etv-station/src/catalog/order.rs` | Order resolution engine (#69). `Catalog::resolve_order` applies a `config::Order` to a resolved `entry_id` set: `field:dir` compound sorts via SQL `ORDER BY` (nulls last per term, implicit `entry_id` tiebreaker), `manual` keeps authored order, `random` is a SplitMix64-seeded Fisher–Yates shuffle (deterministic per seed), `collection` reads `collection_items.position`. Non-sortable fields and `score` are errors. |
| `crates/etv-station/src/catalog/query.rs` | CEL→SQL `WHERE` translation (#68). Walks the `cel` AST for a channel `query` expression over `item.*` and compiles it to a parameterised `WHERE` (scalar comparisons/`in`/`contains`/`startsWith`/`matches`, tag/collection/source membership via `EXISTS`, boolean `&&`/`\|\|`/`!`); `Catalog::resolve_query` runs it. Registers sqlite `REGEXP` (regex crate). Unknown fields and tag comparisons are config errors; empty result is `[]`, not an error. |
| `crates/etv-station/src/catalog/error.rs` | `CatalogError` — open / sqlite / bad-row variants. |
| `crates/etv-station/src/atomic.rs` | `atomic_write_json` — temp file in same dir, fsync, rename. Used by every JSON emission (playout, anchor, durations cache). |
| `crates/etv-station/src/anchor.rs` | `.anchor` sidecar — persists loop position (UTC instant + `item_ids`) across restarts. Re-anchors when item ids change. |
| `crates/etv-station/src/duration.rs` | `DurationCache` (`.durations.json` sidecar) keyed by `(path, mtime)`. Probes Local sources via `ffprobe`; Lavfi/Http durations come from config. Prunes entries whose path is no longer referenced by the channel's current items so the sidecar doesn't grow unboundedly. |
| `crates/etv-station/src/rule.rs` | `Rule` trait + `LoopForever` sequencer: loops the resolver's `ResolvedItem` list — `(t - anchor) mod total_loop_duration` walked over cumulative item durations — to fill the chunk window. |
| `crates/etv-station/src/scan.rs` | Discover existing `{start}_{finish}.json` files in a channel's `output_folder` for startup catch-up. |
| `crates/etv-station/src/emit.rs` | Chunk slicer + filename formatter. Walks `tz::add_chunk` boundaries and writes one playout file per chunk via `atomic_write_json`. |
| `crates/etv-station/src/tz.rs` | IANA tz parsing (via `time-tz`) + chunk-boundary helpers that honor DST. |
| `crates/etv-station/src/daemon.rs` | Per-channel orchestrator: probe durations, anchor, startup catch-up, then `tokio::time::interval` roll loop. Validates every channel's overlay TOML at startup (fails fast on parse error). Handles `Ctrl-C` and `SIGTERM` so supervisors get a clean shutdown signal. Also spawns one overlay-supervisor task per channel with `[overlay]` configured and wipes emitted playout JSON on startup (see #53). Opens + ingests the station catalog once at startup when `catalog_path` is set (`open_and_ingest_catalog`) and shares it (`Arc<Mutex<Catalog>>`) into each channel's resolve (#96). |
| `crates/etv-station/src/overlay_supervisor.rs` | Per-channel `etv-overlay` subprocess lifecycle: pre-creates the fifo, eager-spawns the renderer at startup, restarts on death, kills on daemon shutdown. Passes `--ready-file {output_folder}/.overlay-ready` and `--playout-folder {output_folder}` to the child so it can read the station's chunked playout JSON for per-frame program metadata; logs `"overlay reported ready"` once the marker appears (first frame written). |
| `crates/etv-station/src/errors.rs` | `ConfigError`, `AtomicWriteError`, and the top-level `StationError` runtime enum. |
| `crates/etv-station/tests/lotr_sample.rs` | Integration acceptance test for Sample S2 (#76): resolves `examples/channels/lotr.yaml` against a fixture LOTR catalog and asserts oldest-first release order — the end-to-end query + order + resolve path. |
| `crates/etv-station/tests/dragonball_sample.rs` | Integration acceptance test for Sample S4 (#78): resolves `examples/channels/dragonball.yaml` against a fixture catalog and asserts a `manual` block weaves two `absolute_episode`-ordered query ranges around an inline movie in authored order — per-entry query order (#46) + `absolute_episode` (#47) + query/inline intermingling. |
| `crates/etv-station/tests/trending_shuffle_sample.rs` | Integration acceptance test for Sample S5 (#79): resolves `examples/channels/trending-shuffle.yaml` against a fixture catalog and asserts the "Trending" collection resolves as a set (`item.collections.contains`, non-members excluded) and shuffles — a pinned `seed` reproduces the order. Proves collections-as-set + the resolve→collapse→`random` pipeline. |
| `crates/etv-station/tests/studio_brand_sample.rs` | Integration acceptance test for Sample S9 (#83): resolves `examples/channels/ghibli.yaml` (clean `item.studio`) and asserts the brand tiers resolve by `item.labels` instead — a Disney Label spans Pixar/Marvel/Lucasfilm sub-studios where `item.studio == "Disney"` matches nothing. Proves `studio` (`==`/`!=`) vs `labels` (`contains`) over three metadata-reliability tiers. |
| `crates/etv-overlay/` | Velo Phase B overlay renderer crate. Vello + Rhai + asset loading; standalone binary `etv-overlay`. |
| `crates/etv-overlay/src/overlay_spec.rs` | TOML config parsing — `OverlaySpec` (size, framerate, pixel_format, script path, `layers: Vec<OverlayKind>`) + `OverlayKind` enum: `Empty`, `Watermark { corner, margin, box_size, color }`, `Logo { path, corner, margin, height }`, `Text { content, font_family, font_size, color, corner, margin }`. Accepts both `[[layers]]` arrays and legacy `[kind]` single-form. |
| `crates/etv-overlay/src/vello_renderer.rs` | Headless wgpu + Vello renderer. Iterates `OverlaySpec.layers` per frame, drawing watermarks/logos/text in declaration order. Caches decoded PNGs + a Parley `FontContext`/`LayoutContext` on the renderer. Registers the vendored Inter Regular (`assets/fonts/Inter-Regular.ttf`) into `FontContext` and appends it as a last-resort family in the text `FontStack` so slim deploy containers without a system font stack still render glyphs; logs `error!` once per `font_family` when text shapes to zero glyphs. Handles texture-to-buffer copy with 256-byte row alignment. |
| `crates/etv-overlay/src/rhai_engine.rs` | Per-frame Rhai script evaluator. Scope exposes `time`/`frame` plus program-context constants `title`, `sub_title`, `next_title`, `next_sub_title`, `item_elapsed`, `item_remaining`. Script returns a map with global `visible`/`opacity` plus an optional `layers` array of per-index overrides (`visible`, `opacity`, `content` for Text layers, `corner`). |
| `crates/etv-overlay/src/program_context.rs` | Per-channel schedule reader. Scans the station's chunked playout JSON folder (1Hz mtime poll), merges item lists in start order, and answers `current_at(now: OffsetDateTime)` with current/next title + `item_elapsed`/`item_remaining`. Read-only — no sidecar files. |
| `crates/etv-overlay/src/fifo_writer.rs` | Pre-creates the fifo via `mkfifo`, opens O_RDWR (so neither writer nor reader blocks on the other), writes RGBA frames at the configured framerate. |
| `crates/etv-overlay/src/bin/etv-overlay.rs` | CLI: `render-still` (single PNG), `run` (input.mp4 + overlay → output.mp4 harness), `pipe` (long-running fifo writer used by the station supervisor; warms the renderer + writes the first frame before opening the fifo so cold-start latency can't leak partial data, then touches `--ready-file` if passed). When `--playout-folder` is set, hands the per-frame program context to the Rhai engine. |
| `crates/etv-overlay/fixtures/` | Watermark + fade + dynamic-text TOMLs and Rhai scripts (`now_playing.rhai`, `up_next.rhai`, `pulse_watermark.rhai`, `corner_rotate.rhai`, `now_and_next.rhai`) used by tests and `./tools/overlay-test.sh`. |
| `crates/etv-overlay/assets/fonts/` | Vendored Inter Regular (Latin subset, ~68 KB SIL OFL) bundled into the binary via `include_bytes!` and registered into Parley's `FontContext` as a fallback so `OverlayKind::Text` renders inside slim deploy containers without a system font stack. See the README inside for provenance. |

## Docs

| Path | What |
|---|---|
| `docs/PRD.md` | Product requirements doc — the canonical spec. |
| `docs/roadmap.md` | Now / Next / Later / Deferred. Direction, not task tracking. |
| `docs/architecture.md` | Distillation of PRD §Architecture for quick reference. |
| `docs/schema.md` | Config schema reference — station / channel / block files, entry & source kinds, `ProgramMetadata`, order/mode/filter, with YAML examples. |
| `docs/adr/` | Architecture Decision Records — why a non-obvious call was made (e.g. `0001-reload-generation-revert.md`). |
| `docs/file-map.md` | This page. |
| `docs/index.md` | VitePress landing. |
| `docs/.vitepress/config.mts` | VitePress config. |

## Examples

Fixture files needed by `cargo test` are tracked; personal/host-specific configs are gitignored.

| Path | Tracked | What |
|---|---|---|
| `examples/station.yaml` | yes | Minimal station manifest used as `cargo test` fixture and default `--config` for dev runs. Authored in YAML; the loader accepts TOML or YAML by extension. |
| `examples/channels/lavfi-test.yaml` | yes | Single inline block with three lavfi item entries — used by the `cargo test` fixture. Has an overlay attached for spike testing. |
| `examples/channels/diehard.yaml` | no | Personal Die Hard channel; gitignored. Demonstrates the path form — composes `../blocks/diehard.yaml`. Wired to the Pierce overlay. |
| `examples/blocks/diehard.yaml` | no | Reusable block file (the Die Hard item) referenced by `diehard.yaml`; demonstrates the `block = "path"` include form. |
| `examples/channels/trending.yaml` | no | Personal Project Hail Mary channel; gitignored. Wired to the trending overlay. |
| `examples/channels/star-trek.yaml` | no | 950-episode Star Trek channel (all 12 series, release order). Built from Sonarr; gitignored. |
| `examples/channels/lotr.yaml` | yes | Sample S2 (#76): a **query** channel — resolves the LOTR films from the catalog and plays them oldest-first (`order = "release_date:asc"`). Not in `station.yaml` (needs a populated catalog + daemon wiring #96 to run live); proven by the `lotr_sample` test. |
| `examples/channels/dragonball.yaml` | yes | Sample S4 (#78): a **manual** block weaving two `absolute_episode`-ordered `query` episode-ranges around an inline `item` movie — the hardest authored-order case (per-entry query order inside a manual block). Not in `station.yaml`; proven by the `dragonball_sample` test. |
| `examples/channels/trending-shuffle.yaml` | yes | Sample S5 (#79): a **query** channel treating the "Trending" collection as a set — `order = "random"` (unseeded = fresh shuffle each generation), `mode = "all"`, `duplicates = "collapse"`. Not in `station.yaml`; proven by the `trending_shuffle_sample` test. |
| `examples/channels/ghibli.yaml` | yes | Sample S9 (#83): a **query** channel of one studio via the clean `item.studio` column (`order = "title:asc"`) — the studio tier of the studio-vs-labels sample (brand tiers use `item.labels`). Not in `station.yaml`; proven by the `studio_brand_sample` test. |
| `examples/overlays/pierce_logo.toml` | yes | Overlay config: Pierce logo bottom-right of a 1280×720 frame. Used by the diehard channel. |
| `examples/overlays/trending_logo.toml` | yes | Overlay config: trending logo bottom-right of a 1280×720 frame. Used by the trending channel. |
| `examples/assets/pierce-logo.png` | yes | Pierce channel logo (icon only, 1000×1000 RGBA). |
| `examples/assets/trending-logo.png` | yes | Trending channel logo (1200×1200 RGBA, red gradient + white arrow). |
| `examples/assets/pierce-logo-with-text.png` | yes | Older Pierce logo bundled with text underneath — kept for reference; not used by any channel. |
| `examples/etv-next/normalization.default.json` | yes | Shared ETV-next playback block (ffmpeg + normalization) applied to every generated channel config. |
| `examples/etv-next/presentation.example.json` | yes | Sample per-channel ETV-next overrides (display name, playback `config`), keyed by channel identity. Copy to `presentation.json` to customize. |
| `examples/etv-next/presentation.json` | no | Personal ETV-next display names / overrides consumed by `render-etv-next.py`; gitignored. |
| `examples/etv-next/lineup.json` | no | Generated by `tools/render-etv-next.py` from the station config at dev-run time; gitignored. |
| `examples/etv-next/channel*.json` | no | Per-channel ETV-next configs generated by `render-etv-next.py` (playout folder derived from the station); gitignored. |
| `examples/output/` | no | Station writes playout JSON here during dev; gitignored. |

## Dev tooling

| Path | What |
|---|---|
| `tools/dev-run.sh` | Builds etv-overlay, both etv-next binaries (`ersatztv` and `ersatztv-channel`), starts station + etv-next together, prefixes each line with `[station]`/`[etv]`, traps SIGINT/SIGTERM for clean shutdown. The canonical local integration test. |
| `tools/render-etv-next.py` | Generates ETV-next's `lineup.json` + `channelN.json` from the station config — roster + numbers + each `playout.folder` derived via `--list-folders`, display names + playback block from `presentation.json` / `normalization.default.json`. The single source for the shared-folder contract; called by `dev-run.sh`. |
| `tools/kill-dev.sh` | Sends SIGTERM (or `--force` SIGKILL) to all dev processes: etv-station, ersatztv, ersatztv-channel, and any orphaned ffmpeg/ffprobe children. |
| `tools/frame-grab.sh` | Captures one JPEG frame from a live HLS channel via ffmpeg (15 s timeout) and opens it in Preview. `CHANNEL=N` selects the channel (default 1). |
| `tools/validate-streams.sh` | HTTP probes, codec check, blackdetect, and log scan across all channels in the lineup. Run while a dev integration is active. |
| `tools/query.sh` | Standalone ad-hoc CEL query wrapper (sources `.env`, then invokes the `etv-query-test` binary). |
| `tools/overlay-test.sh` | Runs the etv-overlay pipeline against a bumper fixture and opens the resulting mp4. `FIXTURE=`, `CONFIG=`, `OUTPUT=` override defaults. |
| `tools/overlay-still.sh` | Renders a single overlay frame to PNG and opens it. `CONFIG=`, `TIME=` override defaults. |

## Agent skills

| Path | What |
|---|---|
| `.claude/skills/check-channels.md` | Skill: curl HLS endpoints and validate master/variant playlists for each channel. |
| `.claude/skills/check-epg.md` | Skill: fetch `/xmltv.xml`, validate XMLTV structure, cross-check titles against playout JSON on disk. |
| `.claude/skills/frame-grab.md` | Skill: `ffmpeg` frame capture from a live HLS stream; reads image inline so Claude can see the frame. |
| `.claude/skills/read-logs.md` | Skill: locate and read the most recent `tmp/<cmd>.*.log` file from a dev run. |

## Submodule

| Path | What |
|---|---|
| `etv-next/` | Submodule → `McBrideMusings/etv-next-private`. **Do not edit from this repo.** Bumped deliberately to absorb upstream schema changes. |
| `etv-next/crates/ersatztv-playout/` | The schema crate `etv-station` depends on via path. Compile-time check for schema drift. |
| `etv-next/schema/playout.json` | The JSON Schema for emitted playout files. |

## Operational

| Path | What |
|---|---|
| `tmp/run.log` | Tee'd output of the most recent dev/tooling invocation. Inspect after a failed run. |
| `target/` | Cargo build output. Gitignored. |
| `docs/.vitepress/cache/`, `docs/.vitepress/dist/` | VitePress cache and build output. Gitignored. |
| `node_modules/` | VitePress install. Gitignored. |
