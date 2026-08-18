# The overlay cascade hot-reloads the running process; it never respawns it

A station→channel→block overlay cascade resolves to one whole config, deepest declared level wins, no merge — but applying a *different* resolved config at a block boundary reloads it into the channel's already-running `etv-overlay pipe` process rather than killing and respawning that process. Only a `width`/`height`/`framerate`/`pixel_format` change is rejected at config-load time, because those are baked into the fifo's byte layout.

## Why

This project already vendored `crates/etv-overlay` and its own `OverlaySpec`/TOML format entirely in-house (`cb57e06`) — no upstream ErsatzTV/next concept constrains it. #48 ("Overlay cascade: channel → block → item selects which overlay config runs") settled that the cascade resolves *configuration*, not composition: `PlayoutItem.overlay` is singular, one channel runs exactly one overlay process, and a block/item's job is to pick which whole config that process runs, deepest level wins. That issue left one thing unresolved: what happens mechanically when a block boundary resolves to a config genuinely different from its parent's — different script, different layers, not just a different variable value.

The naive answer is respawn: track which config is loaded, kill the process, spawn a fresh one with the new config. `overlay_supervisor.rs`'s existing demand-driven spawn/despawn machinery (the `.overlay-wanted`/`.overlay-ready` handshake) already knows how to do this — it just doesn't know how to do it *without* a visible gap, because the handshake exists precisely to let `etv-next`'s ffmpeg block on `open()` until a fresh writer is ready. A respawn at a block boundary would reproduce that same warm-up gap mid-stream, which is a real, viewer-visible glitch every time a block airs a different overlay from the one before it.

The station already has the mechanism to avoid this. `program_context.rs` mtime-polls the playout JSON at 1Hz and feeds fresh values into the *same running* Rhai script — proof that this process already tolerates external state changing under it without restarting. Extending that same poll to also cover script/layers/config (not just per-frame variables) costs nothing new in the fifo/ffmpeg protocol: the reader relationship between `etv-next` and the overlay's fifo never breaks, only what the existing writer draws into it changes.

## What we chose, and the rejected alternative

- **Hot-reload script + layers + config in place, chosen.** The daemon resolves the cascade whenever a block boundary is crossed and writes the resolved config to the same polled location `program_context.rs` already watches. `etv-overlay pipe` reloads script and layers when it sees that file change. No process kill, no fifo teardown, no `.overlay-ready` handshake replay, no frame gap.

- **Respawn on every config change, rejected.** Delivers the same outcome — literally implements #48 as originally scoped, block/item genuinely swapping which script/layers run — but reproduces the cold-start warm-up gap the existing `.overlay-ready` handshake was built to hide from a *channel's first watcher*, except now it recurs at every block boundary a viewer sits through. Rejected because the hot-reload path gets the identical outcome for free.

- **`width`/`height`/`framerate`/`pixel_format` stay respawn-only (or rather, load-time-rejected), by necessity, not by choice.** These are baked into the fifo's byte layout, which ffmpeg's overlay filter graph is sized to at *its own* startup. A block/item-level config declaring different dimensions than the channel process was spawned with fails to load rather than silently attempting a live resize — a genuine, accepted gap in the feature, not something hot-reload can be extended to cover without restarting ffmpeg's filter chain itself (a strictly worse, more visible interruption than an overlay-process respawn would have been).

## Consequences

**A block/item can differ from its parent by script and layers, seamlessly.** #48's original acceptance criteria ("a block-level declaration overrides the channel's for items in that block") is satisfiable without the respawn cost #48 implicitly assumed.

**Dimension changes across cascade levels are a hard error, not a feature gap to revisit casually.** An author who wants a block-specific overlay with different pixel dimensions than its channel is told so at load time. Lifting this later means either restarting ffmpeg's filter chain at the boundary (a worse glitch than the respawn this ADR avoids) or picking one fixed dimension for the whole channel and living with it — not a small follow-on change.

**The supervisor gains a second thing to watch per channel.** Today it watches two marker files (`OVERLAY_WANTED_FILE_NAME`, `OVERLAY_READY_FILE_NAME`) for spawn/despawn. This adds a third watched artifact — the resolved-config file `etv-overlay pipe` itself polls — though the poll and reload live inside the overlay process, not the supervisor; the supervisor's own spawn/despawn logic is unchanged by this ADR.
