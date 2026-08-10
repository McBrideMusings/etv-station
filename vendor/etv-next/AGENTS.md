# AGENTS.md

This file provides guidance to AI agents, e.g. Claude Code (claude.ai/code) when working with code in this repository.

## What is this?

ErsatzTV (next) is a Rust rewrite of ErsatzTV — a self-hosted IPTV server that transcodes and streams media as live TV
channels over HTTP/HLS. It intentionally excludes library management and scheduling; it consumes pre-defined playout
JSON files and handles transcoding/streaming.

## Where this checkout lives

This is not a standalone repository. It is [ErsatzTV/next](https://github.com/ErsatzTV/next)
vendored into `etv-station` at `vendor/etv-next/`, together with that project's
changes to it, as ordinary tracked files. There is no fork repository. A change
here is a normal commit in `etv-station`.

- **`origin`** is `McBrideMusings/etv-station` — pushes go there, like any other
  change to that repo.
- **`etv-upstream`** is `https://github.com/ErsatzTV/next` — **never push there.**
  Pulling from it is the entire point; the user is not a maintainer of that repo.

Upstream is absorbed with a real merge, run from the `etv-station` root:

```sh
git subtree pull --prefix=vendor/etv-next etv-upstream main --squash
```

Keep a change under `vendor/etv-next/` in its own commit, separate from
station-side changes — the next upstream merge is far easier to read when the
two are not mixed. See `.claude/skills/upstream-sync/SKILL.md` in `etv-station`
for the survey, the two files that need special handling, and the conflict
doctrine.

**If a change here belongs upstream** (a plain bug fix rather than something
specific to this station), that is a deliberate, separate workflow the user
initiates: branch from `etv-upstream/main` in a clean checkout, apply just that
change, and open a PR against `ErsatzTV/next`. Do not start that flow unasked.

## Build & Development Commands

```bash
# Build
cargo build --workspace --all-features

# Run the IPTV server
cargo run --bin ersatztv -- <path/to/lineup.json>

# Scaffold a new lineup with N channels (creates lineup.json, hls/, channels/<N>/{channel.json,playout/})
cargo run --bin ersatztv -- add-lineup <path/to/lineup.json> --channels <N>

# Add a channel to an existing lineup
cargo run --bin ersatztv -- add-channel <path/to/lineup.json> --number <X>

# Run a single channel worker (usually spawned by the server)
cargo run --bin ersatztv-channel -- run <path/to/channel.json> --output-folder <dir> --number <N>

# Debug channel config and FFmpeg capabilities
cargo run --bin ersatztv-channel -- debug <path/to/channel.json>

# Generate test playout from video files (explicit output folder)
cargo run --bin ersatztv-playout-generator -- --content-folder <dir> --output-folder <dir>

# Generate test playout for a channel in a lineup (resolves the playout folder from channel.json)
cargo run --bin ersatztv-playout-generator -- --content-folder <dir> --lineup <path/to/lineup.json> --channel <N>

# Lint
cargo clippy --locked --workspace --all-features --all-targets -- -D clippy::all

# Format (requires nightly)
cargo +nightly fmt --all

# Format check
cargo +nightly fmt --all -- --check

# There are 2 styles of tests in the repository currently, unit and lightweight integration
# Lightweight integration tests are disabled by default because they require local ffmpeg
# binaries.

# Running the tests:
cargo test

# Run all integration tests explicitly
cargo test --package ffpipeline -- --ignored

# Run just software or hardware tests
cargo test --package ffpipeline --test software -- --ignored
cargo test --package ffpipeline --test videotoolbox -- --ignored
```

## Architecture

### Process Model

The server (`ersatztv`) spawns a separate `ersatztv-channel` subprocess per active channel. Processes communicate via
file-based signaling (`.ready` and `.heartbeat` files) — no IPC. The main server monitors these files with tokio watch
channels.

### Crate Structure

- **`ersatztv`** — Axum HTTP server. Serves M3U/M3U8 playlists + XMLTV EPG, manages channel process lifecycle via
  `ChannelSession::spawn()`. Routes: `/channels.m3u`, `/xmltv.xml`, `/channel/{N}.m3u8`, `/session/{channel}/{file}`.
- **`ersatztv-channel`** — Per-channel worker. Reads playout JSON, builds FFmpeg pipelines, generates HLS segments. Has
  a 4-state machine (`SeekAndWorkAhead` → `ZeroAndWorkAhead` → `SeekAndRealtime` → `ZeroAndRealtime`) for buffering
  strategy.
- **`ffpipeline`** — FFmpeg pipeline builder. Probes source media, selects hardware acceleration, constructs filter
  chains, generates ffmpeg command-line args. Key trait: `HwAccel` with implementations for CUDA, QSV, VAAPI,
  VideoToolbox.
- **`ersatztv-playout`** — Playout JSON data models (serde). Schema at `schema/playout.json` is hand-maintained - keep it in sync when editing the Rust types.
- **`ersatztv-core`** — Shared utilities: heartbeat/ready file management, timing constants.
- **`ersatztv-playout-generator`** — Dev tool for generating playout JSON from video folders or syncing from legacy DB.
- **`libnvidia-sys`, `libva-sys`, `libvpl-sys`** — FFI bindings for hardware acceleration capability detection.
  Platform-specific with stub fallbacks.

### Configuration Tiers

1. **`lineup.json`** — Server bind address, port, output folder, list of channels (each referencing a channel config)
2. **`channel.json`** — Playout folder, FFmpeg paths, normalization settings (video codec/resolution/bitrate, audio codec/bitrate, hardware acceleration)
3. **Playout JSON files** — Named `{start}_{finish}.json` with ISO 8601 timestamps. Loaded on-demand based on current time.

### Key Design Decisions

- Hardware acceleration is auto-detected at runtime via FFI capability probing, with graceful fallback
- HLS segments are 4 seconds; keyframe interval is 2 seconds
- The server is stateless — all state lives in config files and the filesystem (HLS segments, signal files)
- Playout files can be updated on disk without restarting the server

## Design principle: the playout JSON is the contract

ErsatzTV-next consumes playout JSON files and produces HLS streams + an XMLTV EPG. It does **not** manage a media
library, scrape metadata, query external databases, or know anything about how playout JSON is produced. Everything a
downstream consumer (Plex, Jellyfin, Kodi, Channels DVR, etc.) needs about a program — title, episode, description,
artwork, rating, etc. — must already be present on the `PlayoutItem`. If a field is missing, the EPG simply omits it.

**Do not propose features that pull metadata from external sources** (legacy SQLite DBs, scrapers, NFOs, online
providers, library managers). That work belongs in whatever tool generates the JSON, not in this repo. New features
that touch program data should add fields to the playout schema and read from there — never reach outward.
