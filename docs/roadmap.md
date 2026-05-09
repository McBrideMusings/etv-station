# Roadmap

> Direction, not task tracking. Concrete work lives in [GitHub Issues](https://github.com/McBrideMusings/etv-station/issues).

## Now

**v1 — Continuous Loop Forever playout** ([milestone](https://github.com/McBrideMusings/etv-station/milestone/1))

Goal: at any moment, every configured channel has playout JSON files on disk whose `[start, finish)` window contains "now" and extends `window_days` into the future, with item metadata populated so ETV-next's XMLTV is correct. Acceptance per [PRD §Verification](/PRD#verification-v1-acceptance) — 7 days continuous, populated XMLTV, zero loader errors.

The 13 v1 issues group into four implicit phases:

- ✅ **Foundations** — config parsing, atomic writes, sample fixtures (#2, #3, #21).
- ✅ **Loop Forever happy path** — rule + chunk slicer + anchor + ffprobe cache + startup scan + roll loop (#1, #4, #5, #9, #10, #12). `./tools/dev-run.sh` now boots station + ETV-next together and serves HLS segments end-to-end.
- **Operational** — reload, retention sweep, structured logging, container ([#6](https://github.com/McBrideMusings/etv-station/issues/6), [#7](https://github.com/McBrideMusings/etv-station/issues/7), [#11](https://github.com/McBrideMusings/etv-station/issues/11), [#13](https://github.com/McBrideMusings/etv-station/issues/13)).
- **Acceptance** — the 7-day soak run against a live ETV-next instance ([#20](https://github.com/McBrideMusings/etv-station/issues/20)).

## Next — three sequential phases of v2+ scope expansion

The v2+ direction extends `etv-station` from a hand-authored Loop Forever generator into a composable, catalog-aware playout system with overlay graphics. See [PRD §Scope evolution beyond v1](/PRD#scope-evolution-beyond-v1) for the framing and rationale.

Each phase is a milestone with a small, focused set of issues. Phases run sequentially because each de-risks the next.

### Phase A — Query language evaluation

Pick a query language by validating off-the-shelf candidates against real-world channel-building cases (TOS marathon, multi-Trek, TNG seasons 3-5, mixed-source bumper block, Dragon Ball franchise chronological, Star Trek in-universe order). Working assumption: [CEL](https://cel.dev/) via `cel-rust`. Fallback: Plex-API pass-through with structured TOML filters.

Per the global "off-the-shelf first" rule (`~/.claude/CLAUDE.md`), we are not designing a custom language.

Deliverable: a standalone `crates/etv-query-test` binary that connects to Plex, accepts queries in candidate languages, prints normalized item lists. Outcome is a documented language decision, not a schema commit.

### Phase B — Graphics rendering

Bring the [ErsatzTV graphics-engine](https://ersatztv.org/docs/advanced/graphics-engine/) concept to etv-next, but with logic authored in a real scripting language ([Rhai](https://rhai.rs/)) instead of YAML. Run as two sub-tracks because they go hand in hand but split cleanly:

- **Static rendering.** Hardcoded channel watermark in a corner using [Vello](https://github.com/linebender/vello). Proves the rendering pipe integrates with etv-next's output stage. Extends `PlayoutItem` with overlay declarations (etv-next submodule change).
- **Scripted behavior.** Add Rhai. Watermark with script-controlled visibility / corner / size / opacity. Fade in/out on time interval (the "logo pulses every minute" pattern). Now-playing / up-next text overlays.

Deliverable: working overlay pipeline in a dev channel, with a small set of declarative + scripted overlay primitives.

### Phase C — Schema overhaul

With the query language picked and graphics rendering working, redesign the channel/block/entries schema and integrate everything:

- New TOML (or YAML) schema with blocks, channels, `[[entries]]`, includes, modes (`all` / `count`), filters, channel-seeded random order.
- Plex catalog ingester + local-FS catalog ingester (bumpers / commercials / errata).
- Runtime query resolution with snapshot-at-boot and configurable refresh interval.
- Graphics overlay cascade: channel default → block override → item override.
- Migration script from current `[rule] type = "loop_forever"` configs.

## Later

- **Lottie animation runtime spike** — designer-friendly After Effects format for richer overlays via [`velato`](https://github.com/linebender/velato). Tracked as a side project; the maintainer can author equivalent behavior in Rhai for now.
- [Recurring grid rule](https://github.com/McBrideMusings/etv-station/issues/14) — fixed-time blocks. Likely subsumed by Phase C once a fixed-time-block primitive returns.
- [Live event injection](https://github.com/McBrideMusings/etv-station/issues/16) — operator declares a one-shot override window.
- [Web UI for editing channels and items](https://github.com/McBrideMusings/etv-station/issues/17) — once channel count grows past TOML-by-hand ergonomics.
- Public open-source release — revisit once the rule abstraction is validated by 2+ rule implementations.

## Deferred / won't fix

- Real-time control plane (REST API, network injection) — v1 is config-file driven by design. Reload signal + file-watcher are sufficient.
- Encoding decisions (ffmpeg invocation for transcode, hwaccel selection) — ETV-next's responsibility. `etv-station` only reads media metadata via `ffprobe` for duration.
- Forking ETV-next to add scheduling — eats merge conflicts forever. Companion-program approach was chosen specifically to avoid this.
- [Library importer (Plex / Jellyfin / Sonarr metadata) as a separate tool](https://github.com/McBrideMusings/etv-station/issues/18) — **superseded by Phase C's runtime catalog ingester.** Issue #18 stays open until Phase C lands, then closes with a pointer.
- [Random / shuffle rule](https://github.com/McBrideMusings/etv-station/issues/15) — **subsumed by Phase C's `order = "random"` with channel-level seed.** Closes with Phase C.
