---
name: schedule-analysis
description: "Write one channel's programming analysis on demand: each principle in docs/programming.md graded by its rubric, depth (days until each pool and show repeats), overlap (airings shared with other channels) and what changed since a previous run. Reads the dashboard's Review JSON; the station computes none of it. Use for 'grade this channel', 'how deep is 034', 'what does it share with other channels', 'what changed since the last run'. `schedule-analysis <folder> [--days N] [--prod]`."
user_invocable: true
---

# /schedule-analysis — grade one channel's schedule against the principles

`schedule-analysis <folder> [--days N] [--prod]`. `<folder>` is the channel's
lineup key (`034-syfy`; under `examples/` the path relative to the station
file, `channels/a24.yaml`). `--days` is the projection past the published
frontier, 1, 3, 7 or 14 (default 14). `--prod` reads the deployed dashboard.

This is subjective analysis written by an agent. The station core computes none
of it: the dashboard exposes the schedule as facts (airings, items, bumpers,
first repeat, airtime split), and the judgement lives here
(CLAUDE.md, "Core exposes facts; plugins own policy"). Never propose moving a
grade, a depth table or an overlap count into Rust.

Scratch: `/private/tmp/claude/Projects-etv-station/<folder>-analysis/`.

## Step 1 — the schedule

```sh
tools/ui-get.sh '/api/channels/<folder>/review?project=14d' [--prod] > <scratch>/review-<UTC stamp>.json
```

A local dashboard is `./tools/station-ui.sh` (`programming` §"The channel
review" says which lineup it serves). A 14-day projection takes minutes;
`tools/ui-get.sh` asks again until it lands. `admin playout <folder>` opens a
trimmed copy of the same answer in a Wheel pane when a person wants to look at
it.

What the analysis reads from that JSON:

| Field | Holds |
|---|---|
| `airings[]` | `id`, `label`, `show`, `kind`, `pool`, `start` (unix seconds), `duration_secs`, `projected`, `slot` |
| `items{id}` | `season`, `episode`, `year`, `genres`, … per catalog entry |
| `bumpers[]` | `start`, `duration_secs` — the published span only; a projection runs no fill pass |
| `start`, `end`, `days`, `tz`, `span.label` | the span every number covers |
| `first_repeat`, `airtime`, `items_aired`, `distinct_items` | the dashboard's masthead facts |
| `load_error` | set instead of the rest when the channel did not load |

A `?draft=ID` answer is `{deployed, draft, diff}`; the script measures `draft`.

## Step 2 — the measures

```sh
python3 .claude/skills/schedule-analysis/measure.py <scratch>/review-<stamp>.json \
  --catalog tmp/claude/scratchpad/station-cache/catalog.db \
  --random-pools <pools whose order is random> \
  [--others <scratch>/others] [--previous <scratch>/review-<older stamp>.json]
```

It prints measurements only, in the units `docs/programming.md` grades by:

- **Depth**: per pool and per show, aired / distinct / days from the span's
  start until it first airs something a second time (`>N d` when nothing does).
- **Principle measures**: dayparting swing, episodes per visit, stripping share,
  episodes out of order, longest run, interstitial share, days to the first
  program repeat.
- **Overlap** (with `--others`): each other channel's airings that share an
  entry id with this one, largest first.
- **Since the previous run** (with `--previous`): the masthead facts and each
  pool's first repeat, old → new, changed rows starred.

`--catalog` is the local catalog copy `tools/station-ui.sh` keeps (for the
prod catalog, `tools/taste-cache.sh catalog.db` refreshes it); without it
stripping is not measured. `--random-pools` comes from `channel.yaml`: every
pool with `order: random`, whose episodes are shuffled on purpose.

For overlap, fetch every other channel's real-only review first (instant, no
projection), with a scratch script:

```sh
mkdir -p <scratch>/others
for ch in $(tools/ui-get.sh /api/lineup [--prod] | jq -r '.channels[].channel'); do
  tools/ui-get.sh "/api/channels/$ch/review" [--prod] > "<scratch>/others/${ch//\//_}.json" || true
done
```

This counts airings both schedules really hold over their spans, not the pools'
candidate sets; say which spans were compared.

For change since the previous run, keep every run's JSON under a timestamped
name and pass the older one. Two runs compare only when they cover the same
`days` from the same lineup.

## Step 3 — grade and write

For each principle in `docs/programming.md`, in the page's order:

1. Read its **Grade.** paragraph; that paragraph is the rubric, thresholds
   included. Turn the measure into 0–3 by it.
2. Where the paragraph says **Not scored** (movie slot length, themed and
   seasonal periods, marathons), or the measure says "does not apply" or "not
   measured", write **not scored** with that reason. Interstitials are
   measured over the published span only, because projected airings carry no
   bumpers; with no bumpers in the review and a `fill:` in the channel's
   blocks, write not scored rather than 0.
3. Name the airing or show behind the number (the measure prints it), and
   what in `channel.yaml` produces it.

Most thresholds are marked **unsourced** in the doc: read a grade as a
question about the channel, not a verdict on it.

The written analysis, in this order:

1. **Span**: `span.label`, start and end in UTC and station time, airings
   counted.
2. **Principles**: one row each — grade or "not scored", the measure, the
   reason.
3. **Depth**: the pools and the shallowest shows, with their first repeats.
4. **Overlap**: channels sharing airings, largest first, with titles.
5. **Since the previous run**: the starred rows, old → new, and which edit
   moved them.
