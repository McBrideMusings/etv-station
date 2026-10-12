# Programming principles

This page lists the TV-programming principles a channel review judges each
channel against. Each principle has four parts:

- what it means, with the source it comes from;
- one real network that does it;
- how a `channel.yaml` says it today, with the key and a short example;
- where today's schema cannot say it, the bead that tracks the gap.

A number marked **unsourced** or **inference** is reasoning written for this
page. No source states it. Treat it as a starting question for a review, not
as a rule. A review asks each question and shows the numbers. Nothing on this
page is a pass/fail threshold.

The keys named below are documented in the [config schema](/schema), except
`chance` and `from` on a pattern step, which the schema page does not cover yet;
`crates/etv-station/src/config/pool.rs` defines both.
Every YAML example is a fragment. `pools:`, `pattern:` and `sequencer:` sit
inside one entry of `rule.blocks`, as the first example shows in full.

"Depth" on this page has the meaning in the [glossary](/CONTEXT#depth): how
long a channel runs before something airs again. An episode count is called a
library size.

## Dayparting

**What it is.** A network splits the day into parts and airs programs suited
to whoever is watching at that hour
([Wikipedia, Broadcast programming](https://en.wikipedia.org/wiki/Broadcast_programming)).

**Network example.** Daytime shows aim at the audience that is home during the
day; prime time aims at the whole household (same source).

**In `channel.yaml` today.** One block whose pools are the dayparts, arranged
by the daypart sequencer. Each pool claims its hours in its own `config:`; the
pool with no hours fills every hour nobody claims. Hours are local to the
station's `tz`.

```yaml
rule:
  blocks:
    - pools:
        - name: latenight
          expr: 'item.show in ["Rick and Morty", "The Boondocks"]'
          config: { hour_start: 21, hour_end: 6 }
        - name: default
          expr: 'item.type == "movie"'
      sequencer: "../plugins/daypart-sequencer.rhai"
```

The sample script is `examples/plugins/daypart-sequencer.rhai`. Daypart
boundaries drift by the overrun of the last item rather than cutting it
([schema, Dayparting](/schema#dayparting-14)).

**Not expressible today.** A daypart pool cannot resume a show where it left
off: every generation restarts it at the top of its list. Tracked in
`etv-station-199`.

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

**Not expressible today.** `take` is a fixed count; there is no random range
such as "2 to 4" episodes of one show. Tracked in `etv-station-cyq9`.

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

**In `channel.yaml` today.** A daypart pool restricted to weekday hours.
Dayparts are whole hours, and the pool fills its hour: an hour-long drama airs
about one episode a weekday, and a half-hour sitcom would air two or three.

```yaml
pools:
  - name: strip
    expr: 'item.show == "The X-Files"'
    order: "season:asc,episode:asc"
    config: { hour_start: 18, hour_end: 19, weekdays: [mon, tue, wed, thu, fri] }
  - name: default
    expr: 'item.type == "movie"'
sequencer: "../plugins/daypart-sequencer.rhai"
```

**Not expressible today.**

- The strip does not resume across generations: each generation starts the
  show again from its first episode. Tracked in `etv-station-199`.
- A show that strips on two channels keeps a separate place on each, so it
  restarts on every channel it appears on. Tracked in `etv-station-160`
  (`cursor_scope: global`).

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

**Not expressible today.** No key switches a show from in-order to shuffled
once it has aired through once. Tracked in `etv-station-vc98`.

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

**In `channel.yaml` today.** A theme can only be a standing block: a pool
whose `expr` selects the theme (`item.collections.contains("Christmas")`)
airs all year, not for a date range. A daypart pool's `weekdays:` covers a
weekly slot such as a Star of the Month night, but not a month.

**Not expressible today.** A block cannot declare the calendar dates it airs
on. Tracked in `etv-station-162`.

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

**Not expressible today.** A marathon on a date (New Year's Day,
Thanksgiving) needs dated blocks. Tracked in `etv-station-162`.

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
      layers:
        - { type: image, path: logo.png, corner: bottom_right, height: 56 }
  ```

- **Lower third / snipe timing:** the station's Now/Next snipe
  (`deploy/appdata/shared/now-next-snipe.rhai`) reads its timing from the
  overlay's `config:`. `interval_secs` (default 300) sets how often it runs and
  `hold_secs` (default 4.5) how long each card stays.
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

**Not expressible today.** Switching a pool's selection on a date, such as a
new theme each month, needs dated blocks. Tracked in `etv-station-162`.
