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
| `crates/etv-station/` | The daemon binary crate. v1 modules: `config`, `rule`, `roll`, `emit`, `daemon`. |
| `crates/etv-station/src/main.rs` | Entry point. Currently a stub printing the linked schema version. |

## Docs

| Path | What |
|---|---|
| `docs/PRD.md` | Product requirements doc — the canonical spec. |
| `docs/roadmap.md` | Now / Next / Later / Deferred. Direction, not task tracking. |
| `docs/architecture.md` | Distillation of PRD §Architecture for quick reference. |
| `docs/file-map.md` | This page. |
| `docs/index.md` | VitePress landing. |
| `docs/.vitepress/config.mts` | VitePress config. |

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
