---
name: tune-channel
description: "Iterate on one channel's programming with pierce: a scratch copy of the lineup, the dashboard's 14-day review before and after each edit, the dashboard's Edit page, then a deploy on his go. Use for 'tune channel N', 'go through the channels one at a time', 'keep iterating on 032', 'review the program'. `tune-channel <folder>`. Audits for bugs are `audit-channel`; the principles are `docs/programming.md`."
user_invocable: true
---

# /tune-channel — tune one channel's programming, review it, show it

`tune-channel <folder>`. `<folder>` is the directory under
`deploy/appdata/channels/` (`032-action`). pierce goes through the channels one
at a time and decides per channel whether to keep it. A kept channel gets this
loop: tune its `channel.yaml`, review it over 14 days, look at it in the
dashboard's Edit page (`#/edit/<folder>`) and Channels inspector.

Load `programming` first and read `docs/programming.md`; they own the principles
and the comment rule. `audit-channel` owns bug-hunting on prod.

## Where things live

- Live channel: `deploy/appdata/channels/<folder>/channel.yaml`. Plugins:
  `deploy/appdata/plugins/`.
- `deploy/appdata/*` and `.wheel/` are gitignored and exist only in the main
  checkout. An edit makes no commit, so there is nothing to land per edit; run
  `/wrap-up` only when pierce asks.
- Edit page: `http://127.0.0.1:4748/#/edit/<folder>` on the scratch server
  (step 2) shows the YAML beside the preview against the deployed schedule;
  Channels shows blocks, bumpers and previews. `channel.yaml` takes no
  `comment:` key (bead etv-station-7h6h).
- Scratch: `/private/tmp/claude/Projects-etv-station/<folder>-draft/`, holding
  the `appdata/` copy, the baseline JSON and helper scripts.

## The loop

1. **Copy.** `rsync -a deploy/appdata/ <scratch>/appdata/`, so the scratch copy
   holds the live file.
2. **Serve.** In the background, with `.env` exported:
   `STATION_UI_CONFIG=<scratch>/appdata/station.yaml PATH=$PWD/target/release:$PATH ./tools/station-ui.sh --ui-port 4748`.
   Record the PID with `portman keep`.
3. **Baseline.** `ETV_UI_PORT=4748 tools/ui-get.sh '/api/channels/<folder>/review?project=14d'`
   (minutes; it asks again until the projection lands). Keep the JSON in the
   scratch folder under a timestamped name.
4. **Halt** and take pierce's change list. Change nothing until he names it.
5. **Edit** the scratch copy, re-review, and report as `programming` says:
   days until first repeat and film share, then `schedule-analysis` with the
   baseline as `--previous` for the grades, depth and overlap that moved. Check `projection.error` in the JSON — a bad profile keyword shows
   up only there, with `notices` empty.
6. **Approve → deploy.** Copy the scratch files into `deploy/`. A deploy needs
   pierce's go: `admin deploy files`, then POST
   `$PLEX_URL/livetv/dvrs/25/reloadGuide` with the `X-Plex-Token` header.
7. **Show.** Open `http://127.0.0.1:4748/#/edit/<folder>` and
   `#/channels/<folder>/blocks` in a pane, so pierce sees the result.
8. **Stop** the server with `portman stop <pid>` when done.

Audit the comment rule before calling an edit done:
`grep -n '^ \+#' <file>` must come back empty.

## Rules that decide edits

- **Taste profile keywords** must be `keyword_forms` surface spellings or the
  projection fails with "matches no stored keyword". Check one with
  `sqlite3 tmp/claude/scratchpad/station-cache/plexdb.snapshot.db "select keyword from keyword_forms where surface='<x>'"`.
- **`require: true`** entries on a pool's theme keywords stop a zero-match title
  from being drawn. Count what a require list keeps before adding one.
- **Animated films** often carry no Animation genre tag; the `anime` and
  `adult animation` keywords catch them. `animation` and `cartoon` also hit
  live-action remakes, so leave those two out.
- **An airing's slot is the block whose hours it fell in, not the block that
  drew it.** A block finishes its cycle before handing over, so judge a block by
  its pool.
- **`no_repeat_within`** counts airings the previous scheduler made, so
  `constraints.unsatisfied` warnings on a pool clear once those age out.
- **Interstitials are not graded on projected airings**: the simulator never
  runs the align/fill pass, so `schedule-analysis` measures them over the
  published span only.
- A holiday slot that must not change stays byte-identical; diff it after every
  edit.

## Decisions already made

- The dashboard's Blocks, Bumpers and Preview views and Edit page replaced the
  workbench (etv-station-a1h9, etv-station-n0xi); only the dashboard server reads
  Radarr and Sonarr.
- A block's logo beats the schedule's logo.
- Channels 030 and 031 merge into one Comedy channel at 030 (etv-station-50kz).

## Keeping this skill current

When a tuning session teaches a rule that the next channel needs (a keyword
trap, a review number that misleads, a deploy step), add it under "Rules that
decide edits" in the same turn. State what is true now, not how it was found.
