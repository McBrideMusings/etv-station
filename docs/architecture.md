# Architecture

Quick reference. The full rationale lives in [PRD §Architecture](/PRD#architecture); this page exists so you don't have to scroll the PRD when you just want the picture.

## Two programs, one shared filesystem, one shared schema

```
                                ┌─────────────────────┐
                                │  shared volume      │
                                │  /playout/<chan>/   │
                                │    {start}_{finish}.json
                                │                     │
┌────────────────────┐  writes  │                     │  reads  ┌────────────────────┐
│  etv-station       │ ────────▶│                     │ ◀────── │  etv-next          │
│  container         │          └─────────────────────┘         │  container         │
│                    │                                          │                    │
│  rules → JSON      │                                          │  JSON → HLS+XMLTV  │
└────────────────────┘                                          └────────────────────┘
```

- **etv-station** has read/write on the playout volume. Computes "what plays when," writes JSON.
- **etv-next** has read-only on the same volume. Loads the JSON file whose `[start, finish)` covers "now," produces HLS + XMLTV.
- Coupling is exactly two things: the playout JSON schema (pinned via the `etv-next` submodule + Rust path-dep) and the directory layout convention.

## Why a submodule

`etv-station` depends on `etv-next-private/crates/ersatztv-playout` as a Rust path dependency through a git submodule pinned to a specific commit:

- Schema drift becomes a compile-time question. If upstream renames a field, `cargo build` fails before any test runs.
- Adopting an upstream schema change is a deliberate two-step: pull `origin/main` into the submodule, bump the submodule SHA in `etv-station`, rebuild.
- No vendoring (which would re-introduce drift), no crates.io dependency on Jason Dove (which he hasn't published).

## Why a separate program (not a fork of etv-next)

ETV-next's README is explicit: "Library and metadata management, scheduling and playout creation are not in scope for this project." Forking to add scheduling would mean eating merge conflicts on every pipeline-side PR forever. The companion-program approach keeps ETV-next's pipeline work and `etv-station`'s rule work on independent release cadences.

## Why two containers

Honest separation of failure domains:

- `etv-station` can crash, leak memory, get stuck on a bad rule — `etv-next` keeps streaming the materialized window.
- Independent restart cadence, resource limits, images, CI.
- Cost is one extra container; benefit is graceful degradation when something goes wrong on the planning side.

## Why filesystem-only IPC

Matches ETV-next's existing process model — it already uses files (`.ready`, `.heartbeat`) for signaling between server and channel subprocesses. No new protocol surface. `ls` shows you the state. Atomic emission via `rename(2)` means the consumer can't observe a half-written file.

## Why Rust

Because ETV-next is Rust and the submodule + path-dep approach is essentially free in Rust. Any other language would either re-implement the schema models (drift risk) or codegen them (added build complexity, weaker typing). Sharing serde models on the producer and consumer side is the fastest path to "schema drift is impossible at compile time."

## Determinism and reload

Loop Forever is deterministic in `(anchor, items)` — same inputs always produce the same output. This is what makes config reload safe: re-rendering a future-window file in place is idempotent. Past files are immutable; only the unmaterialized window is touched on reload.

## Time zones

Configurable station-wide via `tz` in `station.toml` (or `ETV_STATION_TZ` at runtime). Affects chunk-boundary alignment only — the persisted UTC anchor doesn't move. See [PRD §Time zone](/PRD#time-zone).
