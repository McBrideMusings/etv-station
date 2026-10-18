---
name: programming
description: "Load before editing any channel.yaml under deploy/appdata/channels/ or examples/channels/ (pools, blocks, pattern, sequencer, slots, schedules, dayparts, seasonal runs, rest periods), and before reviewing or judging a channel's schedule. Points at docs/programming.md for the principles and runs the web UI's Review view before and after the edit."
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

The station's web UI (`etv-station --serve-ui`) serves it. Start it from the main checkout:

```sh
./tools/station-ui.sh                   # always a release build; serves http://127.0.0.1:4747/
```

`STATION_UI_CONFIG=<file>` picks the lineup it steps through (default
`deploy/appdata/station.yaml`); point it at `examples/station.yaml` for a channel
under `examples/channels/`. The deployed container serves the same pages on port 4747,
against prod's own catalog and deployed plugins; it cannot see an edit you have not deployed.

Read the numbers from the JSON, not a screenshot:

```sh
curl -s 'http://127.0.0.1:4747/api/review?channel=<folder>&days=14'
```

`<folder>` is the channel's key in the lineup. Under `deploy/appdata/channels/`
it is the directory name, e.g. `034-syfy`. Under `examples/`, where channels are
flat files sharing one folder, it is the path relative to the station file, e.g.
`channels/a24.yaml` — pass it as is, unencoded. `days` defaults to 14, max 60.
Measured 2026-10-18 on a release build: 14 days of 034-syfy (185 generations, 377
airings) takes ~166s, warm or cold, and the server answers nothing else meanwhile —
start it before you need it. On 2026-10-12, at schema v9 (the `item.show` index),
035-prestige took ~14s; it is an entries-only channel that
resolves 27 queries on each of its 331 hourly generations. Without the index,
that 035 request takes over 9 minutes. The local UI reads `taste-cache.sh`'s copy of the
host's catalog, so it has the index only once a deployed daemon has migrated the
host's file and the cache has refetched it. The UI re-reads the station and
channel files on every request, so an edit needs no restart.

## After the edit

Request the same channel and `days` again **from the same web UI process**.
The response's `since_last` array then holds one `{label, old, new, changed}`
row per number. Report these three in chat, before → after:

- `Days until first repeat` (`null` means nothing repeated inside `days`)
- `Film share of airtime (%)`
- `Channels sharing content` (overlap)

Add any other row whose `changed` is `true`, and the `principles` array's grades
before → after. A grade is a 0–3 reading of one measured number against the
thresholds under that principle's **Grade.** in `docs/programming.md`, most of
them unsourced: say what moved and which principle the move answers to, and
read a grade as a question about the channel, not a verdict on it. A restarted web UI has no "before"
— `since_last` is `null` on its first request for a channel — so keep the
process running across the edit.
