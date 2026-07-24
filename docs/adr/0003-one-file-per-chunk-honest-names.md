# A playout file is one chunk, and its name must bracket its items

Each playout JSON file covers exactly one `chunk_hours` chunk on the local-time
grid, holds every item scheduled in that chunk, and is named `{start}_{finish}`
where `finish` is the chunk boundary once the chunk is full, or the last item's
finish while the chunk is still being filled. A file's name never claims more
time than the items inside it cover.

## Why

ErsatzTV-next selects what to play in two stages (`ersatztv-channel`'s
`playout_loader.rs`): first it picks the *file* whose filename span `[start,
finish)` contains `now`, then it `rfind`s the *item* in that file whose span
contains `now`. Both must contain `now`. If the filename claims a range the
items do not fill, next selects the file, finds no covering item, and falls back
to a solid-black/silence stream. The filename is a promise the items must keep.

The station used to break that promise two ways at once. `emit_window` was
called once per *generation* — one full pass of a channel's playlist, often only
a few minutes — and each call wrote a file named from the generation's start to
the next chunk *boundary*. So a 3-minute pass produced a file named for a
6-hour span holding 3 minutes, and a 6-hour chunk accumulated ~120 overlapping
files (one per pass), every one over-claiming to the same boundary. Playback
limped along only because the files happened to tile by start time; the moment a
run stopped mid-chunk and restarted, `scan::highest_finish` read the
over-claiming *name*, believed the chunk was full, resumed past it, and left a
permanent hole that aired as black. Six of sixteen sample channels were black
from exactly this.

Two units had been conflated: a **generation** (one playlist pass, variable
length, the unit of resolution and resume) and a **chunk** (a fixed
`chunk_hours` slice, the unit of file storage that next consumes). Binding file
identity to the generation instead of the chunk is what produced both the file
explosion and the over-claim.

## What we chose, and the rejected alternatives

- **File identity is the chunk boundary, not the generation start.** A
  generation's items are merged into the file for the chunk they fall in
  (read the existing chunk file, append, rewrite), so a chunk grows across
  generations and ticks into one file rather than spawning a file per pass.
  ~4 files/day/channel instead of ~120/chunk. This also makes next's per-lookup
  directory scan cheap, which it was not at hundreds of files per channel.

- **Honest names, with a full chunk still named to its boundary.** A *complete*
  chunk is named `[boundary, boundary]` so adjacent chunks tile exactly and an
  item straddling a boundary (emitted whole into both neighbours, so the
  boundary never cuts a program) is found from either side. A chunk still being
  filled is named `[boundary, last-item-finish]`, so it never claims the empty
  tail. Because the frontier chunk carries the true content end in its name,
  `highest_finish` stays a cheap filename parse and is correct — no need to open
  files to know how far a channel is materialized.

- **Rejected: name every file to its content end, always.** Naming a full chunk
  to its last straddling item's finish makes adjacent file name-ranges *overlap*
  (chunk k ends after chunk k+1 begins), and next's "first file whose range
  contains now" lookup becomes order-dependent and ambiguous. Naming full chunks
  to the boundary keeps the ranges tiling and disjoint.

- **Rejected: minimal patch — honest names but still one file per generation.**
  Fixes the black screen but leaves ~120 files/chunk, which burdens next's
  per-lookup folder scan and the retention sweep. The chunk is the right storage
  unit; a file per pass never was.

- **Rejected: smaller chunks as the default.** next re-scans the whole playout
  folder on every file lookup, so more files is strictly worse for the consumer,
  and a chunk is the unit of atomic rewrite and rewind granularity. Pressure is
  toward fewer/larger files. `chunk_hours` stays configurable (a shorter chunk
  is a valid choice for a channel that wants finer regen granularity), but the
  default is 6h and nothing should go smaller without a concrete reason.

- **Detection and self-heal, not just prevention.** The write-side fix stops new
  holes, but files already on disk stay wrong, and an unforeseen cause could
  still open a hole. So a startup scan reads actual item coverage and rewinds +
  regenerates from the earliest uncovered instant, and the per-roll-tick check
  validates that the about-to-air window is item-covered before the playhead
  reaches it. Prevention is the real fix; these are the backstops.
