# etv-station

`etv-station` is a standalone playout-JSON generator daemon for [ErsatzTV-next](https://github.com/ErsatzTV/next). It reads station/channel TOML configs, applies a sequencing rule (v1: Loop Forever), and writes playout JSON files to a shared volume that ETV-next consumes.

## Architecture in one sentence

One container running two programs over one folder, with one shared schema: `etv-station` writes `{start}_{finish}.json` files; ETV-next reads them, produces HLS + XMLTV. The schema is a path dependency on the vendored ETV-next source under `vendor/etv-next/`, so drift is a compile-time error.

See `docs/architecture.md` for the full picture and `docs/PRD.md` for the spec.

## `vendor/etv-next/` — vendored upstream, ours to modify

`vendor/etv-next/` holds [ErsatzTV/next](https://github.com/ErsatzTV/next) plus this project's modifications to it, as ordinary tracked files. It is not a submodule and there is no fork repository — edit it here like any other code.

Upstream is absorbed with a real merge:

```sh
git subtree pull --prefix=vendor/etv-next etv-upstream main --squash
```

`etv-upstream` is `https://github.com/ErsatzTV/next`; add it with `git remote add etv-upstream https://github.com/ErsatzTV/next` if a fresh clone lacks it. Conflicts land in whichever files upstream and this project both touched. See `.claude/skills/upstream-sync/SKILL.md` for the survey, the two files that need special handling, and the conflict doctrine.

Keep a change that touches `vendor/etv-next/` in its own commit, separate from station-side changes — the next upstream merge is far easier to read when the two aren't mixed.

## `vendor/plexdb-reader/` — vendored copy, not ours to modify

`vendor/plexdb-reader/` holds a hand-copied, unmodified snapshot of [plex-db-ex](https://github.com/McBrideMusings/plex-db-ex)'s `crates/plexdb-reader` — the read-only reader crate the core links to expose enrichment tags, affinity edges, and taste vectors to a granted Rhai plugin (#181). Unlike `vendor/etv-next/`, this is **not** a subtree merge and not editable here: refresh it by copying `crates/plexdb-reader/{Cargo.toml,src}` from a `plex-db-ex` checkout over the existing files, re-adapting only the manifest's dependency lines to this workspace's own `[workspace.dependencies]`. See `docs/architecture.md`'s "Why plexdb-reader is vendored, not a git dependency" for why it isn't a git dependency instead.

The copy and its drift guard are one step, not two: `crates/etv-station/tests/vendor_plexdb_reader_pin.rs` pins the SHA-256 of `vendor/plexdb-reader/src/`, so a refresh that copies the files without updating `VENDOR_SRC_SHA256` in that test fails `cargo test`. After copying, run the test — it fails naming the hash it actually computed — and paste that hash in as the new constant, in the same commit as the copy.

## Build & run

This is a Cargo workspace with three application crates — `crates/etv-station` (daemon), `crates/etv-query-test` (Phase A CEL harness), and `crates/etv-overlay` (Phase B Vello+Rhai overlay renderer) — plus the vendored `vendor/plexdb-reader` (see above). `vendor/etv-next` is its own workspace, excluded from this one and consumed as a path dependency.

The common operations:

```sh
./tools/dev-run.sh                       # run station daemon + ETV-next together (integration test)
./tools/dev-station.sh                   # run ONLY the station daemon (.env sourced, no ETV-next build)
cargo test --workspace                   # run workspace tests
cargo clippy --workspace --all-features --all-targets -- -D clippy::all   # lint, exactly as CI runs it
cargo +nightly fmt --all                 # format
bun run docs:dev                         # serve VitePress docs on http://localhost:5193
./tools/overlay-test.sh                  # render a Vello overlay onto a bumper fixture and open the mp4
./tools/overlay-still.sh                 # render a single overlay frame to PNG and open it

git subtree pull --prefix=vendor/etv-next etv-upstream main --squash   # absorb upstream ErsatzTV/next
```

The subtree pull is the whole mechanical half of an upstream sync; the judgement half — surveying what changed, checking `schema/playout.json` and the ffmpeg pin, and the keep-ours-then-port conflict doctrine — is in `.claude/skills/upstream-sync/SKILL.md`.

`./tools/dev-run.sh` is the canonical local integration test: it builds both etv-next binaries, starts the station daemon (which writes playout JSON to `examples/output/test/`), starts the ErsatzTV-next HTTP server on `127.0.0.1:8409`, and tees both processes' output with `[station]`/`[etv]` prefixes. Hit `http://127.0.0.1:8409/channel/1.m3u8` for HLS or `/channels.m3u` for the lineup.

Required env for the deploy workflow lives in `.env` (gitignored). See `.env.example` for the shape.

## Documentation

This project has a VitePress docs site under `docs/`. Run `bun run docs:dev` to read it on `http://localhost:5193`.

Keep these in sync as you work:

| File | Update when |
|---|---|
| `docs/PRD.md` | Product behavior, scope, or surface area changes |
| `docs/roadmap.md` | Direction shifts, an initiative ships, or a decision is deferred |
| `docs/architecture.md` | The container/vendoring/IPC story changes |
| `docs/file-map.md` | Major files/folders are added, removed, renamed, or moved |

Don't write new top-level planning / phase / feature docs in `docs/` — file a GitHub issue instead. `roadmap.md` is the only forward-looking doc.

## Issue tracker

Work lives in [GitHub Issues](https://github.com/McBrideMusings/etv-station/issues). The [v1 milestone](https://github.com/McBrideMusings/etv-station/milestone/1) tracks everything required for the v1 acceptance bar in `docs/PRD.md` §Verification. Out-of-scope items are filed under the [`v2` label](https://github.com/McBrideMusings/etv-station/labels/v2).

## Time zone

The station runs in a configurable IANA time zone (`tz` in the station config, overridable via `ETV_STATION_TZ` env var). Persisted timestamps are UTC; tz only affects chunk-boundary alignment so chunks roll on local midnight. See `docs/PRD.md` §Time zone.
