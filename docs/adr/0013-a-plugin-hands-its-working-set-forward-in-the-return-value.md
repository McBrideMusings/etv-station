# A plugin hands its working set from pick() to audit() in the return value

`pick()` returns `#{ picks: [...], workspace: <opaque> }` instead of a bare array. The station holds `workspace` for the length of one generation — across two adjacent calls it makes itself — and hands it back as the third argument to `audit(ctx, picks, workspace)`. The sandbox gains no ambient state: there is no slot a script writes to and no slot it reads from, only a value that travels along the call graph.

## Why

`audit()` is called once per generation immediately after `pick()`, and everything substantive it can say — a score, a rank, a candidate count, and above all the candidates a pick was chosen over — lives in the scoring table `pick()` builds and then discards. ADR 0011 named this as its open consequence: either something survives between the two calls or the script recomputes, and whichever holds, `determinism::check`'s two passes must still agree.

Rhai does not offer the thing the question was originally phrased around. A script-defined function cannot read the calling `Scope`, and `pick()` is invoked with a fresh `Scope::new()` (`score.rs:1072`), so module-level state is not something a script can reach for and then be permitted or forbidden. The station has to build whatever carrier exists. The question is which one.

## What we chose, and the rejected alternatives

- **The return value, threaded by the station.** A value passed as an argument has no storage between generations, so two generations cannot share it and the second determinism pass cannot inherit the first's table. The divergence #391 exists to prevent is not caught, it is unrepresentable.
- **An ambient per-generation slot, the `PLUGIN_CLOCK` shape, rejected.** `stash(v)` / `stashed()` registered on the engine over a `thread_local!` cell, set and restored by an RAII guard in `resolve_channel_with_resume` beside `set_plugin_clock` (`score.rs:866`). It is the exact precedent — same file, same funnel, same guard — and it leaves `pick()`'s signature and both shipped scripts untouched, which is the honest argument for it. It is also mutable ambient state whose correctness depends on a guard firing on every path including an early `?`, and a leak between generations is silent. That is a condition guarded rather than removed, and the clock's own reason for being ambient does not apply here: a clock cannot be passed to every call site, and this working set has exactly one producer and one consumer, adjacent, both called by the station.
- **No carrier — `audit()` recomputes, rejected.** The narrowest sandbox, and provably identical output since `ctx`, `seed` and the plugin clock are all pinned per generation. It runs the unbounded half of scoring a second time per pool per generation — the half `prepare` was split away from `pick` to contain (`score.rs:943`), where a six-minute ranker held the station's only database handle. And a ranker re-entered even slightly differently records a scoring run that never aired, which is the category error ADR 0011 rejected recomputation-on-demand for, at a shorter horizon.

The envelope does not undo ADR 0011. That decision rejected an audit *key on a per-item record*, because a per-item key cannot name the losers the item beat. `workspace` is not per-item and names nothing itself; `audit()` remains the function that reports rejections.

## Consequences

**`pick()`'s return shape changes for every script, with no compatibility path.** `taste-cosine.rhai` and `endless-distance.rhai` both move to the envelope, and the contract documentation states one shape. Because #389 introduces `audit()` in the same body of work, the migration happens once rather than twice.

**`workspace` is opaque and unvalidated, the same treatment `metadata` already gets (ADR 0002).** The station moves it and never reads inside it. A script that puts nothing there gets nothing back, and `audit()` returning thin records is indistinguishable to the station from a script with nothing to say.

**A plugin can hold a whole candidate table alive across two calls.** That is the point — #393's near-miss list is the literal object `pick()` built — and it is also the memory cost, bounded only by what the script chose to keep. The bound on what reaches disk lives in the script, per #393.

## What this does not decide

Whether anything other than a `pool_provider` gets a workspace. The sequencer hook (#169) makes one call per generation and has nothing to hand forward yet.
