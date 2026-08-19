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

## Program metadata

What a viewer reads about an item rather than what plays: its title, sub-title, description, season and episode number, rating, year. Carried per scheduled item and turned into guide XML by ErsatzTV-next. Distinct from the item's source, which is the file and the in/out points.

## Metadata cascade

The order in which [[program metadata]] is settled for one item, least specific to most: the catalog row, then the channel, then the block, then the item's own entry. A more specific layer overrides a less specific one field by field, so a block can restate the title without discarding the season number underneath it. The catalog row is the base of the cascade, not a layer in it — it is observed data, and every layer above it is an author stating an intent.

## Missing entry

A catalog `entries` row whose `missing_since` is non-null: every one of its `entry_sources` provenance rows lost its underlying file on a full ingest pass. Never deleted — retained for its `entry_id` (so history, watch-graph, and enrichment data joined on it stay intact) and for reuse if the file resurfaces under a source the next pass re-matches, which clears `missing_since` back to null. Skipped, not excluded, when the scheduler picks new candidates: it still counts toward `no_repeat_within` spacing for airings that already happened, it just isn't eligible to be picked again until it's no longer missing. Contrast a rename, where a source's `playback_path` changes but `missing_since` never gets set.

## Reconciliation sweep

The periodic pass, run on the same interval as [[catalog refresh]], that walks every not-yet-fully-aired playout JSON file (`finish > now`) and patches any item whose embedded `id` (the catalog `entry_id`, already stable across a rename) now resolves to a different `playback_path` than the one baked into the file. Rewrites the file on disk in place; ErsatzTV-next re-reads it per item with no restart needed. An item whose entry has gone missing entirely gets swapped for an error card instead of a path patch.

## Catalog refresh

The periodic re-run of catalog ingest (Plex + local-fs) inside the daemon's own loop, on `catalog_refresh_secs`. Replaces the old startup-only ingest, whose freshness window the same config value already governed.

## Overlay cascade

Which overlay config a channel's single `etv-overlay` process runs, resolved station → channel → block (item deferred — no deployed channel uses `Entry::Item`, the only entry kind an item-level override could attach to). Deepest declared level wins as a **whole-config replacement**, never a field merge — with one additive exception: `overlay: {extend: {layers: [...]}}` keeps the level above and appends layers to it, so shared graphics can be declared once at the station while each channel adds only its own bug (ADR 0008). An extend may also name a different `script:` or replace `config:` wholesale; it carries no geometry and cannot reach inside an inherited layer, so there is still no per-field override. Absence of an `overlay:` key at any level means inherit the nearest ancestor's resolved config; an explicit `clear` opts a level out. Content that varies with the playing item (a title, an award fact) is never a cascade concern — that's the script reading item/block metadata at render time, per #174. A resolved-config change at a block boundary hot-reloads into the *same running* overlay process (mtime-polled, same mechanism as [[program metadata]] already reaches the Rhai script); only a `width`/`height`/`framerate`/`pixel_format` change is rejected at load time, since those are baked into the fifo's byte layout and can't swap without restarting ffmpeg's filter chain. See #48 (original decision, TOML-era) and #174 (metadata bridge); station-level and the hot-reload mechanism are amendments made when porting to YAML, not yet reflected in #48's text.

## Drift

The gap between when a [[daypart]] was meant to start and when it actually does, because the item before it ran past the boundary. Accepted rather than corrected: closing it would mean cutting an item short or leaving dead air, and padding it out is impossible while the library has no interstitials (#85). Bounded by the longest item in the pool.

## Broadcast graphics vocabulary

Borrowed industry terms for the things an overlay draws. None of these are schema keys or code identifiers — they exist here so an overlay author can name what they are building and search for real reference material instead of inventing a word for it. Everything below is one of the [[overlay cascade]]'s layers.

- **Bug** (also **DOG**, "digital on-screen graphic") — the persistent channel mark parked in a corner. `type: logo` is a bug. Every deployed channel here has one.
- **Chyron** — any machine-generated on-screen text. From the Chyron Corporation, whose character generators the term outlived; **char gen** / **CG** are the equipment words, **Aston** the British equivalent (Aston Broadcast Systems). `shared/title-chyron.rhai` is one.
- **Lower third** — a chyron in the bottom third of the frame, usually a name-and-role bar. Positional, not a news-only form: "You're watching *Die Hard*" in the same place is still a lower third.
- **Snipe** — the promotional graphic that slides in over running content to advertise something else, typically what's on next. Distinct from a lower third, which is about what's on *now*.
- **Now / Next / Later** — the three-item rundown snipe: what's playing, what follows, what follows that. The station only carries a one-item lookahead today (`next_title`), so "Later" needs a second one.
- **Bumper** — a short branded segment *between* programs rather than an overlay on top of one. **Ident** is the British word for the channel-identity flavour of it.
- **Squeezeback** (also **L-bar**, **L-wrap**) — the program is scaled down into part of the frame and the freed space carries promo art. Not achievable with an alpha overlay alone; it needs ETV-next's filter chain to scale the video.
- **Endcap** / **credit squeeze** — the squeezeback specifically applied over a program's closing credits.
- **On-air branding package** — the whole set as one design artifact: bug, chyron styles, snipes, bumpers. The search term for reference work.
