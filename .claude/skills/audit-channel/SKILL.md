---
name: audit-channel
description: "Audit one channel end to end: schedule bugs, playback bugs per media element (programmes, bumpers, overlay graphics, logo, subtitles, audio) and a programming review. Use for 'audit channel N', 'check my channel for bugs', 'what's wrong with 034-syfy', 'review this channel'. `audit-channel <folder> [schedule|playback|review]`; no selector runs all three."
user_invocable: true
---

# /audit-channel — find what is wrong with one channel

`audit-channel <folder> [schedule|playback|review]`. `<folder>` is the
directory under `deploy/appdata/channels/` (`002-for-pierce`). With no selector,
run all three passes, starting the review's simulation first because it is the
slow one.

Every finding is a **bug report**: what the channel airs, what it should air,
the evidence (a real value read from prod), the cause (a config key or a
`path:line`), and the cost to a viewer in minutes or airings. A suspicion with
no evidence is a question for the report's last section, not a finding.

Everything here reads prod; nothing writes it. Fixes are proposed at the end
and wait for pierce's `go` — a `deploy/appdata/` edit needs a redeploy to take
effect, and a code bug becomes a bead.

## Step 0 — what is prod running

```sh
admin image-age                 # prod's commit against main
```

The review pass simulates **this checkout's** code against prod's catalog,
while the schedule and playback passes read what prod's **deployed** code
wrote. When the two differ, say so beside any finding the gap could explain.

## Step 1 — the facts (schedule and playback both start here)

```sh
python3 tools/channel-audit.py <folder> --json > <scratch>/audit-<folder>.json
```

About 15s. It reads the not-yet-finished playout chunks, `ffprobe`s every file
they name inside the container, the container log, `history.db`, the lineup,
the guide, `/health/channels.json` and the overlay's files, and lists every
mismatch as `{severity, element, what, …}`. Exit 1 means an `error` finding
exists. Its module docstring says what each check reads. `--lang` sets the
audio language a viewer expects (default `eng`).

The script finds symptoms. Each pass below explains them.

## Step 2 — schedule pass

1. **Why each flagged item is there.** `tools/audit-report.sh <folder> --next 40
   --format json` (what `admin audit` runs) gives every upcoming item's audit
   trail: pool, `rank`, `score`, `recency_factor`, `near_misses`, `guide`
   line. A repeat with `recency_factor 1.0` was ranked as if its earlier
   airing did not exist.
2. **The generation's own warnings.** `constraints.unsatisfied` names the pool
   and its violation count: the no-repeat rule failed and the station aired the
   nearest arrangement. Tie it to the config line (`advance: restart`,
   `no_repeat_within`, the pool's size) that makes the rule unmeetable.
3. **Error cards.** For each, read the card's reason (`file not found`, …),
   then look at the directory inside the container
   (`ssh … "docker exec etv-station ls -la '<dir>'"`) and at the catalog row
   (`entries.missing_since`, `entry_sources.playback_path / last_seen /
   missing_since` in `/mnt/user/appdata/etv-station/data/catalog.db`). Cost it:
   airings × minutes from `history.db`.
4. **Blocks and slots.** For a channel with `schedule:`/`slots:`/dayparts,
   compare each block's first item start with its slot's hour in the chunk
   items; record median and worst lateness.

Done when every `schedule`, `log` and error-card finding has a cause line.

## Step 3 — playback pass, one media element at a time

| Element | Check | How |
|---|---|---|
| Programmes | file exists, has video, length matches slot | `channel-audit.py` `programme` findings; error cards per Step 2.3 |
| Audio | the selected stream is in the viewer's language when the file has one | `audio` findings; the station-wide `tracks:` in `deploy/appdata/station.yaml` and the channel's own `tracks:` decide the pick (`crates/etv-station/src/tracks.rs`) |
| Subtitles | a foreign-language item carries subtitles, or the reason it cannot | `subtitles` findings. Station-wide `tracks.subtitle.inherit: false` turns them off on every channel because iPlayTV freezes on a subtitle track (etv-station-j4dw); report the foreign titles the channel draws as the consequence |
| Overlay graphics | every card renders, reads, and fits; assets exist | Render it (below) and look at every image |
| Logo | on-screen bug present and legible; guide logo sent | the stills; `logo` findings (`tvg-logo`, XMLTV `<icon>`; set by the channel's `logo:`) |
| Bumpers | each `fill:` script fills a gap without error | a block with `fill:` only: `./target/debug/etv-station bumper-preview --gap 45s <script>` per script; `POST /api/channels/<ch>/graphics/render {"kind":"bumper","script":…,"gap":"45s"}` (or Channels › Graphics) renders one to an mp4 |
| Stream | not failing, not stalling, encoding on the GPU | `health` fact; `tools/attribute-stalls.sh --channel <N>`; `admin verify accel` |

**Render the overlay** with an item that really airs:

```sh
./target/debug/etv-station --config deploy/appdata/station.yaml --dump-overlay <channel name> > <scratch>/ov.yaml
ffmpeg -v error -y -ss 600 -i "<local copy of the file>" -frames:v 1 -vf scale=1280:720 <scratch>/bg.png
./target/debug/etv-overlay render-still --config <scratch>/ov.yaml --output <scratch>/ov-t<T>.png --time <T> \
  --title … --sub-title … --season … --episode … --year … --content-rating … --next-title …
magick <scratch>/bg.png <scratch>/ov-t<T>.png -composite <scratch>/comp-t<T>.png
```

`T=2` shows the NOW card, `T=7` NEXT, `T=60` the idle frame. Render a film and
an episode; add a block card's moment when a pool has a `label`. Read every
composite and write down each difference from what the card should show. NEXT
leaving out the episode number is `now-next-snipe.rhai`'s `format_card(…, false)`
on purpose.

**A live frame needs pierce's yes.** Requesting `/channel/<N>.m3u8` starts a
transcode on prod. When he agrees, `stream-triage` owns reading the stream.

Done when each of the seven rows has a result: findings, "clean", or "not
applicable" with the reason.

## Step 4 — review pass

Run the channel review as `.claude/skills/programming/SKILL.md` §"The channel review"
says, 14 days, and keep the server running — the before/after of any fix you
propose comes from the same process. Grade the channel against
`docs/programming.md`, naming the principle behind each critique, and report
the three numbers that skill names plus every `principles` grade below 2.

If every channel answers `no such column`, prod's catalog is at an older schema
than this checkout (etv-station-jrkt). Say the review pass is blocked and why,
or apply the missing migration to the local copy and start the binary without
`tools/station-ui.sh`, which would refetch it.

Read the channel's taste and draw against what the audit trail shows — a
`taste_score` of 0 on a pick means the profile tilt alone placed it. Compare
with the sibling channels that share its pools or plugin.

## Step 5 — the report

In chat, in this order:

1. **Headline**: the finding that costs a viewer the most, as a number
   (minutes of error card per day, airings in the wrong language).
2. **Findings by element**, each as a bug report (see the top of this file).
3. **Programming review**: grades, the three numbers, critiques with their
   principle.
4. **Fixes**, as numbered options with a pick in the CLAUDE.md template — a
   `deploy/appdata/` edit (the `programming` skill owns how), a bead for a
   code bug, or a question for pierce.
5. **Not checked**, each with the command that did not run.

Images the report relies on go in by absolute path.
