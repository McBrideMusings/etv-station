# File map

Concise repo navigation. See [PRD §Architecture → Repository layout](/PRD#repository-layout) for the rationale.

## Top-level

| Path | What |
|---|---|
| `Cargo.toml` | Workspace manifest. Members: `crates/etv-station`. Excludes `etv-next/`. |
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

| Path | What |
|---|---|
| `examples/station.toml` | Minimal station manifest (`tz = "America/Chicago"`, one channel) used as the default `--config` for the station and as a fixture in `cargo test`. |
| `examples/channels/lavfi-test.toml` | Loop-Forever channel with three lavfi items — each declares both video and audio in one filter graph so etv-next can transcode without falling back to black/silence. |
| `examples/etv-next/lineup.json` | Lineup config for the etv-next dev run. Binds `127.0.0.1:8409`, declares one channel referencing `channel.json`, HLS output at `tmp/hls`. |
| `examples/etv-next/channel.json` | Channel config used by etv-next: points `playout.folder` at `../output/test` so etv-next reads what station writes. videotoolbox hwaccel for macOS dev. |
| `examples/output/` | Gitignored. Station writes playout JSON files here; etv-next reads from here. |

## Dev tooling

| Path | What |
|---|---|
| `tools/dev-run.sh` | Helper for `./tools/dev-run.sh` — builds both etv-next binaries (`ersatztv` and `ersatztv-channel`), starts station + etv-next together, prefixes each line with `[station]`/`[etv]`, traps SIGINT/SIGTERM for clean shutdown. |

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
