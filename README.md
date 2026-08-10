# etv-station

Playout-JSON generator daemon for [ErsatzTV-next](https://github.com/ErsatzTV/next).

`etv-station` is the operator-side companion to ETV-next. ETV-next does transcoding
and streaming (playout JSON → HLS + XMLTV) but explicitly leaves scheduling and
playout creation out of scope. `etv-station` fills that gap: it reads channel
config, applies a sequencing rule, and continuously writes the
`{start}_{finish}.json` playout files ETV-next consumes — so every configured
channel always has JSON on disk whose `[start, finish)` window covers "now" and
extends N days into the future.

The two ship in one container over one playout folder. The only coupling is
the playout JSON schema (a path dependency on the vendored ETV-next source, so
schema drift is a compile-time error) and the directory-layout convention, derived
from the station config rather than authored twice.

```
┌─────────────── one container ───────────────┐
│ ┌────────────────┐ writes  ┌──────────────┐ │
│ │  etv-station   │ ──────▶ │ playout dir  │ │
│ │  rules → JSON  │         │ {start}_{fin}│ │
│ └────────────────┘         │ .json / chan │ │
│ ┌────────────────┐  reads  └──────────────┘ │
│ │  etv-next      │ ◀───────────────┘        │
│ │ JSON→HLS+XMLTV │                          │
│ └────────┬───────┘                          │
└──────────┼──────────────────────────────────┘
           │ HTTP
           ▼
   IPTV clients (Plex, Jellyfin, Kodi, …)
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

Everything needed to build is in the one repo — no submodules:

```sh
git clone git@github.com:McBrideMusings/etv-station.git
```

[ErsatzTV/next](https://github.com/ErsatzTV/next) is vendored under
`vendor/etv-next/`, along with this project's modifications to it, as ordinary
tracked files. Edit them here; upstream is absorbed with a real merge:

```sh
git remote add etv-upstream https://github.com/ErsatzTV/next   # once per clone
git subtree pull --prefix=vendor/etv-next etv-upstream main --squash
```

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
- [Architecture](docs/architecture.md) — the container / vendoring / IPC story.
- [Roadmap](docs/roadmap.md) — direction and what's deferred.
