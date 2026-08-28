# For You Mark — verdict

**Question.** The three For You channels need one family silhouette with three
variants. 002 and 003 currently point their bug at the *personal* marks
(`shared/pierce-logo.png`, `shared/madison-logo.png`), which are the same PNGs
`010-pierce` and `011-madison` use — so "For Pierce" and "Pierce" are
indistinguishable on screen. 001 wears a beamed-eighth-note standing in for a
short-video feed.

**Winner: Beacon.** Open linear form, monogram core. Arcs sit either side of the
core rather than stacked over it, so the mark reads as the radio callsign
convention — `((P))`, a station transmitting at you. 001 keeps a solid core: the
family head is the transmitter, the two people are what it is tuned to.

Rejected, and why:

- **Badge** — contained squircle, monogram knocked out. Most legible of the four
  at every size. Lost on weight: a filled 84x84 block is the loudest possible
  bug, held every second of every programme.
- **Trace** — a taste sparkline, letter-free, differentiated by peak shape.
  Lost because "sharp peak vs broad plateau" is a coin flip at 24px, which is
  the size that decides whether 002 and 003 are actually distinct.
- **Aperture** — segmented ring, channel encoded as which segment is weighted.
  Best concept (even weights literally draw the pooled house vector) and least
  legible: it differentiates on stroke weight, the first property a bilinear
  downscale destroys (#296).

**Found by looking, not by reasoning.** Beacon's first drawing stacked two arcs
over a rounded block, which is the wifi glyph — a bug that reads as a
connectivity error. Trace's 001 was a two-peak line that drew the letterform
**M**, colliding with the one channel it has to stay distinct from. Both were
invisible in the source and obvious in the screenshot.

**Promotion.** Beacon is drawn as SVG in the fragment at `viewBox 0 0 100 100`:
four arcs (r=30 and r=42, either side, stroke 8.5, round caps) plus a 36x36
rounded core at rx=11, with the monogram knocked out via `<mask>`. That is the
source to port — SVG committed under a widened `.gitignore` carve-out, PNG built
from it by `tools/build-marks.sh`, which also supports a Unicode-glyph source.
Rewrite it properly; do not copy the prototype markup.
