# An audit trail is a stage list in the playout JSON, and a plugin explains itself in a function

Every scheduled item carries an `audit` array in its `metadata`: one record per mechanism that acted on it, in the order they acted, each `#{ stage, by, verdict, detail }`. It is written as the item is laid down and rides the chunk JSON to disk beside it. A `pool_provider` plugin produces its own records from a fifth contract function, `audit(ctx, picks)`, called once per generation immediately after `pick()` and handed back the list `pick()` returned.

## Why

A channel that looks wrong is unanswerable today. `taste-debug` re-runs a scorer's `pick()` in isolation against the current catalog and reports what the plugin *would* choose now — which is a different question from why the film that aired at 20:00 aired at 20:00, because the catalog, the resume cursor and the watch history have all moved since. For the 60 channels with no `plugin:` pool it cannot say anything at all: `ResolvedItem.block` knows which block produced an item and is consumed by the overlay timeline, and nothing else about the decision survives the generation that made it.

The transport for fixing that already exists and already carries bytes. `ResolvedItem.metadata` (`resolve.rs:104`) reaches `PlayoutItem::metadata` (`rule.rs:194`) and lands in `{start}_{finish}.json` untouched, which is how a plugin pick's `score` and `on_profile_keywords` already reach disk. Recording the decision at the moment it is made, in the file that records the decision's outcome, means the report describes what actually aired rather than what a re-simulation believes would air.

A single field cannot hold it. Several mechanisms act on one item in sequence — a pool ranked it, a `select` drew it, `constraints: no_repeat_within` kept or moved it, the pattern's `take` placed it — and each has something different to say. A flat `#{ by, why }` record forces four true statements into one, and the field that loses is whichever the last writer overwrote.

## What we chose, and the rejected alternatives

- **A list of stages, not a flat record.** The flat form was the first design and it failed against real config: a `001-for-you` movie is picked by `taste-cosine`, drawn by `select: round_robin`, placed by `take: 2`, and on `002-for-pierce` is then subject to `no_repeat_within: 24h` (`channel.yaml:80`), which runs after the pool and can move it. All four are true and a singular `by` holds one.
- **`stage` is a closed set the station owns; `detail` stays opaque.** A report has to be filterable across 64 channels that pick things four different ways, and that needs a shared axis. What each stage says about its own decision is not the station's business — the same split ADR 0002 draws around plugin `metadata`.
- **The chunk JSON, not a sidecar `{start}_{finish}.audit.json`.** A sidecar keeps the playout file at its current size and keeps the bytes away from ETV-next, which is real. It also introduces a second artifact with its own retention, its own drift, and its own hole after a coverage heal — the failure class `--backfill-history` exists to repair. Measured against `examples/output/test/` (26 chunks, 29,783 bytes average, 11 items each), an item is already 2.7KB and an audit trail is 400–800 bytes: 15–30%, which does not buy a second lifecycle.
- **Recorded at generation, not recomputed on demand.** Recomputation is free, needs no schema and no disk, and `determinism.rs` proves generation reproduces from pinned inputs. The inputs are not pinned in the case that matters: diagnosing something that already aired means reading a catalog, a resume cursor and a watch history that have all moved since it was scheduled.
- **A function, not a key on `pick()`'s record.** `#{ entry_id, metadata, audit }` is the smaller change and needs no new sandbox rule. But a per-item key can only describe that item, and the losers are gone by the time the record is written — so "ranked 3rd, chosen over two the script's own replay TTL suppressed" is inexpressible. `audit(ctx, picks)` still has the scoring table in scope, which is the only place in the system where a rejection can be named at all.
- **Required of a `pool_provider`, not optional.** An optional key needs no migration and breaks nothing, and a script that declines still loads. The channels most worth diagnosing are the ones whose selection logic is custom, so an obligation that custom code may decline is not an obligation.

## Consequences

**`audit()` needs `pick()`'s working set, and that is a sandbox guarantee we did not previously make.** The two calls happen within one generation and the second is useless without the first's scoring table, so either module-level state survives between them or the script recomputes. Whichever holds, `determinism::check`'s two passes must still agree — the check runs both through `resolve_channel_with_resume`, so a script that carries state across the pair carries it across each pass identically or the check fails, which is the intended outcome.

**A report is only as good as the stage that wrote it, and the station cannot tell a lazy record from a thorough one.** `verdict` and `detail` are prose and opaque respectively. A plugin that returns `#{ stage: "pool", by: "me", verdict: "picked" }` satisfies the contract and explains nothing. This is the same trade ADR 0002 made for replay policy: the config cannot see what the script does, and nothing in the YAML distinguishes a good implementation from a bad one.

**Nothing before the change ships is diagnosable.** Chunks already on disk carry no `audit`, and `retention_days: 7` prunes elapsed ones, so the report begins working for schedule generated after deploy and there is no backfill — the inputs that produced the older schedule are gone, which is the same reason recomputation was rejected above.

## What this does not decide

Which stages the built-in machinery emits, and where in `pattern.rs` / `constrain.rs` / `resolve.rs` they are written from. The first slice covers the plugin path alone — the four channels with a `plugin:` pool — so the closed `stage` set is exercised by one producer before 60 channels' worth of production sites are instrumented against it.
