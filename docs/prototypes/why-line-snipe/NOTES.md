# Why Line Snipe — verdict

**Question.** The NOW card gains a second row explaining why the film on screen
was picked. How should that line read — as part of the card, or as something
else?

**Winner: Subtitle.** The why line takes the row the episode title already uses,
at the size and alpha `deploy/appdata/station.yaml` already declares for layer 3
(16px, alpha 0.667, `bottom_left`, margin 40), arriving on the third stagger
beat at 0.16s. Nothing in the renderer, the layer list, or the motion changes —
only the string the script writes into layer 3.

Rejected, and why:

- **Annotation** — italic, alpha 0.50, pulled back to the label column with an
  em-dash lead. Lost twice: italic Helvetica at 15px over moving video is the
  least legible combination of the four, and starting the why at x=40 while the
  title starts at x=116 gives the card two left margins instead of one.
- **Aside** — arrives 0.55s after the card lands, opacity only, warm tint.
  Genuinely the best *reading* of what the line is (the station speaking after
  it told you what's on) and it costs 0.55s of a 4.5s hold, so the sentence is
  fully legible for under four seconds.
- **Inline** — appended to the title row after a middot, no second row. Lost
  because the two facts fuse into one line and the why gets no emphasis of its
  own.

**Measured, against two claims I had written and had wrong.** I had said the
long why line would overflow the 1280px frame. It does not: the worst realistic
case ends around x=830, and at ~7.4px per character at 15px Helvetica the row
has roughly 1,120px — about 150 characters — before it reaches the edge. The
binding limit is `max_chars: 40`, which is a config number of ours, not a
physical one. **Raise it for this row when promoting.**

**Also corrected against the renderer.** A scrim is a gradient across the whole
band, opaque at the frame edge and zero alpha at `size`
(`vello_renderer.rs:249`) — not the flat plate the first draft drew.

**Promotion.** One flag on `deploy/appdata/shared/now-next-snipe.rhai`, gated on
the item carrying a reason set, so the other 60 channels are untouched. Rewrite
properly; do not copy the prototype's JS.
