# /// script
# requires-python = ">=3.11"
# dependencies = ["textual>=0.60", "httpx>=0.27"]
# ///
"""Assert the epg-browser TUI still fits a narrow terminal.

The three-column layout it started with needed 128 columns before anything
clipped, which is wider than a laptop terminal or a split pane. This drives the
real app under Textual's headless test driver against fixture data — no network,
no station — and fails if any pane spills past the terminal edge at 80 columns.

Run with: uv run tools/epg-layout-check.py [width] [height]
"""

from __future__ import annotations

import asyncio
import importlib.util
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

MIN_WIDTH = 80  # the width the two-row layout is required to survive
MIN_HEIGHT = 24

SRC = Path(__file__).resolve().parent / "epg-browser.py"
spec = importlib.util.spec_from_file_location("epg_browser", SRC)
epgb = importlib.util.module_from_spec(spec)
sys.modules["epg_browser"] = epgb
spec.loader.exec_module(epgb)


def fixture() -> tuple[dict, list]:
    """Two channels and a back-to-back run of long-titled shows, so the
    programme rows include a worst-case "A → B" changeover line."""
    now = datetime.now(timezone.utc)
    channels = {
        f"ersatztv.{n}": epgb.Channel(
            tvg_id=f"ersatztv.{n}",
            name=name,
            logo=None,
            stream_url=f"http://100.114.249.118:8419/channel/{n}.m3u8",
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
                icon="http://100.114.249.118:8419/artwork/imdb_tt28754309.jpg",
            )
        )
    return channels, progs


def _label_text(label) -> str:
    for attr in ("content", "_content", "renderable"):
        if hasattr(label, attr):
            return str(getattr(label, attr))
    return ""


async def check(width: int, height: int) -> bool:
    epgb.fetch_lineup = lambda host: fixture()
    app = epgb.build_app("http://127.0.0.1:8409")
    ok = True
    async with app.run_test(size=(width, height)):
        print(f"=== {width}x{height} ===")
        for sel in ("#top", "#channels", "#programmes", "#detail"):
            r = app.query_one(sel).region
            print(f"  {sel:12} x={r.x:<4} y={r.y:<4} w={r.width:<4} h={r.height}")

        right = max(app.query_one(s).region.right for s in ("#channels", "#programmes", "#detail"))
        bottom = max(app.query_one(s).region.bottom for s in ("#top", "#detail"))
        for what, got, limit in (("right edge", right, width), ("bottom edge", bottom, height)):
            good = got <= limit
            ok &= good
            print(f"  {what:12} {got} <= {limit}  {'OK' if good else 'OVERFLOW'}")

        # The detail pane holds the widest single line in the app — the full
        # stream URL. If it no longer spans the terminal, the two-row layout
        # has been undone.
        detail_w = app.query_one("#detail").region.width
        good = detail_w >= width - 2
        ok &= good
        print(f"  {'detail width':12} {detail_w} (terminal {width})  {'OK' if good else 'TOO NARROW'}")

        # Report-only: a changeover row can still clip at 80 columns. The full
        # text is always in the detail pane, so this is not a failure.
        pane = app.query_one("#programmes").region.width
        rows = [len(_label_text(i.query_one("Label")).split("\n")[0]) for i in app.query("#programmes ListItem")]
        widest = max(rows, default=0)
        print(f"  {'widest row':12} {widest} chars, pane {pane}  {'fits' if widest <= pane else 'clips (detail pane has it)'}")
    return ok


def main() -> None:
    width = int(sys.argv[1]) if len(sys.argv) > 1 else MIN_WIDTH
    height = int(sys.argv[2]) if len(sys.argv) > 2 else MIN_HEIGHT
    ok = asyncio.run(check(width, height))
    print("PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
