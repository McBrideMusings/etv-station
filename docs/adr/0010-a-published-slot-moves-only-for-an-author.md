---
applies-to:
  - crates/etv-station/src/daemon.rs
  - crates/etv-station/src/resume.rs
  - crates/etv-station/src/resolve.rs
---

# A published slot moves only for an author

The regeneration fingerprint hashes a channel's config and resolved overlay, and
nothing else. Catalog drift — a film arriving in Plex, an item crossing the watched
boundary — never rewinds an already-published schedule. New content reaches the
screen at the window frontier on the ordinary roll tick, not by rewriting a future a
viewer has already been shown.

## Why

`channel_input_fingerprint` (`crates/etv-station/src/daemon.rs:1916`) hashed three
things into one SHA-256 stored as `Checkpoint::inputs`
(`crates/etv-station/src/resume.rs:113`): the resolved candidate entry-id list, the
channel config bytes, and the resolved overlay bytes. A mismatch rewinds the channel
to its earliest unaired checkpoint and regenerates from there
(`daemon.rs:2085-2136`).

One hash, two unrelated questions. A config or overlay change means the author
edited a file and wants it on the screen — that is what the rewind exists for (#53).
A candidate-id change means Plex ingested something; nobody asked for anything. Both
produced the same full rewind, so for a broad-pool channel like `examples/samples/foryou.yaml`
a restart could reorder up to `window_days` of published schedule with no author
involved.

Seeding the scorer (#324, #325) makes a regeneration reproducible over *unchanged*
inputs. It does nothing here, because the inputs genuinely changed. The re-run is
correct and the reshuffle is still unwanted.

## What we chose, and the rejected alternatives

- **Drop candidate ids from the hash, rather than branch on them.** The first design
  split `Checkpoint::inputs` into two fields so each cause could carry its own
  policy. Once the policy for drift was "do nothing," the second field was written on
  every checkpoint and read by nothing, and `resolve_channel_fingerprint_ids`
  (`crates/etv-station/src/resolve.rs:478`) kept running a real pool query once per
  roll tick — every 60 seconds on `foryou.yaml` — to produce it. Removing the input
  beats storing a flag about it.

- **No stability horizon, no debounce, no threshold.** Each is a guard over the
  conflation above, and each needs a number nobody can pick without watching a
  channel for a week. Every channel and sample in the repo sets `window_days: 1`
  (`crates/etv-station/src/config/channel.rs:300` is the default), so the published
  window is at most a day and a horizon buys almost nothing over freezing it whole. A
  threshold is worse than either: it makes the viewer-facing guarantee
  "stable unless enough changed," which cannot be stated.

- **Freshness is not lost, it is relocated.** `pattern_catch_up` extends the frontier
  every `roll_interval` using the refreshed catalog and re-runs the scorer there.
  `foryou.yaml:57-60` already argues this is the design: a short window means the
  ranking is at most hours stale when it airs. What this ADR forbids is rewriting the
  span *behind* the frontier, which no freshness argument asks for.

- **A broken slot is not stale, and still moves.** An unaired item whose catalog entry
  has `missing_since` set is not merely out of date — it cannot play. The
  reconciliation sweep substitutes an error card in place
  (`crates/etv-station/src/reconcile.rs:197-209`) because `PlayoutItem` carries fixed
  `start`/`finish` (`vendor/etv-next/crates/ersatztv-playout/src/playout.rs:67-70`)
  and a real replacement would not fill the span. So the startup path rewinds from
  that item's own `start` instead — narrower than the earliest unaired checkpoint,
  and the one case where the published schedule is wrong rather than old.

## Consequences

- The guarantee is now statable: **once a slot is published, only an author's edit
  moves it.** A viewer who reads the guide at 14:00 sees the same lineup at 14:30.
- A SIGHUP with an unchanged config is a complete no-op for the unaired window.
- Checkpoints written before this change carry a hash covering all three inputs, so
  they mismatch once on the first restart and regenerate. `Option::None` and a stale
  hash already both mean "regenerate," so no migration is needed.
- Catalog refresh (`daemon.rs:1130`) never triggered this rewind and still does not.
  It re-ingests and runs the reconciliation sweep; the fingerprint compare has one
  call site, in `channel_loop`'s startup block.
