---
name: programming
description: "Load before editing any channel.yaml under deploy/appdata/channels/ or examples/channels/ (pools, blocks, pattern, schedule, sequencer, dayparts, rest periods), and before reviewing or judging a channel's schedule. Points at docs/programming.md for the principles and runs the taste-lab Review view before and after the edit."
user_invocable: true
---

# /programming — edit or review a channel's schedule

The principles live in one place: `docs/programming.md`. This skill holds none
of them. It names the steps around a schedule edit.

## Before the edit

1. **Read `docs/programming.md`** in full before changing what a channel airs or
   when. Name the principle each change serves when you report it.
2. **Run the channel's Review view and keep its numbers** (below). That run is
   the "before".
3. **Read the comment-style rule** for `channel.yaml` in `CLAUDE.local.md`
   (§"Comment style in `deploy/appdata/channels/*/channel.yaml`") and follow it.
   Its audit command must come back empty before you call the edit done.

## Where to edit

`deploy/appdata/*` is gitignored. It exists only in the main checkout, never in
a worktree under `~/.worktrees/`, and an edit there
produces no commit. Edit those files in the main checkout, by hand, in this
session — never hand that work to a worktree agent.

## The Review view

taste-lab serves it. Start it from the main checkout:

```sh
./tools/taste-lab.sh                    # always a release build; prints http://127.0.0.1:4747/
```

`--station <file>` picks the lineup it steps through (default
`deploy/appdata/station.yaml`); point it at `examples/station.yaml` for a channel
under `examples/channels/`.

Read the numbers from the JSON, not a screenshot:

```sh
curl -s 'http://127.0.0.1:4747/api/review?channel=<folder>&days=14'
```

`<folder>` is the channel's key in the lineup. Under `deploy/appdata/channels/`
it is the directory name, e.g. `034-syfy`. Under `examples/`, where channels are
flat files sharing one folder, it is the path relative to the station file, e.g.
`channels/a24.yaml` — pass it as is, unencoded. `days` defaults to 14, max 60.
Measured 2026-10-12 on a catalog at schema v9 (the `item.show` index): 14 days
takes ~5s for SyFy and ~14s for 035-prestige, an entries-only channel that
resolves 27 queries on each of its 331 hourly generations. Without the index,
that 035 request takes over 9 minutes. taste-lab reads `taste-cache.sh`'s copy of the
host's catalog, so it has the index only once a deployed daemon has migrated the
host's file and the cache has refetched it. taste-lab re-reads the station and
channel files on every request, so an edit needs no restart.

## After the edit

Request the same channel and `days` again **from the same taste-lab process**.
The response's `since_last` array then holds one `{label, old, new, changed}`
row per number. Report these three in chat, before → after:

- `Days until first repeat` (`null` means nothing repeated inside `days`)
- `Film share of airtime (%)`
- `Channels sharing content` (overlap)

Add any other row whose `changed` is `true`. The numbers are questions plus
data, never a pass/fail verdict: say what moved and which principle in
`docs/programming.md` the move answers to. A restarted taste-lab has no "before"
— `since_last` is `null` on its first request for a channel — so keep the
process running across the edit.
