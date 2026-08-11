# A plugin gets the channel's seed, and randomness is the only thing `ctx` widens for

`ctx.seed` carries the channel's resolved seed, mixed with the pool name, so a script can make a random choice that reproduces exactly on a second generation. It is the only value added to the plugin context for this purpose, and no other capability follows from it — a script still cannot read the clock, open a file, or ask the station to compute anything.

## Why

ADR-0002 fixed the split: the plugin owns gathering and ranking, the station supplies inputs and takes back an ordered list of `entry_id`s. plex-db-ex ADR-0011 then handed one specific knob across that line — *"The exploration fraction is not a property of a taste vector at all… It describes how much of a channel is deliberately off-profile — a decision about assembling a lineup, made by whatever is building the lineup."* The lineup is built by the plugin. So the exploration fraction is the plugin's, and #254 is the first script to want it.

The trouble is what "off-profile" means mechanically. The scorer ranks candidates by cosine similarity against a pooled keyword vector. An exploration slot is, by definition, a slot filled *without* consulting that ranking — the ranking is the profile, and off-profile means not it. There is no second ordering to fall back on. A choice among candidates that have no ranking basis is a random choice; anything else is a ranking wearing a different name.

A Rhai script cannot make one. `determinism.rs:94` records why: *"`elapsed()` (Rhai's own wall clock) is the one vector it exposes, which is why 'reads wall-clock time' and 'uses unseeded randomness' are the same check."* The generate-twice-and-diff check pins an unset channel seed to one freshly-drawn value and runs both passes against it (`determinism.rs:115-117`), so a script seeding itself from `elapsed()` fails that check by construction. There is no entropy inside the sandbox that survives it, and that is deliberate.

The station, meanwhile, already holds exactly the right value. Every random choice it makes itself is seeded from the channel `seed` plus a position (`determinism.rs:7`). The seed was simply never passed down.

## What we chose, and the rejected alternatives

- **`ctx.seed`, mixed with the pool name.** Two pools of one channel — `movies` and `shows` in `examples/samples/foryou.yaml` both point at the same script — must not draw the same sequence, or their exploration slots correlate and the channel airs its surprises in lockstep. Mixing the pool name in is what keeps them independent while keeping both reproducible.

- **A deterministic band stride, rejected.** Every fifth returned slot drawn from rank 100+ needs no seed and passes the determinism check today. It also needs a band boundary and a stride, neither of which anyone can derive — and plex-db-ex ADR-0011 spent its length arguing that a constant nobody can derive is a constant nobody can tell has gone stale. Trading one unpickable number for two is not a saving.

- **The station pre-shuffles and hands the script an order, rejected.** It keeps `ctx` narrow, and it puts the station in the business of deciding which candidates are off-profile — which requires the station to know the ranking, which is the exact thing ADR-0002 gave to the plugin. #108 already deleted `Order::Score` on the neighbouring principle that a relevance score is not computable from the ids being ordered.

- **No exploration at all, rejected on request.** Ranking purely by score is defensible and ships with zero tunables. It was put forward and turned down: the fraction is wanted.

## This is not #159's signal

#159's rule — a plugin needing a core change means the hook boundary is wrong — is about a plugin needing the *core to do its job*. A seed is not work being done on the script's behalf; it is an input the station already computes, that the sandbox provably cannot obtain, and whose absence is enforced by a check the project wrote on purpose. The boundary is in the right place. The input list was one item short.

The test for the next case: if a script asks for something the station would have to *compute for it*, that is #159's signal and the hook is wrong. If it asks for a value the station already holds and the sandbox is deliberately denied, that is this ADR, and the answer is to pass it.

## Consequences

**The determinism check keeps its teeth, and gains a distinction it did not have.** Before this, "the script was nondeterministic" and "the script read the clock" were the same finding. Now a script can vary its output between channels and generations while passing the check, and a failure means what it says — the script reached for entropy the station did not give it.

**A plugin author can now write a nondeterministic-looking channel by accident.** `ctx.seed` is a number; nothing stops a script from ignoring it and deriving a choice some other way that happens to reproduce. The check catches the failing case, not the sloppy one. The plugin guide (#172) is where this belongs.

**Nothing else in `ctx` moves.** This ADR is a precedent for passing a value the station already has, not for growing the context whenever a script wants something. Each future addition argues for itself against the test above.
