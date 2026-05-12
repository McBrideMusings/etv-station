# File map

Concise repo navigation. See [PRD §Architecture → Repository layout](/PRD#repository-layout) for the rationale.

## Top-level

| Path | What |
|---|---|
| `Cargo.toml` | Workspace manifest. Members: `crates/etv-station`, `crates/etv-query-test`, `crates/etv-overlay`. Excludes `etv-next/`. |
| `Cargo.lock` | Workspace lockfile. |
| `task runner` | Generated task runner. **Do not hand-edit** — regenerated from `the task-runner config`. |
| `the task-runner config` | Source of truth for `the project task runner` commands. Uses the `docker-unraid` archetype. |
| `package.json` | VitePress devDep + `docs:dev` / `docs:build` scripts. Docs-only; no runtime JS. |
| `bun.lock` | Bun lockfile for the docs `package.json`. |
| `rustfmt.toml` | Workspace formatting config. |
| `CLAUDE.md` | Repo-level agent guidance: build/run, submodule rules, docs convention. |
| `.env.example` | Template for the deploy-related env vars (`APP_IMAGE`, `UNRAID_HOST`, …). Real `.env` is gitignored. |
| `LICENSE`, `README.md` | Boilerplate. |

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
| `crates/etv-station/src/config/` | TOML config parsing: `station`, `channel`, `rule`, `item`, `load`, `validate`. |
| `crates/etv-station/src/atomic.rs` | `atomic_write_json` — temp file in same dir, fsync, rename. Used by every JSON emission (playout, anchor, durations cache). |
| `crates/etv-station/src/anchor.rs` | `.anchor` sidecar — persists loop position (UTC instant + `item_ids`) across restarts. Re-anchors when item ids change. |
| `crates/etv-station/src/duration.rs` | `DurationCache` (`.durations.json` sidecar) keyed by `(path, mtime)`. Probes Local sources via `ffprobe`; Lavfi/Http durations come from config. |
| `crates/etv-station/src/rule.rs` | `Rule` trait + `LoopForever` impl: `(t - anchor) mod total_loop_duration` walked over cumulative item durations. |
| `crates/etv-station/src/scan.rs` | Discover existing `{start}_{finish}.json` files in a channel's `output_folder` for startup catch-up. |
| `crates/etv-station/src/emit.rs` | Chunk slicer + filename formatter. Walks `tz::add_chunk` boundaries and writes one playout file per chunk via `atomic_write_json`. |
| `crates/etv-station/src/tz.rs` | IANA tz parsing (via `time-tz`) + chunk-boundary helpers that honor DST. |
| `crates/etv-station/src/daemon.rs` | Per-channel orchestrator: probe durations, anchor, startup catch-up, then `tokio::time::interval` roll loop. Validates every channel's overlay TOML at startup (fails fast on parse error). Handles `Ctrl-C` and `SIGTERM` so supervisors get a clean shutdown signal. Also spawns one overlay-supervisor task per channel with `[overlay]` configured and wipes emitted playout JSON on startup (see #53). |
| `crates/etv-station/src/overlay_supervisor.rs` | Per-channel `etv-overlay` subprocess lifecycle: pre-creates the fifo, eager-spawns the renderer at startup, restarts on death, kills on daemon shutdown. Passes `--ready-file {output_folder}/.overlay-ready` to the child and logs `"overlay reported ready"` once the marker appears (first frame written). |
| `crates/etv-station/src/errors.rs` | `ConfigError`, `AtomicWriteError`, and the top-level `StationError` runtime enum. |
| `crates/etv-overlay/` | Velo Phase B overlay renderer crate. Vello + Rhai + asset loading; standalone binary `etv-overlay`. |
| `crates/etv-overlay/src/overlay_spec.rs` | TOML config parsing — `OverlaySpec` (size, framerate, pixel_format, script path, `layers: Vec<OverlayKind>`) + `OverlayKind` enum: `Empty`, `Watermark { corner, margin, box_size, color }`, `Logo { path, corner, margin, height }`, `Text { content, font_family, font_size, color, corner, margin }`. Accepts both `[[layers]]` arrays and legacy `[kind]` single-form. |
| `crates/etv-overlay/src/vello_renderer.rs` | Headless wgpu + Vello renderer. Iterates `OverlaySpec.layers` per frame, drawing watermarks/logos/text in declaration order. Caches decoded PNGs + a Parley `FontContext`/`LayoutContext` on the renderer. Registers the vendored Inter Regular (`assets/fonts/Inter-Regular.ttf`) into `FontContext` and appends it as a last-resort family in the text `FontStack` so slim deploy containers without a system font stack still render glyphs; logs `error!` once per `font_family` when text shapes to zero glyphs. Handles texture-to-buffer copy with 256-byte row alignment. |
| `crates/etv-overlay/src/rhai_engine.rs` | Per-frame Rhai script evaluator. Exposes `time` (seconds) and `frame` (index) constants; script returns `#{visible, opacity}` map applied uniformly to every layer in the spec. |
| `crates/etv-overlay/src/fifo_writer.rs` | Pre-creates the fifo via `mkfifo`, opens O_RDWR (so neither writer nor reader blocks on the other), writes RGBA frames at the configured framerate. |
| `crates/etv-overlay/src/bin/etv-overlay.rs` | CLI: `render-still` (single PNG), `run` (input.mp4 + overlay → output.mp4 harness), `pipe` (long-running fifo writer used by the station supervisor; warms the renderer + writes the first frame before opening the fifo so cold-start latency can't leak partial data, then touches `--ready-file` if passed). |
| `crates/etv-overlay/fixtures/` | Watermark + fade TOMLs + Rhai scripts used by tests and `./tools/overlay-test.sh`. |
| `crates/etv-overlay/assets/fonts/` | Vendored Inter Regular (Latin subset, ~68 KB SIL OFL) bundled into the binary via `include_bytes!` and registered into Parley's `FontContext` as a fallback so `OverlayKind::Text` renders inside slim deploy containers without a system font stack. See the README inside for provenance. |

## Docs

| Path | What |
|---|---|
| `docs/PRD.md` | Product requirements doc — the canonical spec. |
| `docs/roadmap.md` | Now / Next / Later / Deferred. Direction, not task tracking. |
| `docs/architecture.md` | Distillation of PRD §Architecture for quick reference. |
| `docs/file-map.md` | This page. |
| `docs/index.md` | VitePress landing. |
| `docs/.vitepress/config.mts` | VitePress config. |

## Examples

Fixture files needed by `cargo test` are tracked; personal/host-specific configs are gitignored.

| Path | Tracked | What |
|---|---|---|
| `examples/station.toml` | yes | Minimal station manifest used as `cargo test` fixture and default `--config` for dev runs. |
| `examples/channels/lavfi-test.toml` | yes | Loop-Forever channel with three lavfi items — used by the `cargo test` fixture. Has an overlay attached for spike testing. |
| `examples/channels/diehard.toml` | no | Personal Die Hard channel config; gitignored. Wired to the Pierce overlay for Velo Phase B. |
| `examples/channels/trending.toml` | no | Personal Project Hail Mary channel; gitignored. Wired to the trending overlay. |
| `examples/channels/star-trek.toml` | no | 950-episode Star Trek channel (all 12 series, release order). Built from Sonarr; gitignored. |
| `examples/overlays/pierce_logo.toml` | yes | Overlay config: Pierce logo bottom-right of a 1280×720 frame. Used by the diehard channel. |
| `examples/overlays/trending_logo.toml` | yes | Overlay config: trending logo bottom-right of a 1280×720 frame. Used by the trending channel. |
| `examples/assets/pierce-logo.png` | yes | Pierce channel logo (icon only, 1000×1000 RGBA). |
| `examples/assets/trending-logo.png` | yes | Trending channel logo (1200×1200 RGBA, red gradient + white arrow). |
| `examples/assets/pierce-logo-with-text.png` | yes | Older Pierce logo bundled with text underneath — kept for reference; not used by any channel. |
| `examples/etv-next/lineup.json` | no | Generated from `lineup.json.tpl` at dev-run time; gitignored. |
| `examples/etv-next/lineup.json.tpl` | no | Template for the lineup JSON; references channel1/channel2/channel3 configs; gitignored. |
| `examples/etv-next/channel.json` | no | Host-specific etv-next channel config; gitignored. |
| `examples/etv-next/channel2.json` | no | etv-next config for the Pierce channel (1280×720 h264 via videotoolbox); gitignored. |
| `examples/etv-next/channel3.json` | no | etv-next config for the Trending channel; gitignored. |
| `examples/output/` | no | Station writes playout JSON here during dev; gitignored. |

## Dev tooling

| Path | What |
|---|---|
| `tools/dev-run.sh` | Helper for `./tools/dev-run.sh` — builds etv-overlay, both etv-next binaries (`ersatztv` and `ersatztv-channel`), starts station + etv-next together, prefixes each line with `[station]`/`[etv]`, traps SIGINT/SIGTERM for clean shutdown. |
| `tools/kill-dev.sh` | Helper for `./tools/kill-dev.sh` — sends SIGTERM (or `--force` SIGKILL) to all dev processes: etv-station, ersatztv, ersatztv-channel, and any orphaned ffmpeg/ffprobe children. |
| `tools/frame-grab.sh` | Helper for `./tools/frame-grab.sh` — captures one JPEG frame from a live HLS channel via ffmpeg (15 s timeout) and opens it in Preview. `CHANNEL=N` selects the channel (default 1). |
| `tools/validate-streams.sh` | Helper for `./tools/validate-streams.sh` — HTTP probes, codec check, blackdetect, and log scan across all channels in the lineup. |
| `tools/query.sh` | Standalone wrapper for `./tools/query.sh` (sources `.env`, then invokes the `etv-query-test` binary). |
| `tools/overlay-test.sh` | Helper for `./tools/overlay-test.sh` — runs the etv-overlay pipeline against a bumper fixture and opens the resulting mp4. `FIXTURE=`, `CONFIG=`, `OUTPUT=` override defaults. |
| `tools/overlay-still.sh` | Helper for `./tools/overlay-still.sh` — renders a single overlay frame to PNG and opens it. `CONFIG=`, `TIME=` override defaults. |

## Agent skills

| Path | What |
|---|---|
| `.claude/skills/check-channels.md` | Skill: curl HLS endpoints and validate master/variant playlists for each channel. |
| `.claude/skills/check-epg.md` | Skill: fetch `/xmltv.xml`, validate XMLTV structure, cross-check titles against playout JSON on disk. |
| `.claude/skills/frame-grab.md` | Skill: `ffmpeg` frame capture from a live HLS stream; reads image inline so Claude can see the frame. |
| `.claude/skills/read-logs.md` | Skill: locate and read the most recent `tmp/<cmd>.*.log` file from an `the project task runner` run. |

## Submodule

| Path | What |
|---|---|
| `etv-next/` | Submodule → `McBrideMusings/etv-next-private`. **Do not edit from this repo.** Bumped deliberately to absorb upstream schema changes. |
| `etv-next/crates/ersatztv-playout/` | The schema crate `etv-station` depends on via path. Compile-time check for schema drift. |
| `etv-next/schema/playout.json` | The JSON Schema for emitted playout files. |

## Operational

| Path | What |
|---|---|
| `tmp/run.log` | Tee'd output of the most recent `a tools/ script` invocation. Inspect after a failed run. |
| `target/` | Cargo build output. Gitignored. |
| `docs/.vitepress/cache/`, `docs/.vitepress/dist/` | VitePress cache and build output. Gitignored. |
| `node_modules/` | VitePress install. Gitignored. |
