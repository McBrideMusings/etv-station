# Roadmap

> Direction, not task tracking. Concrete work lives in [GitHub Issues](https://github.com/McBrideMusings/etv-station/issues).

## Now

**v1 — Continuous Loop Forever playout** ([milestone](https://github.com/McBrideMusings/etv-station/milestone/1))

Goal: at any moment, every configured channel has playout JSON files on disk whose `[start, finish)` window contains "now" and extends `window_days` into the future, with item metadata populated so ETV-next's XMLTV is correct. Acceptance per [PRD §Verification](/PRD#verification-v1-acceptance) — 7 days continuous, populated XMLTV, zero loader errors.

The 13 v1 issues group into four implicit phases:

- **Foundations** — config parsing, atomic writes, CI ([#2](https://github.com/McBrideMusings/etv-station/issues/2), [#3](https://github.com/McBrideMusings/etv-station/issues/3), [#8](https://github.com/McBrideMusings/etv-station/issues/8)).
- **Loop Forever happy path** — rule + chunk slicer + anchor + ffprobe cache + startup scan + roll loop ([#1](https://github.com/McBrideMusings/etv-station/issues/1), [#4](https://github.com/McBrideMusings/etv-station/issues/4), [#5](https://github.com/McBrideMusings/etv-station/issues/5), [#9](https://github.com/McBrideMusings/etv-station/issues/9), [#10](https://github.com/McBrideMusings/etv-station/issues/10), [#12](https://github.com/McBrideMusings/etv-station/issues/12)).
- **Operational** — reload, retention sweep, structured logging, container ([#6](https://github.com/McBrideMusings/etv-station/issues/6), [#7](https://github.com/McBrideMusings/etv-station/issues/7), [#11](https://github.com/McBrideMusings/etv-station/issues/11), [#13](https://github.com/McBrideMusings/etv-station/issues/13)).
- **Acceptance** — the 7-day soak run against a live ETV-next instance.

## Next

**v2 — More rules, less hand-editing** ([label](https://github.com/McBrideMusings/etv-station/labels/v2))

Once Loop Forever is stable and the rule trait has shipped one real implementation, the natural follow-ups are more rules. Filed as placeholders:

- [Recurring grid rule](https://github.com/McBrideMusings/etv-station/issues/14) — weekday/time slots with a fallback loop.
- [Random / shuffle rule](https://github.com/McBrideMusings/etv-station/issues/15) — pool with no-repeat-window + per-item weights.
- [Live event injection](https://github.com/McBrideMusings/etv-station/issues/16) — operator declares a one-shot override window.

## Later

- [Web UI for editing channels and items](https://github.com/McBrideMusings/etv-station/issues/17) — once channel count grows past TOML-by-hand ergonomics.
- Public open-source release — revisit once the rule abstraction is validated by 2+ rule implementations.

## Deferred / won't fix

- [Library importer (Plex / Jellyfin / Sonarr metadata)](https://github.com/McBrideMusings/etv-station/issues/18) — explicit non-goal per [PRD §Non-goals](/PRD#non-goals). If implemented, belongs in a separate tool that emits TOML/JSON consumed by `etv-station`, not in the daemon itself.
- Real-time control plane (REST API, network injection) — v1 is config-file driven by design. Reload signal + file-watcher are sufficient.
- Encoding decisions (ffmpeg invocation for transcode, hwaccel selection) — ETV-next's responsibility. `etv-station` only reads media metadata via `ffprobe` for duration.
- Forking ETV-next to add scheduling — eats merge conflicts forever. Companion-program approach was chosen specifically to avoid this.
