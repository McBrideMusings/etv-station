# The reason set and the audit trail are separate fields

A pick's viewer-facing justification and its diagnostic justification are recorded twice, in two sibling keys of the same `metadata` blob: `reason_set` for the why line an overlay renders on screen (#318), `audit` for the stage list a report reads (ADR 0011). One computation in the scorer, two writes, two consumers that never constrain each other.

## Why

Both are per-pick justifications, both are recorded at pick time by the same script, and both ride `PlayoutItem::metadata` to the same chunk file. Folding them into one structure is the obvious move, and it is the one that would be wrong.

They differ in what happens when they are absent. A missing `audit` entry costs a line in a text file nobody is reading at the time. A missing `reason_set` empties the NOW card's second row on a channel whose entire premise is that it chose *for you* — a visible on-air defect, in front of viewers, with no way to notice it until someone is watching. Those are not the same availability requirement, and a shared field would hold both to the weaker one.

They also change at different rates. The audit trail is a diagnostic surface that should be free to grow stages, thicken `detail`, and be reshaped when a report turns out to answer the wrong question. The why line is broadcast graphics: #318 fixed its treatment against three rejected alternatives after a prototype comparison, and it renders on the same stagger beat as the row it shares. Coupling them makes every audit schema change a change to what goes out on air.

## What we chose, and the rejected alternatives

- **Two sibling keys, written independently.** The scorer computes its reasons once and writes both shapes from that one computation.
- **The reason set as a stage inside the audit trail, rejected.** It records the fact once, which is the honest argument for it, and it means the two can never disagree. It also puts a broadcast-graphics dependency on a diagnostic schema, and makes the overlay script walk an array to find the one stage it cares about — per frame, in a script that holds no memory between frames.
- **Building the audit trail first and re-slicing #318 as a consumer of it, rejected.** Only one thing would ever get built, and #318's tracer bullet would prove the audit trail end to end on real hardware. But #318 is specced, sliced and prototyped, and it deliberately chose to be a tracer bullet; putting it behind an architecture that does not exist yet reverses that choice to buy an elegance neither feature needs.

## Consequences

**One fact, two writers, so they can drift.** The audit trail can say `score 0.82` while the reason set is built from a weight that moved, and nothing in the system compares them. This is a real cost accepted deliberately: the alternative couples an on-air surface to a diagnostic one, and a drifted diagnostic is a wrong text file while a coupled one is a dark row on screen.

**`why` is a reserved word in this project's vocabulary and means the on-screen sentence.** `docs/CONTEXT.md` defines *why line*, *reason*, and *reason set* around the viewer-facing form. The audit trail's per-stage field is `verdict` for exactly this reason, and a future audit key named `why` would collide with a term that already means something else.

## What this does not decide

Whether the same split applies to a second consumer, should one appear. Two keys is a decision about these two, not a general rule that every consumer of pick-time justification gets its own field.
