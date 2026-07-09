# Reload-generation prepare failures revert to the last-known-good config

The station daemon reloads config on SIGHUP by tearing down the current generation and preparing a new one (`prepare_generation`: tz-parse + overlay-validate + `output_folder` mkdir). A prepare failure on the **first** generation is fatal — fail loud at startup. But a failure on a **reload** generation must not kill a daemon that was streaming fine, so it reverts to the last config that prepared cleanly and re-spawns that instead.

## Why

The reload contract is "a bad edit keeps the previous config running." That was only half-honoured: config *parse* failures fell back to the previous config, but generation-setup failures (`tzmod::parse`, `validate_overlay_configs`, and — after #34 — the `create_dir_all` mkdir) used `?` and killed the daemon. The mkdir case is the live one: an uncreatable `output_folder` (disk full, an unmounted volume, a permissions blip) on reload would tear down healthy channels. See #90.

## What we chose, and the rejected alternatives

- **One prepare path, not two.** `prepare_generation` is the single home for every "is this config runnable" check; `run` calls it once per generation on both the startup and reload paths, and the reload's `config::load` no longer re-validates. The bug existed precisely because checks were split across two load paths, so `load_and_validate` gained tz+overlay but nobody added mkdir to it — a check added in one place silently skipped the other.
- **Whole-generation revert, not per-channel skip.** If new config `{A,B,C}` has A's folder uncreatable, reverting keeps old A, B, and C all streaming; skipping A would instead drop a channel that was live. Revert is both simpler and keeps more channels up, and it matches the existing "config swaps as an atomic unit" model.
- **`Arc::ptr_eq` infinite-loop guard.** Reverting points `station` at `last_good`; if that config *also* fails to prepare (e.g. total disk failure affecting every folder, or its overlay file was deleted), the pointers match and the daemon exits rather than looping forever. Nothing runnable is nothing to fall back to.
