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
