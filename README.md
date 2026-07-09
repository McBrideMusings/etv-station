# etv-station

Playout-JSON generator daemon for [ErsatzTV-next](https://github.com/ErsatzTV/next).

`etv-station` is the operator-side companion to ETV-next. ETV-next does transcoding
and streaming (playout JSON → HLS + XMLTV) but explicitly leaves scheduling and
playout creation out of scope. `etv-station` fills that gap: it reads channel
config, applies a sequencing rule, and continuously writes the
`{start}_{finish}.json` playout files ETV-next consumes — so every configured
channel always has JSON on disk whose `[start, finish)` window covers "now" and
extends N days into the future.

The two run as separate containers over one shared volume. The only coupling is
the playout JSON schema (pinned via the `etv-next` git submodule, so schema drift
is a compile-time error) and the directory-layout convention.

```
┌────────────────┐  writes   ┌──────────────────┐  reads   ┌────────────────┐
│  etv-station   │ ────────▶ │  shared volume   │ ◀─────── │  etv-next      │
│  rules → JSON  │           │  {start}_{fin}.  │          │  JSON→HLS+XMLTV│
└────────────────┘           │  json per chan   │          └────────────────┘
                             └──────────────────┘                  │ HTTP
                                                                    ▼
                                                          IPTV clients (Plex,
                                                          Jellyfin, Kodi, …)
```

## Status

Early development — not yet at the v1 acceptance bar. What exists today:

- **Loop Forever daemon** (`crates/etv-station`) — config parser, anchor sidecar,
  ffprobe duration cache, chunk slicer, roll loop, IANA time-zone handling,
  SIGHUP config reload.
- **Overlay renderer** (`crates/etv-overlay`) — Vello + Rhai graphics overlay
  cascade (Phase B).
- **CEL query harness** (`crates/etv-query-test`) — Phase A experiment for the
  catalog query language.

In flight: the [Phase C schema overhaul](https://github.com/McBrideMusings/etv-station/milestone/4)
(block/channel/entries schema, Plex + local-FS catalog ingesters, runtime query
resolution).

- v1 acceptance (7-day continuous soak, populated XMLTV, zero loader errors) is
  tracked by the [v1 milestone](https://github.com/McBrideMusings/etv-station/milestone/1).
- Out-of-scope-for-v1 ideas live under the [`v2` label](https://github.com/McBrideMusings/etv-station/labels/v2).
- All work is tracked in [GitHub Issues](https://github.com/McBrideMusings/etv-station/issues).

## Clone

This repo is private and pulls ETV-next in as a submodule for build-time access to
the playout schema. Clone with submodules:

```sh
git clone --recurse-submodules git@github.com:McBrideMusings/etv-station.git
```

If you already cloned without `--recurse-submodules`:

```sh
git submodule update --init --recursive
```

> **Do not edit files under `etv-next/`** from this repo — it's a submodule
> pointing at the ETV-next source. Schema changes are made upstream, then absorbed
> by bumping the submodule SHA.

## Build & run

A Cargo workspace with three crates. Common operations:

```sh
./tools/dev-run.sh                             # station daemon + ETV-next together (integration test)
cargo test --workspace                         # run workspace tests
cargo clippy --workspace -- -D clippy::all     # lint (deny all warnings)
cargo +nightly fmt --all                       # format
bun run docs:dev                               # serve the docs on http://localhost:5193
```

`./tools/dev-run.sh` is the canonical local integration test: it builds both
ETV-next binaries, starts the station daemon (writing playout JSON to
`examples/output/test/`), starts the ETV-next HTTP server on `127.0.0.1:8409`, and
tees both processes' output. Then hit `http://127.0.0.1:8409/channel/1.m3u8` for
HLS or `/channels.m3u` for the lineup.

## Docs

Full docs are a VitePress site under `docs/` (`bun run docs:dev`). Start here:

- [PRD](docs/PRD.md) — what it does, scope, verification bar.
- [Architecture](docs/architecture.md) — the container / submodule / IPC story.
- [Roadmap](docs/roadmap.md) — direction and what's deferred.
