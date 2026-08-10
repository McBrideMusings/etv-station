# Context

Resolved vocabulary for this project. Terms only — decisions live in `docs/adr/`, behavior in `docs/PRD.md`, config shapes in `docs/schema.md`.

## Scorer plugin

A Rhai script a pool names instead of a CEL expression. It picks its own candidates, ranks them, and returns an ordered list of `entry_id`s. Recommendation lives entirely inside it, and so does replay by default — the station supplies inputs and never computes a taste score or a replay rule of its own, so a scorer written without suppression legitimately repeats. A pool's own `constraints: no_repeat_within` is the channel author's opt-in floor over the returned list; the two do not layer, so it is for pools whose script does *not* suppress. See ADR 0002 and #116.

## Pooled history

Recent watch activity for the whole Plex server, fetched once per generation from Tautulli and handed to the scorer plugin as a single list with no user dimension. Distinct from per-user history, which is deferred to #112.

## Recently-aired tail

The trailing run of entries this channel already scheduled, read from the play-history ledger. Two things use it: the `no_repeat_within` adjacency pass, which needs the previous generation's last item as position -1, and a scorer plugin, which uses it to avoid resurfacing what just played.

## Target count

The number of items the station asks a scorer plugin to return, derived from the generation's window duration. The plugin chooses its own corpus, so nothing else can size it, and the plugin cannot derive the window itself.

## Generation

One full pass of a channel's resolved playlist, laid end to end from where the last pass finished. Variable length (as long as the playlist's total runtime). The unit of resolution, resume, and ledger recording — not the unit of file storage. A tick chains many generations forward until the window is materialized. Distinct from a [[chunk]].

## Chunk

A fixed `chunk_hours` slice of the schedule on the local-time grid (00:00, chunk_hours, 2×chunk_hours, …), and the unit of playout-file storage: one file per chunk, holding every item scheduled in it. Distinct from a [[generation]] — many short generations fill one chunk. ErsatzTV-next consumes one chunk file at a time. See ADR 0003.

## Over-claiming file

A playout file whose filename span is wider than the items inside it cover. ErsatzTV-next picks a file by its name then an item within it by the item's span; an over-claiming file gets picked but yields no covering item, so playback falls back to black. The failure ADR 0003 exists to prevent.

## Window materialization

How far ahead a channel's schedule is written: the roll tick keeps the folder covered to `now + window_days`. Distinct from a [[chunk]] (the file-slicing unit) and a [[generation]] (one playlist pass) — materialization is the horizon, chunks and generations are how it gets filled.

## Dated block

A block declaring the calendar dates it airs on. Read once per [[generation]] when the channel's blocks are composed, so it decides whether the block takes part at all — not where in the day it lands. Among a channel's dated blocks only the first whose dates match is kept, and "first" means first in the file. Contrast a [[daypart]], which is a clock-time concept and lives somewhere else entirely. See ADR 0004.

## Undated block

A block with no calendar declaration. It always airs, which is what makes it the default a channel falls back to when no [[dated block]] matches. A channel that declares dated blocks and no undated block fails at load, because it could otherwise go dark on an unmatched day.

## Daypart

A stretch of the day with a declared character — late-night animation, prime-time films. Not a schema concept: a channel wanting dayparts is one block whose pools are its dayparts, arranged by a sequencer plugin that reads the clock. Distinct from a [[dated block]], which is calendar-conditional and resolved before any clock exists. See ADR 0004 and #169.

## Drift

The gap between when a [[daypart]] was meant to start and when it actually does, because the item before it ran past the boundary. Accepted rather than corrected: closing it would mean cutting an item short or leaving dead air, and padding it out is impossible while the library has no interstitials (#85). Bounded by the longest item in the pool.
