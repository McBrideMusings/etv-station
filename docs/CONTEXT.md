# Context

Resolved vocabulary for this project. Terms only — decisions live in `docs/adr/`, behavior in `docs/PRD.md`, config shapes in `docs/schema.md`.

## Scorer plugin

A Rhai script a pool names instead of a CEL expression. It picks its own candidates, ranks them, and returns an ordered list of `entry_id`s. All recommendation and replay policy lives inside it; the station supplies inputs and never computes a taste score itself. See ADR 0002.

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
