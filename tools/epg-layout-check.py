# /// script
# requires-python = ">=3.11"
# dependencies = ["textual>=0.60", "httpx>=0.27"]
# ///
"""Assert the epg-browser TUI fits both of its layouts.

It has two regimes, picked from the terminal width by EpgBrowserApp.WIDE_COLUMNS:
below it, #detail is a bottom row spanning the full width; at or above it,
#detail is a right-hand column. Three side-by-side panes clip under 128 columns,
which is why the stacked layout is the default. This drives the real app under
Textual's headless test driver against fixture data — no network, no station —
and fails if any pane spills past the terminal edge, or if #detail is too narrow
to hold the widest line in the app (the full stream URL).

With no arguments it checks both regimes. Given a size, it checks just that one.

Run with: uv run tools/epg-layout-check.py [width] [height]
"""

from __future__ import annotations

import asyncio
import importlib.util
import re
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

MIN_WIDTH = 80  # the width the two-row layout is required to survive
MIN_HEIGHT = 24
WIDE_WIDTH = 160  # a terminal wide enough to earn the right-hand detail column
WIDE_HEIGHT = 45
DETAIL_MIN = 52  # the full stream URL — the widest single line in the app.
# Measured against #detail's CONTENT box, so its border and padding are already
# excluded and the number is comparable to a character count.

SRC = Path(__file__).resolve().parent / "epg-browser.py"
spec = importlib.util.spec_from_file_location("epg_browser", SRC)
epgb = importlib.util.module_from_spec(spec)
sys.modules["epg_browser"] = epgb
spec.loader.exec_module(epgb)


def fixture() -> tuple[dict, list]:
    """Two channels and a back-to-back run of long-titled shows, so the
    programme rows include a worst-case changeover block — one that renders as
    a block line plus a hanging line for the incoming show. `number` is set
    because every row is prefixed with the channel number, and a fixture
    without one measures a row six characters narrower than prod draws."""
    now = datetime.now(timezone.utc)
    channels = {
        f"ersatztv.{n}": epgb.Channel(
            tvg_id=f"ersatztv.{n}",
            name=name,
            logo=None,
            stream_url=f"http://station.example:8419/channel/{n}.m3u8",
            number=n,
        )
        for n, name in ((1, "001-for-you"), (2, "002-for-pierce"))
    }
    progs = []
    for i, title in enumerate(["Shoresy", "Pride and Prejudice", "Edith's Diary"]):
        start = now + timedelta(hours=i)
        progs.append(
            epgb.Programme(
                channel_id="ersatztv.1",
                start=start,
                stop=start + timedelta(hours=1),
                title=title,
                sub_title="Set the Tone",
                desc="The Bulldogs face off against Hunt and their veteran defence.",
                season=2,
                episode=3,
                categories=["Action", "Comedy", "Drama"],
                rating="TV-MA",
                icon="http://station.example:8419/artwork/imdb_tt28754309.jpg",
            )
        )
    return channels, progs


MARKUP_RE = re.compile(r"\[/?[a-z][^\]]*\]")


def _label_text(label) -> str:
    """The text the terminal actually draws. Rich markup tags are stripped —
    a row is styled with [dim]…[/dim] around the channel number and the
    season/episode suffix, and counting those tags as characters overstates
    every row by ~11 and reports clipping that isn't there."""
    for attr in ("content", "_content", "renderable"):
        if hasattr(label, attr):
            return MARKUP_RE.sub("", str(getattr(label, attr)))
    return ""


async def check(width: int, height: int) -> bool:
    epgb.fetch_lineup = lambda host: fixture()
    app = epgb.build_app("http://127.0.0.1:8409")
    ok = True
    async with app.run_test(size=(width, height)):
        wide = app.query_one("#layout").has_class("wide")
        print(f"=== {width}x{height} — detail {'right column' if wide else 'bottom row'} ===")
        panes = ("#layout", "#channels", "#programmes", "#detail")
        for sel in panes:
            r = app.query_one(sel).region
            print(f"  {sel:12} x={r.x:<4} y={r.y:<4} w={r.width:<4} h={r.height}")

        right = max(app.query_one(s).region.right for s in panes)
        bottom = max(app.query_one(s).region.bottom for s in panes)
        for what, got, limit in (("right edge", right, width), ("bottom edge", bottom, height)):
            good = got <= limit
            ok &= good
            print(f"  {what:12} {got} <= {limit}  {'OK' if good else 'OVERFLOW'}")

        # The detail pane holds the widest single line in the app — the full
        # stream URL. Stacked, it must span the terminal; as a column, it must
        # still be wide enough for that line plus its 1-column padding.
        detail_w = app.query_one("#detail").content_size.width
        floor = DETAIL_MIN if wide else width - 2
        good = detail_w >= floor
        ok &= good
        print(f"  {'detail width':12} {detail_w} >= {floor} (terminal {width})  {'OK' if good else 'TOO NARROW'}")

        # Report-only: a changeover row can still clip at 80 columns. The full
        # text is always in the detail pane, so this is not a failure.
        pane = app.query_one("#programmes").region.width
        rows = [len(_label_text(i.query_one("Label")).split("\n")[0]) for i in app.query("#programmes ListItem")]
        widest = max(rows, default=0)
        print(f"  {'widest row':12} {widest} chars, pane {pane}  {'fits' if widest <= pane else 'clips (detail pane has it)'}")
    return ok


def main() -> None:
    if len(sys.argv) > 1:
        sizes = [(int(sys.argv[1]), int(sys.argv[2]) if len(sys.argv) > 2 else MIN_HEIGHT)]
    else:
        sizes = [(MIN_WIDTH, MIN_HEIGHT), (WIDE_WIDTH, WIDE_HEIGHT)]
    ok = all([asyncio.run(check(w, h)) for w, h in sizes])
    print("PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
