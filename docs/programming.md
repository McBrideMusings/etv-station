# Programming principles

This page lists the TV-programming principles a channel review judges each
channel against. Each principle has four parts:

- what it means, with the source it comes from;
- one real network that does it;
- how a `channel.yaml` says it today, with the key and a short example;
- where today's schema cannot say it, the bead that tracks the gap;
- how the Review view grades it, when a schedule yields a number for it.

A number marked **unsourced** or **inference** is reasoning written for this
page. No source states it. Treat it as a starting question for a review, not
as a rule.

The Review view grades a principle 0 to 3 only when the simulated schedule
measures it; the channel's total counts those principles alone. A principle
with no number to measure, or one that does not apply to the channel, is
listed with the reason and left out of the total. Every grade threshold below
is **unsourced** unless it cites a source: it is the line this page draws, not
an industry standard. `crates/etv-station/src/review.rs` computes them.

The keys named below are documented in the [config schema](/schema).
Every YAML example is a fragment. `pools:`, `pattern:` and `sequencer:` sit
inside one entry of `rule.blocks` or one `rule.slots` entry, and `slots:` and
`schedules:` sit under `rule:`, as the first example shows in full.

"Depth" on this page has the meaning in the [glossary](/CONTEXT#depth): how
long a channel runs before something airs again. An episode count is called a
library size.

## Dayparting

**What it is.** A network splits the day into parts and airs programs suited
to whoever is watching at that hour
([Wikipedia, Broadcast programming](https://en.wikipedia.org/wiki/Broadcast_programming)).

**Network example.** Daytime shows aim at the audience that is home during the
day; prime time aims at the whole household (same source).

**In `channel.yaml` today.** Each daypart is a slot, and a schedule places it
on its hours. `rule.blocks` fills every hour no schedule claims. Hours are local
to the station's `tz`.

```yaml
rule:
  blocks:
    - pools: [{ name: films, expr: 'item.type == "movie"' }]
      pattern: [{ pool: films, take: 1 }]
  slots:
    latenight:
      pools:
        - name: latenight
          expr: 'item.show in ["Rick and Morty", "The Boondocks"]'
      pattern: [{ pool: latenight, take: 1 }]
  schedules:
    - name: nightly
      hours: { "21..06": latenight }
```

A daypart boundary lands late by the overrun of whatever is airing across it,
the item or the rest of its pattern cycle, rather than cutting it
([schema, Schedules](/schema#schedules-—-rule-slots-and-rule-schedules)). A slot
continues where it stopped the night before: its pool rotation and list
position are kept while other blocks air. The sample is
`examples/samples/adult-swim.yaml`.

**Grade.** Applies to a channel with two or more pools. The measure is the
largest swing in any one pool's share of airtime between the day's four
6-hour quarters (midnight, 6 AM, noon, 6 PM, local time); an airing's time is split across the quarters it covers. 30 points or more
is 3, 20 is 2, 10 is 1, under 10 is 0 (**unsourced**).

## Block programming and episodes per block

**What it is.** Complementary programs air together under one name, grouped by
genre or audience
([Military Gogglebox](https://militarygogglebox.com/2020/08/18/what-is-broadcast-programming/)).
A block airs a short run of one show before moving on.

**Network example.** ABC's "TGIF" and NBC's "Must See TV" (same source). MeTV's
Saturday runs cartoons in the morning, westerns in the afternoon, the Three
Stooges early evening, Svengoolie in prime time and sci-fi late
([Wikipedia, MeTV](https://en.wikipedia.org/wiki/MeTV), via a search summary).
Cozi TV aired episodes 33 and 34 of The Munsters back to back on one listed
date ([TV Insider, Cozi TV](https://www.tvinsider.com/network/cozi-tv/schedule/),
one data point).

**How many episodes per block.** 2 to 4 is **unsourced**. The Cozi pairing is
one observed example of 2; nothing found states a norm.

**In `channel.yaml` today.** A pattern block. `take` is how many items one
visit airs; `rotate: visit` keeps those items on one show; `advance: resume`
makes the next visit continue where the last stopped.

```yaml
pools:
  - name: shows
    expr: 'item.show in ["The Munsters", "The Addams Family"]'
    order: "season:asc,episode:asc"
    rotate: visit
    advance: resume
pattern:
  - pool: shows
    take: 3
```

A step with `chance:` below `1.0` fires only some of the time. A second step
on the same pool with `chance: 0.5` does not lengthen the visit, though: under
`rotate: visit` each step is a new visit, so it adds three episodes of the
next show in the rotation half the time.

**A random range.** `take` also accepts a range, so the visit length varies
the way real blocks do:

```yaml
pattern:
  - pool: shows
    take: "2-4"        # or { min: 2, max: 4 }
```

Each visit is asked for 2, 3 or 4 episodes of one show, every size equally
likely. The size is rolled per visit from the channel `seed`, so a pinned seed
asks for the same sequence of sizes every time. A series that runs short follows
`on_short`, as it does for a fixed count. Unlike a second step with `chance`,
the visit always happens and the next show in the rotation is not pulled in;
only the length changes. Field rules are in the
[schema](./schema.md#pattern-step-take-—-how-many-items-one-visit-draws).

**Grade.** Applies when a series airs. The measure is the mean number of
episodes of one show per visit, ignoring interstitials between them. 2 to 4
is 3 (the unsourced range above), over 4 up to 6 is 2, over 6 or 1.5 to 2 is
1, under 1.5 is 0.

## Stripping, and how deep a show must be to strip

**What it is.** A series airs in the same slot every weekday
([Wikipedia, Strip programming](https://en.wikipedia.org/wiki/Strip_programming)).

**Network example.** Syndicated sitcom reruns at a fixed hour Monday to Friday
(same source).

**Library size needed.** The rule of thumb is 100 episodes, which gives 20 weeks of
weekdays before a repeat
([Slate](https://slate.com/news-and-politics/2009/12/why-syndicated-tv-shows-are-programmed-out-of-sequence.html);
[Wikipedia, Strip programming](https://en.wikipedia.org/wiki/Strip_programming)).
Three seasons, about 75 episodes, gives 15 weeks (Strip programming). The
threshold had fallen to about 88 episodes by the early 2010s, and 44 episodes
stripped daily repeats in under 9 weeks
([Wikipedia, 100 episodes](https://en.wikipedia.org/wiki/100_episodes), via a
search summary). A former Turner executive is quoted as wanting at least 66
episodes and about 3 months before a repeat; the source URL
([NPR transcript](https://www.npr.org/transcripts/129861344)) is **unconfirmed**.

**A 24/7 single-show channel uses a show up about 30 times faster**
(**inference**, and the runtimes below are assumed, not sourced). Counted per
airing day: a strip airs one episode a weekday, and a 24/7 channel of
hour-long episodes (about 44 minutes without ads) airs about 32 a day. Counted
in calendar time the gap is wider: 100 episodes last about 3 days on the
single-show channel against 20 weeks (140 days) stripped, about 45 times
faster. Half-hour sitcoms (about 22 minutes) go about 65 a day, so the same
100 episodes last about a day and a half.

**In `channel.yaml` today.** A slot placed by a weekday schedule. The slot
fills its range: an hour-long drama airs about one episode a weekday on
`"18..19"`, and a half-hour sitcom on `"18:00..18:30"` airs one.

```yaml
slots:
  strip:
    pools:
      - name: strip
        expr: 'item.show == "The X-Files"'
        order: "season:asc,episode:asc"
        advance: resume
    pattern: [{ pool: strip, take: 1 }]
schedules:
  - name: weeknights
    weekdays: [mon, tue, wed, thu, fri]
    hours: { "18..19": strip }
```

`advance: resume` carries the strip on from the last episode aired, across
generations and across the hours other blocks fill.

**Across channels.** By default a show that strips on two channels keeps a
separate place on each. Add `cursor_scope: global` to the pool and it continues
from the latest episode of that show aired on any channel; the second channel to
air it carries on rather than replaying. Only shows share a place; a movie pool
has no show to key on and restarts. `advance: restart` still restarts —
the scope picks which cursor a resuming pool reads, not whether it reads one.

**Grade.** Applies when a show that aired has 75 or more episodes in the
catalog (three seasons, per Strip programming above). The measure is the
share of the span's weekdays on which that show starts an episode within 30 minutes of its
usual weekday start time, taking the best such show. 80% or more is 3, 60% is
2, 40% is 1, under 40% is 0 (**unsourced**).

## Episode order

**What it is.** Stations air self-contained shows out of order. Order is more
likely kept during the first strip cycle, while the exclusivity window is
open; after that, episodes become "more like movable units on a master
schedule"
([Slate](https://slate.com/news-and-politics/2009/12/why-syndicated-tv-shows-are-programmed-out-of-sequence.html)).
Popular and holiday episodes air more often, and a station may loop a
"subcycle" built from part of the series (same source). Narrative shows need
care when aired out of order; format and lifestyle shows do not
([US patent 11638054](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/11638054)).

**Network example.** TBS aired The Office season 4, then a season 2 marathon,
then the season 5 premiere, in one week (Slate).

**In `channel.yaml` today.** In order, continuing across days:

```yaml
order: "season:asc,episode:asc"
advance: resume
```

Shuffled from the first airing: `order: "random"`. A seeded run of
consecutive episodes from a random starting point: `from: random` on the
pattern step. A subcycle: a narrower `expr` on the pool
(`item.show == "The Office" && item.season <= 2`). `filter: { seasons: [1, 2] }`
does the same on an entries block only; a pattern or sequencer block rejects
it at load.

**Review question.** Is each show on the channel self-contained (a sitcom, a
procedural, a format show), which can air in any order, or narrative (a
serialized drama), which needs broadcast order?

**Grade.** Applies when a show airs twice from a pool whose `order` is not
`random`; a random pool shuffles on purpose. The measure is the share of
steps between consecutive airings of one show that do not go to the next
episode, to episode 1 of the next season, or back to the show's first aired
episode (episode 1) after its last, as a looping channel does. 0% is 3, up to 5% is 2, up to
15% is 1, more is 0 (**unsourced**).

## Movie slot length and cheap repeats

**What it is.** A film's slot is longer than the film. A film of about 100
minutes needs about 150 minutes with ads
([Wikipedia, TV advertisements by country](https://en.wikipedia.org/wiki/TV_advertisements_by_country),
via a search summary). The ABC Sunday Night Movie ran 120 to 180 minutes with
ads ([Wikipedia](https://en.wikipedia.org/wiki/The_ABC_Sunday_Night_Movie)).
Cable licenses cover dozens or hundreds of films for years, so a repeat airing
costs "almost nothing"
([Sparetimetv](https://www.sparetimetv.com/why-cable-channels-air-the-same-movie-over-and-over/)).

**Network example.** TNT listed Kong: Skull Island at 4:15 PM, Jurassic World:
Fallen Kingdom at 6:45 PM and Twister at 9:00 PM: 150 minutes between starts,
then 135
([TV Insider, TNT](https://www.tvinsider.com/network/tnt/schedule/), via a
search summary). TBS aired Angels in the Outfield at midnight and again at
4:00 AM the same night
([Screen Rant, TBS](https://screenrant.com/db/schedule/tbs/), via a search
summary).

**Unsourced.** How often a cable film repeats per week or month, and how long
a movie package window runs. No source found gives either.

**In `channel.yaml` today.** Films play back to back at their own length; the
station has no ads to fill the extra time. Repeat spacing is a temporal
`no_repeat_within`:

```yaml
pools:
  - name: films
    expr: 'item.type == "movie" && item.genres.contains("Action")'
    order: "random"
    constraints:
      no_repeat_within: "7d"
```

**Not expressible today.** A film cannot be padded out to a clock-aligned
slot (for example, a 100-minute film filling 2:00 to 4:30 with filler after
it). Tracked in `etv-station-85` (pad/fallback filler). How much of a mixed
channel's airtime goes to films versus series cannot be stated directly
either; it falls out of `take` counts and runtimes. Tracked in
`etv-station-wi81`.

**Grade.** Not scored: padding a film to its slot is the principle, and the
station cannot express it yet.

## Themed and seasonal periods

**What it is.** A network gives part or all of its schedule to a theme for a
set period
([Wikipedia, Broadcast programming](https://en.wikipedia.org/wiki/Broadcast_programming)).
The grid also follows the TV year: reruns from mid-December to mid-January or
the February sweeps
([Wikipedia, Rerun](https://en.wikipedia.org/wiki/Rerun)).

**Network examples.**

- TCM's 31 Days of Oscar ran February 13 to March 15 in 2026, with a theme per
  day ([TCM](https://www.tcm.com/articles/021882/31-days-of-oscar-2026-part-ii)).
- Hallmark's Countdown to Christmas 2026 runs October 16 to December 20, with
  34 premieres, mostly Friday to Sunday evenings
  ([Variety](https://variety.com/lists/hallmark-countdown-to-christmas-2026-schedule-new-movies/)).
- Freeform's 25 Days of Christmas runs December 1 to 25, and a 2017 article
  says it reuses about 50 films each year
  ([Arts at Michigan](https://artsatmichigan.umich.edu/ink/2017/11/28/freeform-25-days-of-christmas)).
- TCM's Star of the Month moves between weekdays from month to month
  ([TCM October 2026 schedule PDF](https://static.tcm.com/2026-09/NPN_OCT26_PrimetimeSchedule_Final2.pdf)).

**In `channel.yaml` today.** A slot holding the theme, placed by a schedule
with `dates:`. A yearly span repeats every year and may cross December 31st; a
span with years is a one-off.

```yaml
slots:
  christmas:
    pools:
      - name: christmas
        expr: 'item.collections.contains("Hallmark Christmas")'
    pattern: [{ pool: christmas, take: 1 }]
schedules:
  - name: countdown
    dates: ["12-01..12-25"]
    hours: { "00..24": christmas }
```

A dated schedule outranks a weekday one, so the season replaces the regular
weekly grid without reordering anything. A theme for part of the day keeps the
rest: a schedule that claims only `"18..24"` leaves the daytime to the weekly
schedules and the any-time rules. A Star of the Month night is a schedule with
both `dates:` (the month) and `weekdays:` (the night). The sample is
`examples/samples/seasonal.yaml`.

**Grade.** Not scored: a seasonal run is a choice a channel makes, not a standard
it meets.

## Marathons

**What it is.** A continuous run devoted to one program or franchise
([Wikipedia, Broadcast programming](https://en.wikipedia.org/wiki/Broadcast_programming)).

**Network example.** Syfy's New Year's Day Twilight Zone marathon (same
source). Boomerang ran a 96-hour Thanksgiving marathon in 2020
([Wikipedia, Boomerang](https://en.wikipedia.org/wiki/Boomerang_(TV_network))).

**In `channel.yaml` today.** A marathon that happens sometimes, at an
unplanned moment, is a pattern step with `take: all` and a low `chance`:

```yaml
pattern:
  - pool: shows
    take: 2
  - pool: shows
    take: all
    chance: 0.05
```

`take: all` with `group_by: season` airs a whole season in one visit.

A marathon on a date is a slot placed on that date. A date that moves each
year, such as Thanksgiving, lists each year's span:

```yaml
schedules:
  - name: new-year
    dates: ["01-01"]
    hours: { "00..24": twilight }
  - name: thanksgiving
    dates: ["2026-11-26..2026-11-29", "2027-11-25..2027-11-28"]
    hours: { "00..24": marathon }
```

**Not expressible today.** A rule such as "the fourth Thursday of November";
each year's dates are written out.

**Grade.** Not scored: a marathon is a choice a channel makes, not a standard
it meets. The Review view shows the longest run of one show.

## Interstitials and bumpers

**What it is.** Promos run 15 to 60 seconds, 30 seconds most often, within a
range of 5 to 90
([Wikipedia, Promo (media)](https://en.wikipedia.org/wiki/Promo_(media))). A
bumper is a short branded segment between programs, usually "coming up next";
no source gives a standard bumper length (same source). The network bug sits
almost always in a screen corner
([Wikipedia, Digital on-screen graphic](https://en.wikipedia.org/wiki/Digital_on-screen_graphic)).
A lower third promoting what comes next sits in the title-safe lower area and
leaves after a few seconds
([Wikipedia, Lower third](https://en.wikipedia.org/wiki/Lower_third)).
Playout systems fill schedule gaps with bumpers rather than black
([5centsCDN](https://www.5centscdn.net/blog/fast-channel-infrastructure/), via
a search summary).

**Network example.** A premium channel with no ads airs promos in a break
after each program, and the break's length varies with the next start time
(Promo (media)).

**In `channel.yaml` today.**

- **Bug:** an image layer in the channel's overlay, appended to the station's
  shared graphics with `extend`.

  ```yaml
  overlay:
    extend:
      graphics:
        - layers:
            - { type: image, path: logo.png, corner: bottom_right, height: 56 }
  ```

- **Lower third / snipe timing:** the station's Now/Next snipe
  (`deploy/appdata/shared/now-next-snipe.rhai`) reads its timing from the
  overlay's `config:`. `interval_secs` (default 300) sets how often it runs and
  `hold_secs` (default 4.5) how long each card stays.
- **Naming a themed block on screen:** `label:` (and an optional `advisory:`)
  on a pool. Every item the pool draws carries them, and the station's block
  card (`deploy/appdata/shared/block-card.rhai`) shows them when the block
  changes and on an interval. See `docs/schema.md` "Pool `label` / `advisory`".
- **Bumpers between programs:** a pool of short files and a pattern step that
  draws one between programs.

  ```yaml
  pools:
    - name: bumpers
      expr: 'item.fs_dir == "bumpers"'
      order: "random"
  pattern:
    - { pool: shows, take: 3 }
    - { pool: bumpers, take: 1 }
  ```

**Not expressible today.** Filling the gap to the next clock boundary with
bumpers, rather than airing a fixed count of them, is pad/fallback filler.
Tracked in `etv-station-85`.

**Grade.** Applies once two programs have aired. An airing under 5 minutes
counts as an interstitial. The measure is the share of changes from one program to the
next that have at least one interstitial between them. 90% or more is 3, 50%
is 2, anything above 0 is 1, none is 0 (**unsourced**). The bug is not
graded: a schedule does not show it.

## Rest periods and freshness

**What it is.** A title rests before it airs again. Repeats shown 40 or 100
days after a premiere draw much smaller audiences
([US patent 9918142](https://image-ppubs.uspto.gov/dirsearch-public/print/9918142),
via a search hit). One slot can fill a year from 26 episodes if each airs at
most twice ([Wikipedia, Rerun](https://en.wikipedia.org/wiki/Rerun)). FAST
guidance is to refresh a channel weekly and rotate monthly
([FAST Channels](https://www.fastchannels.tv/news/fast-faqs-managing-a-fast-channel),
via a search summary).

**Unsourced.** A standard rest period between repeats of the same title. A
floor of 60 to 90 days for films is a guess. Wear-out data found applies to
ads, not programs: ads repeated less than 5 minutes apart were 15% less likely
to keep viewers in the room
([TVision](https://www.tvisioninsights.com/resources/repeat-ctv-ad-length)).

**Rest period from pool size** (**inference**). Days before a title repeats is
roughly the pool's total runtime divided by the hours a day the pool airs. A
pool of 80 films averaging 110 minutes is about 147 hours; airing 12 hours a
day, it repeats after about 12 days. A simulation measures this directly,
because rotation, `advance: resume` and scorer suppression all change it.

**In `channel.yaml` today.**

- `constraints: { no_repeat_within: "14d" }` on a pool: no title twice in 14
  days of airtime. A bare number (`no_repeat_within: 10`) counts that pool's
  own draws instead of time.
- On a taste-scored channel, `scoring: { recent_depth: 200 }` sets how many
  recently aired titles the scorer holds back.
- A pool whose `expr` reads a Plex collection (`item.collections.contains(...)`)
  changes with the collection, which is the station's form of a monthly
  rotation.
- On an `advance: resume` pool, a multi-episode show played through to its
  last episode sits out the rest of the lap: the rotation passes over it until
  every other show has had a turn, then restarts it at episode 1. Films
  (series of one) are not rested this way; `no_repeat_within` covers them.
- `seed: { rotate: 2d }` and `temperature:` rotate what a ranked pool hands
  back. See the section below.

**Grade.** Applies once a program has aired. The measure is the days until
any program (an airing of 5 minutes or more) airs a second time, or the whole
span when nothing repeats; a repeating bumper does not count. 14 days or more
is 3, 7 is 2 (FAST guidance to refresh weekly, above), 3 is 1, under 3 is 0.

## Rotating the list: seed and temperature

**What it is.** A ranked pool, such as a taste-scored films pool, returns
nearly the same ranking every time. Two programming habits keep the head of a
ranking from airing on a loop: walk down the list instead of reopening it at
the top, and refresh the list itself on a schedule. FAST guidance to refresh
weekly and rotate monthly (above) is the second habit at a longer cadence.

**In `channel.yaml`.**

```yaml
seed: { rotate: 2d }        # a new list every two local days
temperature: 0.25           # how far each list strays from the pure ranking
rule:
  blocks:
    - pools:
        - name: kaiju-films
          plugin: "../../plugins/taste-cosine.rhai"
          advance: resume   # walk down the period's list, don't reopen it
        - name: kaiju-shows
          plugin: "../../plugins/taste-cosine.rhai"
          advance: resume
          temperature: 0    # keep the same shows; only the films vary
```

- `advance: resume` is what walks. Each period's list is ranked once, and
  every generation in that period continues down it. `advance: restart`
  reopens the top every generation, so slot 1 airs again whenever the pool
  comes up.
- The seed decides which titles fill a list's exploration slots and, with a
  temperature above 0, the order of the whole list. At temperature 0 a new
  seed changes only the exploration picks.
- Temperature is measured, not guessed. On 034 SyFy's films pools, a 14-day
  web UI review put the median pick at score rank 7 at `0.1`, 15 at
  `0.25`, and 21 at `0.5`. At `0.5`, 15 of 91 films came from below rank 100.
  Run the Review view at two or three values and read the titles.
- A shows pool usually wants `temperature: 0`. A new list there swaps which
  shows the rotation visits, and a show half-way through a season should not
  leave the rotation because a period ended. The walk opens each new list on
  the first show it has not visited this lap, so the tail of a stable list
  still gets its turn.
- `temperature: { rotate: 3d, cycle: [0.5, 0.5, 0.2] }` alternates looser and
  tighter periods. Each field runs its own clock.

**Sizing the cadence** (**inference**). A pool comes up once per pattern pass.
On 034, a pass of 7 films and 21 episodes ran about 27 hours in the review simulation, so a 2-day
period airs about 2 films per films pool. A period shorter than one pass
changes the list before the walk reaches slot 2, and the pool behaves like
`advance: restart` on a new list each time.

A new theme each month is a slot per theme and a schedule per month whose
`dates:` spans that month, such as `["04-01..04-30"]`.
