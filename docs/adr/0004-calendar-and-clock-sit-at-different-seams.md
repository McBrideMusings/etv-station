# Calendar conditioning filters blocks; clock conditioning is a sequencer plugin

A block can declare the calendar dates it airs on. That declaration is read at channel resolution, where it decides which blocks compose the generation. A block cannot declare a time of day; a channel that wants dayparting names a sequencer plugin (#169) and lets the script place its pools against the clock. Cascade priority for dated blocks is the order they are written in `rule.blocks` — there is no priority field and no derived rank.

## Why

#162 (date windows) and #14 (dayparting) were written as two halves of one thing. Both bodies say to design them together, and read as prose they look identical: a block that airs only sometimes, with a cascade and a mandatory default underneath it.

The pipeline says they are not the same thing. `resolve_channel` walks `config.rule.blocks` and concatenates each block's items into one flat list (`crates/etv-station/src/resolve.rs:181-224`). No wall-clock time exists at that point. Times are assigned afterwards by `Sequential`, which implements `Rule::items_covering(anchor_utc, from, to)` (`crates/etv-station/src/rule.rs:9-15`) by laying that flat list end to end. So:

- "Is today inside the Halloween window?" is answered once for a whole generation. Every deployed channel runs `window_days = 1`, so a generation is a day, and a date predicate consulted at `resolve.rs:181` is exact.
- "What airs at 20:00 on Tuesday?" cannot be answered there at all. It is a per-item question about wall-clock position, and the only stage holding a clock is `items_covering`.

One mechanism spanning both seams would have to move one of them. Pushing dayparting down means resolving each channel once per daypart slice, making composition clock-aware for all 62 channels to serve three. Pulling date windows up means teaching the emission stage about blocks, when its entire input today is a flat item list plus durations. Both pay for a resemblance that is only in the prose.

## What we chose, and the rejected alternatives

- **A date predicate on `BlockInclude`, evaluated before concatenation.** Non-matching dated blocks are dropped at `resolve.rs:181`; the rest concatenate exactly as now. Nothing downstream changes.

- **Source order is the cascade priority — not a `priority:` field, not derived from window width.** `RuleConfig.blocks` is already a `Vec<BlockInclude>` the author writes top-down (`crates/etv-station/src/config/rule.rs:18`), so a ranking already exists in the file; it was simply never consulted. Halloween beats autumn because Halloween is written first. Deriving priority from window width, which #162 proposed, leaves two equal-width windows with no defined winner — the issue names that flaw itself. An authored `priority:` integer introduces a second ordering concept beside the list order that is already there, and lets two windows claim the same rank. Making the existing order load-bearing costs no new concept and makes "why did autumn air on Halloween?" answerable by reading the block order rather than by doing window arithmetic.

- **Undated blocks are the default, and at least one must exist.** #162 requires a default that always resolves so a channel never goes dark. Rather than a distinguished `default` marker, a block with no date predicate simply always airs. A channel that declares dated blocks and no undated block fails at load. Among dated blocks, only the first match is kept; undated blocks are unconditional.

- **Dayparting is a sequencer plugin, not an `airs:` schema field.** This was the closest call. A block-level `airs: {at, days}` plus a new `Rule` impl beside `Sequential` is the better *feature* — every channel author reaches dayparting by writing config instead of a script. It is the worse *change*: it makes composition clock-aware for every channel to serve the three network mirrors, and it retires flat concatenation as the only composition model. The sequencer hook already receives the generation window bounds, so the clock is at that seam already.

  The shape this forces: a network mirror is **one block** whose pools are its dayparts (`late-night`, `prime`, `afternoon`) plus a default pool. It cannot be several blocks with one dayparted, because `resolve.rs:223` (`out.extend(block_items)`) concatenates blocks end to end and leaves no gaps — a sequencer placing its own block's items has no way to leave 03:00 open for a different block to fill. #14's original criterion, "a channel's non-dayparted blocks fill the hours no daypart claims," is unreachable across blocks and becomes "a default pool fills the unclaimed hours" inside one.

- **Daypart boundaries drift. Nothing is truncated and nothing is dark.** A sequencer prefers items that fit the time remaining before the next boundary; when nothing fits, the daypart starts late. Truncating at the boundary via `out_point` (`crates/etv-station/src/config/entry.rs:58`) would hold the grid exactly and stop a film mid-scene. Refusing to start an unfittable item would hold the grid and leave dead air, which contradicts #14's own requirement that a channel is never dark.

  What rules out the exact grid is not preference: hitting 20:00 on the second requires filler to pad with, and the library has none. #85 defers pad/fallback interstitials with the note that there are no commercials, bumpers, or interstitials to pack gaps with. A broadcast network holds its grid with promos; this station has nothing to hold it with, so the grid can only be approached. The drift is bounded by the longest item in the pool, which the pool author controls.

## Consequences

**Block order stops being cosmetic on dated channels.** Today the order of `rule.blocks` affects only the sequence items play in. On a channel with dated blocks it also decides which block airs at all, so moving a block up or down in the file changes the schedule rather than just its arrangement. Nothing in the file distinguishes a channel where order matters this way from one where it does not; the load-time failure for a missing undated block is the only guard, and it only catches the total absence of a default, not a miswritten order.

**Dayparting is unreachable from plain config.** Any channel that wants a fixed-time grid must carry a Rhai script, and the three network mirrors each carry their own. If dayparting spreads past them — a Seasonal channel wanting mornings to be kids' programming, say — "write a script" becomes the wrong answer for a common case, and the `airs:` field rejected above is what that would reopen.

**The XMLTV guide advertises the drifted times, not the intended ones.** The guide is generated from the emitted playout JSON, so a daypart that starts six minutes late is published as starting six minutes late. That is honest rather than wrong, but it means the guide cannot be used to advertise a clean schedule, and a viewer comparing two channels' listings will see ragged boundaries.

## What this does not decide

#14 is now blocked by #169 and carries no schema work. Whether the sequencer's fit-to-slot preference is written into each network mirror's script or factored into a shared helper shipped alongside the sample scorers is left to #169's implementation.
