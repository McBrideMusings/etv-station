# File map

Concise repo navigation. See [PRD §Architecture → Repository layout](/PRD#repository-layout) for the rationale.

## Top-level

| Path | What |
|---|---|
| `Cargo.toml` | Workspace manifest. Members: `crates/etv-station`, `crates/etv-query-test`. Excludes `etv-next/`. |
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
| `crates/etv-station/src/daemon.rs` | Per-channel orchestrator: probe durations, anchor, startup catch-up, then `tokio::time::interval` roll loop. Top level handles `Ctrl-C`. |
| `crates/etv-station/src/errors.rs` | `ConfigError`, `AtomicWriteError`, and the top-level `StationError` runtime enum. |

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
| `examples/channels/lavfi-test.toml` | yes | Loop-Forever channel with three lavfi items — used by the `cargo test` fixture. |
| `examples/channels/diehard.toml` | no | Personal Die Hard channel config; gitignored. |
| `examples/channels/star-trek.toml` | no | 950-episode Star Trek channel (all 12 series, release order). Built from Sonarr; gitignored. |
| `examples/etv-next/lineup.json` | no | Generated from env vars at dev-run time; gitignored. |
| `examples/etv-next/channel.json` | no | Host-specific etv-next channel config; gitignored. |
| `examples/output/` | no | Station writes playout JSON here during dev; gitignored. |

## Dev tooling

| Path | What |
|---|---|
| `tools/dev-run.sh` | Helper for `./tools/dev-run.sh` — builds both etv-next binaries (`ersatztv` and `ersatztv-channel`), starts station + etv-next together, prefixes each line with `[station]`/`[etv]`, traps SIGINT/SIGTERM for clean shutdown. |
| `tools/kill-dev.sh` | Helper for `./tools/kill-dev.sh` — sends SIGTERM (or `--force` SIGKILL) to all dev processes: etv-station, ersatztv, ersatztv-channel, and any orphaned ffmpeg/ffprobe children. |
| `tools/frame-grab.sh` | Helper for `./tools/frame-grab.sh` — captures one JPEG frame from a live HLS channel via ffmpeg (15 s timeout) and opens it in Preview. `CHANNEL=N` selects the channel (default 1). |
| `tools/validate-streams.sh` | Helper for `./tools/validate-streams.sh` — HTTP probes, codec check, blackdetect, and log scan across all channels in the lineup. |
| `tools/query.sh` | Standalone wrapper for `./tools/query.sh` (sources `.env`, then invokes the `etv-query-test` binary). |

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
