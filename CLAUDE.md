# etv-station

`etv-station` is a standalone playout-JSON generator daemon for [ErsatzTV-next](https://github.com/ErsatzTV/next). It reads station/channel TOML configs, applies a sequencing rule (v1: Loop Forever), and writes playout JSON files to a shared volume that ETV-next consumes.

## Architecture in one sentence

Two containers, one shared filesystem, one shared schema: `etv-station` writes `{start}_{finish}.json` files; ETV-next reads them, produces HLS + XMLTV. The schema is pinned via the `etv-next/` git submodule so drift is a compile-time error.

See `docs/architecture.md` for the full picture and `docs/PRD.md` for the spec.

## Submodule rules — DO NOT EDIT FROM THIS REPO

`etv-next/` is a submodule pointing at `McBrideMusings/etv-next-private`. **Never make changes to files under `etv-next/` from this repo.** Schema / pipeline changes are made in the etv-next checkout proper, then absorbed here by bumping the submodule SHA. Touching `etv-next/` from inside `etv-station` will silently lose work on the next submodule update.

## Build & run

This is a Cargo workspace with three crates — `crates/etv-station` (daemon), `crates/etv-query-test` (Phase A CEL harness), and `crates/etv-overlay` (Phase B Vello+Rhai overlay renderer). The `the project task runner` script wraps the common operations:

```sh
./tools/dev-run.sh            # run station daemon + ETV-next together (integration test)
cargo test --workspace           # cargo test --workspace
cargo clippy --workspace -- -D clippy::all            # clippy with -D clippy::all
cargo +nightly fmt --all            # nightly rustfmt
bun run docs:dev           # serve VitePress docs on http://localhost:5193
./tools/overlay-test.sh   # render a Vello overlay onto a bumper fixture and open the mp4
./tools/overlay-still.sh  # render a single overlay frame to PNG and open it
the deploy task runner          # build linux/amd64 Docker image (provided by docker-unraid archetype)
the deploy task runner         # build + recreate container on Unraid (env: APP_IMAGE, UNRAID_HOST, UNRAID_USER)
```

`./tools/dev-run.sh` is the canonical local integration test: it builds both etv-next binaries, starts the station daemon (which writes playout JSON to `examples/output/test/`), starts the ErsatzTV-next HTTP server on `127.0.0.1:8409`, and tees both processes' output with `[station]`/`[etv]` prefixes. Hit `http://127.0.0.1:8409/channel/1.m3u8` for HLS or `/channels.m3u` for the lineup.

Direct cargo commands work fine too. `the task-runner config` is the source of truth — `the project task runner` is generated. Edit `the task-runner config`, then `regenerate via the project task runner`.

Required env for deploy commands lives in `.env` (gitignored). See `.env.example` for the shape.

## Documentation

This project has a VitePress docs site under `docs/`. Run `bun run docs:dev` (or `bun run docs:dev`) to read it on `http://localhost:5193`.

Keep these in sync as you work:

| File | Update when |
|---|---|
| `docs/PRD.md` | Product behavior, scope, or surface area changes |
| `docs/roadmap.md` | Direction shifts, an initiative ships, or a decision is deferred |
| `docs/architecture.md` | The container/submodule/IPC story changes |
| `docs/file-map.md` | Major files/folders are added, removed, renamed, or moved |

Don't write new top-level planning / phase / feature docs in `docs/` — file a GitHub issue instead. `roadmap.md` is the only forward-looking doc.

## Issue tracker

Work lives in [GitHub Issues](https://github.com/McBrideMusings/etv-station/issues). The [v1 milestone](https://github.com/McBrideMusings/etv-station/milestone/1) tracks everything required for the v1 acceptance bar in `docs/PRD.md` §Verification. Out-of-scope items are filed under the [`v2` label](https://github.com/McBrideMusings/etv-station/labels/v2).

## Time zone

The station runs in a configurable IANA time zone (`tz` in `station.toml`, overridable via `ETV_STATION_TZ` env var). The persisted anchor is UTC; tz only affects chunk-boundary alignment so chunks roll on local midnight. See `docs/PRD.md` §Time zone.
