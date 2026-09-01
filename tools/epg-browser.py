# /// script
# requires-python = ">=3.11"
# dependencies = ["textual>=0.60", "httpx>=0.27"]
# ///
"""Browse a station's live schedule: a Textual TUI by default, or JSON
subcommands for scripted/agent use. Reads /xmltv.xml + /channels.m3u over HTTP
from prod, local, or any --host — never the filesystem, so the same code path
works against a box this Mac has no mount to.

Run with: uv run tools/epg-browser.py [args]
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import re
import subprocess
import sys
from dataclasses import asdict, dataclass, field
from datetime import date, datetime, timedelta, timezone
from pathlib import Path
from xml.etree import ElementTree as ET

import httpx

REPO_ROOT = Path(__file__).resolve().parent.parent
XMLTV_DATETIME = "%Y%m%d%H%M%S %z"

PRESETS = {
    "local": "http://127.0.0.1:8409",
}


def load_dotenv_var(name: str) -> str | None:
    """Read one KEY=VALUE line from .env without adding a dependency on
    python-dotenv. Only used to resolve PROD_URL when it isn't already
    exported into the environment (uv run doesn't source .env for us)."""
    env_path = REPO_ROOT / ".env"
    if not env_path.is_file():
        return None
    for line in env_path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        if key.strip() == name:
            return value.strip().strip('"').strip("'")
    return None


def resolve_host(host_flag: str | None, local_flag: bool) -> str:
    """--host wins outright; else --local; else prod (the default, --prod is a
    no-op spelled out for symmetry with --local)."""
    if host_flag:
        return host_flag.rstrip("/")
    if local_flag:
        return PRESETS["local"]
    prod_url = os.environ.get("PROD_URL") or load_dotenv_var("PROD_URL")
    if not prod_url:
        print(
            "error: PROD_URL not set (checked env and .env) — pass --host or --local",
            file=sys.stderr,
        )
        sys.exit(1)
    return prod_url.rstrip("/")


@dataclass
class Channel:
    tvg_id: str
    name: str
    logo: str | None
    stream_url: str
    number: int | None = None


@dataclass
class Programme:
    channel_id: str
    start: datetime
    stop: datetime
    title: str | None = None
    sub_title: str | None = None
    desc: str | None = None
    icon: str | None = None
    season: int | None = None
    episode: int | None = None
    categories: list[str] = field(default_factory=list)
    rating: str | None = None
    star_rating: str | None = None

    def to_json(self) -> dict:
        d = asdict(self)
        d["start"] = self.start.isoformat()
        d["stop"] = self.stop.isoformat()
        return d


EXTINF_RE = re.compile(
    r'#EXTINF:-1(?:\s+tvg-id="(?P<tvg_id>[^"]*)")?'
    r'(?:\s+tvg-chno="(?P<tvg_chno>[^"]*)")?'
    r'(?:\s+tvg-name="(?P<tvg_name>[^"]*)")?'
    r'(?:\s+tvg-logo="(?P<tvg_logo>[^"]*)")?'
    r'(?:\s+group-title="(?P<group>[^"]*)")?'
    r",\s*(?P<display>.*)"
)

# The station does not emit `tvg-chno` (#375), so the number a client actually
# dials has to be recovered from the id ETV-next assigns — `ersatztv.<N>`,
# where N is `index + 1` over the roster (crates/etv-station/src/etv_next.rs:355).
# Read `tvg-chno` first anyway, so this keeps working the day #375 lands and
# stops guessing the moment the real number is on the wire.
TVG_ID_NUM_RE = re.compile(r"\.(\d+)$")


def channel_number(tvg_chno: str | None, tvg_id: str, position: int) -> int | None:
    if tvg_chno and tvg_chno.strip().isdigit():
        return int(tvg_chno.strip())
    m = TVG_ID_NUM_RE.search(tvg_id)
    if m:
        return int(m.group(1))
    return position or None


def parse_channels_m3u(text: str) -> dict[str, Channel]:
    channels: dict[str, Channel] = {}
    lines = [ln for ln in text.splitlines() if ln.strip()]
    i = 0
    position = 0
    while i < len(lines):
        line = lines[i]
        if line.startswith("#EXTINF"):
            m = EXTINF_RE.match(line)
            stream_url = lines[i + 1].strip() if i + 1 < len(lines) else ""
            if m and m.group("tvg_id"):
                position += 1
                channels[m.group("tvg_id")] = Channel(
                    tvg_id=m.group("tvg_id"),
                    name=m.group("tvg_name") or m.group("display") or m.group("tvg_id"),
                    logo=m.group("tvg_logo") or None,
                    stream_url=stream_url,
                    number=channel_number(m.group("tvg_chno"), m.group("tvg_id"), position),
                )
            i += 2
        else:
            i += 1
    return channels


def parse_xmltv(text: str) -> list[Programme]:
    root = ET.fromstring(text)
    programmes: list[Programme] = []
    for el in root.findall("programme"):
        start = datetime.strptime(el.get("start", ""), XMLTV_DATETIME)
        stop = datetime.strptime(el.get("stop", ""), XMLTV_DATETIME)
        icon_el = el.find("icon")
        rating_el = el.find("rating/value")
        star_el = el.find("star-rating/value")
        season = episode = None
        ep_num = el.find('episode-num[@system="xmltv_ns"]')
        if ep_num is not None and ep_num.text:
            parts = ep_num.text.strip(".").split(".")
            if len(parts) >= 2 and parts[0].isdigit() and parts[1].isdigit():
                season, episode = int(parts[0]) + 1, int(parts[1]) + 1
        programmes.append(
            Programme(
                channel_id=el.get("channel", ""),
                start=start,
                stop=stop,
                title=(el.findtext("title") or None),
                sub_title=(el.findtext("sub-title") or None),
                desc=(el.findtext("desc") or None),
                icon=(icon_el.get("src") if icon_el is not None else None),
                season=season,
                episode=episode,
                categories=[c.text for c in el.findall("category") if c.text],
                rating=(rating_el.text if rating_el is not None else None),
                star_rating=(star_el.text if star_el is not None else None),
            )
        )
    return programmes


def fetch_lineup(host: str) -> tuple[dict[str, Channel], list[Programme]]:
    with httpx.Client(timeout=10.0) as client:
        m3u = client.get(f"{host}/channels.m3u")
        m3u.raise_for_status()
        xmltv = client.get(f"{host}/xmltv.xml")
        xmltv.raise_for_status()
    channels = parse_channels_m3u(m3u.text)
    programmes = parse_xmltv(xmltv.text)
    return channels, programmes


def programmes_for(programmes: list[Programme], channel_id: str) -> list[Programme]:
    return sorted(
        (p for p in programmes if p.channel_id == channel_id), key=lambda p: p.start
    )


def current_and_next(
    programmes: list[Programme], channel_id: str, now: datetime
) -> tuple[Programme | None, Programme | None]:
    chan = programmes_for(programmes, channel_id)
    current = next((p for p in chan if p.start <= now < p.stop), None)
    upcoming = [p for p in chan if p.start > now]
    nxt = upcoming[0] if upcoming else None
    return current, nxt


def _fmt_dt(dt: datetime, today: date | None = None, with_date: bool = False) -> str:
    """Weekday-qualified so a two-day-old entry can't be misread as "later
    today" — a bare %H:%M is exactly what made the schedule gap in #292 look
    like an upcoming show instead of a stale listing days in the past. When a
    reference date is provided via `today`, drops the weekday prefix for blocks
    that fall on today (saving 4 characters) since the weekday only earns its
    place once the list runs past midnight.

    `with_date` swaps the weekday for the calendar date. The weekday only
    disambiguates a window under seven days wide: history mode reaches back
    exactly seven, so its oldest rows land on the same weekday as today and
    "Wed 16:30" stops meaning anything. Two characters wider, and unambiguous
    at any depth."""
    if today is not None and dt.date() == today:
        return dt.strftime("%H:%M")
    if with_date:
        return dt.strftime("%m-%d %H:%M")
    return dt.strftime("%a %H:%M")


def _fmt_dt_sec(dt: datetime, with_date: bool = False) -> str:
    """Same as _fmt_dt but with seconds — used for real programme-boundary
    timestamps (changeovers, EPG-data cutoffs), never for block labels."""
    if with_date:
        return dt.strftime("%m-%d %H:%M:%S")
    return dt.strftime("%a %H:%M:%S")


def _fmt_span(seconds: float) -> str:
    """4h23m gap or 12m gap — never bare minutes-since-epoch-style noise."""
    total = int(seconds)
    h, rem = divmod(total, 3600)
    m = rem // 60
    if h and m:
        return f"{h}h{m:02d}m"
    if h:
        return f"{h}h"
    return f"{m}m"


def current_gap(programmes: list[Programme], channel_id: str, now: datetime) -> tuple[datetime, datetime | None] | None:
    """(gap_start, gap_end) if `now` falls in a scheduling gap for this channel,
    else None. gap_end is None when nothing at all is scheduled past `now` —
    the "playout generation stalled" case, not just a short between-shows gap.
    """
    chan = programmes_for(programmes, channel_id)
    if any(p.start <= now < p.stop for p in chan):
        return None
    before = [p for p in chan if p.stop <= now]
    after = [p for p in chan if p.start > now]
    gap_start = max((p.stop for p in before), default=None)
    gap_end = min((p.start for p in after), default=None)
    if gap_start is None and gap_end is None:
        return None  # no programmes at all for this channel — nothing to report a span for
    return gap_start, gap_end


# --- JSON subcommands -------------------------------------------------------


def cmd_channels(host: str) -> None:
    channels, programmes = fetch_lineup(host)
    now = datetime.now(timezone.utc)
    out = []
    for tvg_id, chan in channels.items():
        current, _ = current_and_next(programmes, tvg_id, now)
        out.append(
            {
                "id": tvg_id,
                "number": chan.number,
                "name": chan.name,
                "stream_url": chan.stream_url,
                "current": current.to_json() if current else None,
            }
        )
    print(json.dumps(out, indent=2))


def cmd_channel(host: str, channel_id: str) -> None:
    channels, programmes = fetch_lineup(host)
    chan = channels.get(channel_id)
    if chan is None:
        print(f"error: no channel with id {channel_id!r}", file=sys.stderr)
        sys.exit(1)
    out = {
        "id": chan.tvg_id,
        "number": chan.number,
        "name": chan.name,
        "stream_url": chan.stream_url,
        "programmes": [p.to_json() for p in programmes_for(programmes, channel_id)],
    }
    print(json.dumps(out, indent=2))


def cmd_current(host: str, channel_id: str) -> None:
    _, programmes = fetch_lineup(host)
    now = datetime.now(timezone.utc)
    current, _ = current_and_next(programmes, channel_id, now)
    print(json.dumps(current.to_json() if current else None, indent=2))


async def check_one(client: httpx.AsyncClient, url: str, sem: asyncio.Semaphore) -> int | str:
    async with sem:
        try:
            resp = await client.head(url, timeout=10.0)
            if resp.status_code == 405:
                resp = await client.get(url, headers={"Range": "bytes=0-0"}, timeout=10.0)
            return resp.status_code
        except httpx.HTTPError as e:
            return f"error: {e}"


def cmd_artwork_check(host: str, channel_id: str | None) -> None:
    channels, programmes = fetch_lineup(host)
    scoped = (
        [p for p in programmes if p.channel_id == channel_id]
        if channel_id
        else programmes
    )
    targets: list[tuple[str, str, str | None]] = []
    for chan in channels.values():
        if channel_id and chan.tvg_id != channel_id:
            continue
        if chan.logo:
            targets.append((chan.logo, chan.tvg_id, None))
    for p in scoped:
        if p.icon:
            targets.append((p.icon, p.channel_id, p.title))

    async def run() -> list[dict]:
        sem = asyncio.Semaphore(8)
        broken = []
        async with httpx.AsyncClient() as client:
            results = await asyncio.gather(
                *(check_one(client, url, sem) for url, _, _ in targets)
            )
        for (url, chan, title), status in zip(targets, results):
            if status != 200:
                broken.append({"url": url, "status": status, "channel": chan, "programme": title})
        return broken

    broken = asyncio.run(run())
    print(json.dumps({"checked": len(targets), "broken": broken}, indent=2))


# --- Textual TUI -------------------------------------------------------------


def build_app(host: str):
    """Construct the TUI without running it. Split from run_tui so
    tools/epg-layout-check.py can drive the same app under Textual's headless
    test driver and assert the layout still fits a narrow terminal."""
    from rich.markup import escape
    from textual import work
    from textual.app import App, ComposeResult
    from textual.containers import Container, VerticalScroll
    from textual.widgets import Footer, Header, ListItem, ListView, Label, Static
    from textual.worker import get_current_worker

    class EpgBrowserApp(App):
        ENABLE_COMMAND_PALETTE = False
        # One grid, two regimes, picked from the terminal's width by
        # _apply_layout. Three side-by-side panes need WIDE_COLUMNS before
        # anything clips (34+1 channels, 40+1 programmes, 52 for the widest
        # detail line — the full stream URL), which is wider than a laptop
        # terminal or a split pane. Narrower than that, detail stacks
        # underneath and gets the full terminal width, which is the pane that
        # actually wanted it: URLs, a `desc` paragraph, a comma-joined category
        # list. Vertical space is the cheap axis — the programme list never
        # fills a terminal's height — so stacking is the default and the right
        # column is what a wide terminal buys.
        CSS = """
        #layout {
            layout: grid;
            grid-size: 2 2;
            grid-columns: 34 1fr;
            grid-rows: 2fr 1fr;
        }
        #layout.wide {
            grid-size: 3 1;
            grid-columns: 34 1fr 55;
            grid-rows: 1fr;
        }
        #channels { border-right: solid $accent; }
        #programmes { min-width: 40; }
        #detail { column-span: 2; border-top: solid $accent; padding: 0 1; }
        #layout.wide #detail {
            column-span: 1;
            border-top: none;
            border-left: solid $accent;
        }
        """
        # Terminal columns at or above which #detail becomes a right-hand
        # column instead of a bottom row. Derived from the panes themselves,
        # not taste: 34 channels + 40 minimum programmes + 55 detail (52 chars
        # of stream URL, 1 column of padding either side, 1 for its left border).
        WIDE_COLUMNS = 129
        BINDINGS = [
            ("r", "refresh", "Refresh"),
            ("h", "toggle_history", "History ↕"),
            ("v", "open_vlc", "Stream in VLC"),
            ("a", "open_artwork", "Artwork preview"),
            ("s", "open_sublime", "XML in Sublime"),
            ("left", "focus_channels", "◀ Channels"),
            ("right", "focus_programmes", "Shows ▶"),
            ("q", "quit", "Quit"),
            ("escape", "quit", "Quit"),
        ]

        REFRESH_SECS = 60  # re-fetch lineup from the station this often
        TICK_SECS = 15  # rebuild the item list (cheap, no fetch) this often —
        # this is what makes the top row's clock roll from e.g. 15:45 to 16:00 live
        BLOCK_MINUTES = 15  # the grid a row's height snaps to — one line per
        # boundary it crosses, not the width of a row (a row is one item now)
        BLOCK_SAFETY_CAP = 2000  # ~20 days of blocks walked — a runaway-loop backstop, not a UX truncation
        GUTTER_W = 2  # the ┌/│/└ rule column between the channel tag and the
        # clock, plus its trailing space
        # How far back history mode reaches, when the guide still holds that
        # much. The station's default `retention_days` is 7, so in practice the
        # retained data runs out first and this never truncates — it is here so
        # a channel with a longer retention can't produce a list nobody can
        # scroll, not to hide anything at the usual setting.
        HISTORY_DAYS = 7

        def __init__(self, host: str) -> None:
            super().__init__()
            self.host = host
            self.channels: dict[str, Channel] = {}
            self.programmes: list[Programme] = []
            self.selected_channel: str | None = None
            # True until the first lineup lands (from the startup fetch or a
            # later refresh); refresh_detail shows a loading message while
            # this is set instead of "No channel selected."
            self.loading = True
            self.load_error: str | None = None
            # False: now at the top, reading forward. True: history mode — now
            # at the BOTTOM, reading back. See _load_programmes.
            self.history = False
            # Parallel to #programmes items. Each entry is either:
            #   ("item", seg_start, (Programme|None, start, stop))  — one media item (or one gap span)
            #   ("end", end_bound, None)          — "Last EPG data: ..."; forward mode, last row
            #   ("start", win_start, None)        — "Start of retained history"; history mode, FIRST row
            #   ("empty", None, last_known_programme_or_None)  — nothing to show in this direction
            self.programme_rows: list[tuple[str, datetime | None, object]] = []

        def compose(self) -> ComposeResult:
            yield Header()
            with Container(id="layout"):
                yield ListView(id="channels")
                yield ListView(id="programmes")
                with VerticalScroll(id="detail"):
                    yield Static("Loading…", id="detail-body", markup=True)
            yield Footer()

        def _apply_layout(self, width: int) -> None:
            """Right-hand detail column on a wide terminal, bottom row
            otherwise. Only the class changes — the widgets never move, so
            selection, scroll position and focus survive a resize."""
            self.query_one("#layout").set_class(width >= self.WIDE_COLUMNS, "wide")

        def on_resize(self, event) -> None:
            self._apply_layout(event.size.width)

        def on_mount(self) -> None:
            self._apply_layout(self.size.width)
            self.title = f"epg-browser — {self.host}"
            self._sync_mode_subtitle()
            # Paint the shell before the first byte of network data exists —
            # first paint then costs imports only, not the ~1.2s lineup fetch.
            lv = self.query_one("#channels", ListView)
            lv.append(ListItem(Label("Loading channels…")))
            self.query_one("#detail-body", Static).update(f"Loading lineup from {self.host}…")
            self.query_one("#channels", ListView).focus()
            self.action_refresh()
            self.set_interval(self.REFRESH_SECS, self.action_refresh)
            self.set_interval(self.TICK_SECS, lambda: self._load_programmes(keep_selection=True))

        def action_refresh(self) -> None:
            """Kick off a lineup fetch on a thread worker and return
            immediately — never block the event loop. `_apply_lineup` (called
            back via call_from_thread) does the work this used to do inline."""
            self._fetch_lineup_worker()

        @work(exclusive=True, thread=True, group="lineup")
        def _fetch_lineup_worker(self) -> None:
            try:
                channels, programmes = fetch_lineup(self.host)
            except Exception as exc:  # noqa: BLE001 - surfaced to the UI, not crashed on
                self.call_from_thread(self._lineup_failed, str(exc))
                return
            # exclusive=True cancels the previous worker but a thread worker
            # keeps running to completion — this is what stops a slow fetch
            # a newer refresh superseded from clobbering the newer one.
            if get_current_worker().is_cancelled:
                return
            self.call_from_thread(self._apply_lineup, channels, programmes)

        def _lineup_failed(self, message: str) -> None:
            self.loading = False
            self.load_error = message
            self.notify(f"Failed to fetch lineup: {message}", severity="error")
            self.refresh_detail()

        def _apply_lineup(self, channels: dict[str, Channel], programmes: list[Programme]) -> None:
            """Runs on the event loop via call_from_thread, so this whole body
            is one loop callback — the 15s tick can never observe channels and
            programmes half-swapped."""
            self.channels, self.programmes = channels, programmes
            self.loading = False
            self.load_error = None
            lv = self.query_one("#channels", ListView)
            keep_index = lv.index
            lv.clear()
            now = datetime.now(timezone.utc)
            for tvg_id, chan in self.channels.items():
                current, _ = current_and_next(self.programmes, tvg_id, now)
                subtitle = current.title if current and current.title else self._gap_subtitle(tvg_id, now)
                num = f"{chan.number:>3}  " if chan.number is not None else ""
                lv.append(
                    ListItem(
                        Label(f"[dim]{num}[/dim]{escape(chan.name)}\n[dim]{' ' * len(num)}{escape(subtitle)}[/dim]"),
                        name=tvg_id,
                    )
                )
            if self.channels and self.selected_channel is None:
                self.selected_channel = next(iter(self.channels))
            if keep_index is not None and keep_index < len(lv):
                lv.index = keep_index
                # The highlighted row and the detail pane must name the same
                # channel — if the lineup reordered since the last fetch,
                # keep_index now lands on a different tvg_id than the one
                # that was selected before, so resync selected_channel to
                # what's actually highlighted rather than leaving it stale.
                self.selected_channel = list(self.channels.keys())[keep_index]
            self._load_programmes(keep_selection=True)

        @staticmethod
        def _floor_block(dt: datetime, minutes: int) -> datetime:
            floored_minute = (dt.minute // minutes) * minutes
            return dt.replace(minute=floored_minute, second=0, microsecond=0)

        def _load_programmes(self, keep_selection: bool = False) -> None:
            """(Re)populate the #programmes sidebar with one row per media
            item, each row's height snapped to the BLOCK_MINUTES grid (one
            line per boundary it crosses). Every item is rendered, on-air or
            off-air; nothing is summarized or capped, so a scheduling gap is
            exactly as long on screen as it is in reality.

            Two directions, toggled with `h`:

            - **Forward** (default) — the window is [now, end of known EPG].
              The now block is the first row and the list reads downward into
              the future, which is what an EPG is for: find the live edge, then
              scroll down to see what's coming.
            - **History** — the window is [now - HISTORY_DAYS, now], clamped to
              the oldest programme the guide still holds. Rows stay in
              chronological order, so the now block is the *last* row and the
              list is entered at the bottom: you scroll UP to go back in time.
              Travelling upward instead of downward is the point — it is the
              cue that the list is pointing the other way, and it matches how
              every scrollback reads.

            Both directions put the block containing `now` under the ▶ marker
            and select it on a fresh channel switch; it is simply at opposite
            ends of the list. On a periodic rebuild, whatever item the user had
            highlighted stays highlighted (by its title and real start time,
            not row index, since the list shifts every time a block rolls
            over or an item's row is regrouped)."""
            lv = self.query_one("#programmes", ListView)
            keep_kind = None
            keep_key = None
            if keep_selection and lv.index is not None and self.programme_rows and lv.index < len(self.programme_rows):
                keep_kind, keep_row_ts, keep_payload = self.programme_rows[lv.index]
                if keep_kind == "item":
                    keep_title = keep_payload[0].title if keep_payload[0] is not None else None
                    keep_key = (keep_title, keep_row_ts)
            lv.clear()
            self.programme_rows = []
            if self.selected_channel is None:
                self.refresh_detail()
                return
            now = datetime.now(timezone.utc)
            all_progs = programmes_for(self.programmes, self.selected_channel)
            if self.history:
                horizon = now - timedelta(days=self.HISTORY_DAYS)
                progs_win = [p for p in all_progs if p.stop > horizon and p.start < now]
            else:
                progs_win = [p for p in all_progs if p.stop > now]

            # Every slot row carries the channel number, not just the pane
            # header. The rows are what gets selected and copied out of the
            # terminal, and a copied block of times and titles that doesn't say
            # which of 64 channels it came from can't be pasted into an issue.
            selected = self.channels.get(self.selected_channel)
            chan_num = selected.number if selected else None
            chan_tag = f"[dim]ch{chan_num:<3}[/dim] " if chan_num is not None else ""
            # Rendered width of chan_tag, markup stripped — the title-elision
            # budget in _seg_text is sized off this, so it has to be what the
            # terminal draws, not the length of the markup string.
            chan_tag_w = len(f"ch{chan_num:<3} ") if chan_num is not None else 0

            if not progs_win:
                last = max(all_progs, key=lambda p: p.stop) if all_progs else None
                if self.history:
                    head = "[red]⚠ No EPG history retained[/red]"
                    detail = f"[dim]   nothing aired in the last {self.HISTORY_DAYS} days is still in the guide[/dim]"
                    if last is None:
                        detail = "[dim]   no programme data at all for this channel[/dim]"
                    label = f"{head}\n{detail}"
                elif last is None:
                    label = "[red]⚠ No EPG scheduled right now[/red]\n[dim]   no programme data at all for this channel[/dim]"
                else:
                    ago = _fmt_span((now - last.stop).total_seconds())
                    label = (
                        "[red]⚠ No EPG scheduled right now[/red]\n"
                        f"[dim]   Last EPG: {escape(last.title or '(untitled)')} — "
                        f"ended {_fmt_dt_sec(last.stop)} ({ago} ago)[/dim]"
                    )
                lv.append(ListItem(Label(label), name="empty"))
                self.programme_rows.append(("empty", None, last))
                lv.index = 0
                self.refresh_detail()
                return

            # The window both modes render, as real unrounded timestamps. One
            # end is always `now` — the difference is which end.
            if self.history:
                win_start = max(horizon, min(p.start for p in progs_win))
                end_bound = now
            else:
                win_start = now
                end_bound = progs_win[-1].stop  # where known EPG data actually ends

            # The block-clock column is a property of the render window, not
            # of any one row: derive its width from win_start/end_bound/
            # self.history alone, once, rather than by formatting every block
            # and taking a max after the fact. Otherwise a window that crosses
            # midnight renders a 5-char HH:MM column for today's rows and a
            # 9-char "Wed HH:MM" column for the rest, breaking the aligned
            # clock column #418 chose the left-gutter layout for.
            today = now.date()
            stamp_w = max(len(_fmt_dt(d, today=today, with_date=self.history)) for d in (win_start, end_bound))

            # Sweep progs_win into a flat list of (programme_or_None, start, stop)
            # segments covering [win_start, end_bound) with zero gaps between
            # them and real (unrounded) boundary timestamps — this is what lets
            # a single 15m block contain a mid-block changeover. Clamped to the
            # window at both ends: a show that runs past either edge must render
            # as content up to the edge, never as a phantom "off-air" gap.
            segments: list[tuple[Programme | None, datetime, datetime]] = []
            cursor = win_start
            for p in progs_win:
                if p.stop <= win_start:
                    continue
                if p.start > cursor:
                    segments.append((None, cursor, min(p.start, end_bound)))
                segments.append((p, max(p.start, win_start), min(p.stop, end_bound)))
                cursor = p.stop
                if cursor >= end_bound:
                    break

            def _title(seg: tuple[Programme | None, datetime, datetime]) -> str:
                return (seg[0].title or "(untitled)") if seg[0] else "— off-air —"

            # Usable row width, read off the live pane so the elision below
            # tracks a resized terminal instead of a number baked in here. Zero
            # before the first layout pass, hence the fallback.
            pane_w = self.query_one("#programmes", ListView).size.width or 46

            def _seg_text(seg: tuple[Programme | None, datetime, datetime], prefix_w: int) -> str:
                """Row text for one segment: the show, plus its season/episode
                when it has one. Without it a single-series channel renders as
                a screen of identical titles, and a row copied out of the list
                names the series but not the episode. Films and specials carry
                no episode-num and keep the bare title."""
                if seg[0] is None:
                    return "[red]— off-air —[/red]"
                p = seg[0]
                if p.season is not None and p.episode is not None:
                    suffix = f" - S{p.season:02d}E{p.episode:02d}"
                elif p.episode is not None:
                    suffix = f" - E{p.episode:02d}"
                else:
                    suffix = ""
                # The episode number is the part that must survive a narrow
                # pane. It sits at the end of the row, so left to itself it is
                # the first thing clipped off — elide the title instead, which
                # stays recognisable at half length while "S01E04" does not.
                title = _title(seg)
                budget = pane_w - prefix_w - len(suffix)
                if budget >= 4 and len(title) > budget:
                    title = title[: budget - 1] + "…"
                return escape(title) + (f"[dim]{escape(suffix)}[/dim]" if suffix else "")

            # History mode's bound row is the FIRST row, not the last: the list
            # reads oldest-at-top, so "this is as far back as the guide goes"
            # belongs above the oldest block, where scrolling up runs into it.
            if self.history:
                lv.append(
                    ListItem(
                        Label(
                            f"[yellow]⏶ Start of retained EPG: {_fmt_dt_sec(win_start, with_date=True)}[/yellow]\n"
                            "[dim]   nothing older than this is still in the guide[/dim]"
                        ),
                        name="start",
                    )
                )
                self.programme_rows.append(("start", win_start, None))

            select_idx = 0
            seg_i = 0
            cur = self._floor_block(win_start, self.BLOCK_MINUTES)
            step = timedelta(minutes=self.BLOCK_MINUTES)
            row_count = 0
            # prefix_w depends only on stamp_w now, not on any one block's
            # clock text, so it's computed once here rather than per block.
            prefix_w = 2 + chan_tag_w + self.GUTTER_W + stamp_w + 2

            # A row is one media item (or one gap span), not one block — but
            # the loop below still walks blocks, because that's what already
            # produces the right lines in the right order (a title line for
            # whichever segment opens the row, a bare-clock line for every
            # block boundary after it). This just groups those lines into
            # per-segment row buffers instead of emitting one ListItem per
            # block. `open_ident` tracks row ownership by the identity (`is`)
            # of the segments[] tuple the open row belongs to — two adjacent
            # gap segments can compare equal by value, so identity is what
            # keeps them from merging into one row.
            #
            # Each line is built with a `\x00` sentinel where its gutter
            # glyph goes — which glyph (┌ first, │ middle, └ last, or a blank
            # gutter for a one-line row) depends on the row's total line
            # count, which isn't known until the row closes. flush() resolves
            # every sentinel in one pass right before the row is rendered.
            row_open = False
            open_ident: object = None
            open_payload: tuple[Programme | None, datetime, datetime] | None = None
            open_lines: list[str] = []
            open_row_ts: datetime | None = None
            open_is_now = False

            def flush() -> None:
                nonlocal row_open, open_ident, open_payload, open_lines, open_row_ts, open_is_now, select_idx
                if not row_open:
                    return
                n = len(open_lines)
                if n == 1:
                    # A lone ┌ with nothing under it reads as broken — a
                    # one-line row gets a blank gutter instead, keeping the
                    # clock column aligned with every other row.
                    resolved = [open_lines[0].replace("\x00", " ", 1)]
                else:
                    resolved = []
                    for idx, line in enumerate(open_lines):
                        glyph = "┌" if idx == 0 else ("└" if idx == n - 1 else "│")
                        resolved.append(line.replace("\x00", glyph, 1))
                lv.append(ListItem(Label("\n".join(resolved)), name="item"))
                row_idx = len(self.programme_rows)
                self.programme_rows.append(("item", open_row_ts, open_payload))
                title = open_payload[0].title if open_payload is not None and open_payload[0] is not None else None
                if keep_kind == "item" and (title, open_row_ts) == keep_key:
                    select_idx = row_idx
                elif keep_kind not in ("item", "end", "start") and open_is_now:
                    select_idx = row_idx
                row_open = False
                open_ident = None
                open_payload = None
                open_lines = []
                open_row_ts = None
                open_is_now = False

            while cur < end_bound and row_count < self.BLOCK_SAFETY_CAP:
                block_end = cur + step
                overlapping = []
                i = seg_i
                while i < len(segments) and segments[i][1] < block_end and segments[i][2] > cur:
                    overlapping.append(segments[i])
                    i += 1
                while seg_i < len(segments) and segments[seg_i][2] <= block_end:
                    seg_i += 1
                # The ▶ marks the block containing `now` in both directions —
                # row 0 going forward, the last row going back. Testing for
                # `now` rather than row position is what makes that one rule.
                is_now = cur <= now < block_end
                marker = "▶" if is_now else " "
                time_str = _fmt_dt(cur, today=now.date(), with_date=self.history).rjust(stamp_w)
                # One line per programme this block touches, gutter glyph
                # left as a `\x00` sentinel for flush() to resolve. The title
                # is stated once, on a row's first line only; every line
                # after it in the same row carries just the block clock.
                # Which row a line joins is decided below: a block-clock line
                # continues the still-open row when it's the same segment
                # crossing a new boundary, and starts a fresh row otherwise
                # — and a fresh row is always that row's first line, so it
                # always carries the title.
                if not overlapping:
                    # No segment covers this block at all — a window-edge
                    # artifact (`cur` floors to before win_start on the first
                    # block), not a real gap; `segments` already carries real
                    # gaps as None-programme entries. One line, its own row,
                    # never merged with a neighbour.
                    flush()
                    row_open = True
                    open_ident = object()
                    open_payload = (None, cur, block_end)
                    open_lines = [f"{marker} {chan_tag}\x00 {time_str}  [red]— off-air —[/red]"]
                    open_row_ts = cur
                    open_is_now = is_now
                    flush()
                else:
                    for j, seg in enumerate(overlapping):
                        continuation = j == 0 and row_open and open_ident is seg
                        if j == 0:
                            clock = time_str
                        else:
                            clock = _fmt_dt(seg[1], today=now.date(), with_date=self.history).rjust(stamp_w)
                        if continuation:
                            # Same item as the still-open row, crossing into a
                            # new boundary mid-item — add a bare-clock line,
                            # not a new row, so the title isn't restated.
                            line = f"{marker} {chan_tag}\x00 {clock}"
                            open_lines.append(line)
                            if is_now:
                                open_is_now = True
                        else:
                            flush()
                            row_open = True
                            open_ident = seg
                            open_payload = seg
                            open_lines = [f"{marker} {chan_tag}\x00 {clock}  {_seg_text(seg, prefix_w)}"]
                            open_row_ts = seg[1]
                            open_is_now = is_now
                cur = block_end
                row_count += 1
            flush()

            # Forward mode's bound row closes the list. History mode already
            # opened with its own bound row and ends at `now`, which is not an
            # edge of the data — there is nothing to say down there.
            if not self.history:
                lv.append(
                    ListItem(
                        Label(
                            f"[red]⚠ Last EPG data: {_fmt_dt_sec(end_bound)}[/red]\n"
                            "[dim]   nothing scheduled after this[/dim]"
                        ),
                        name="end",
                    )
                )
                self.programme_rows.append(("end", end_bound, None))
                if keep_kind == "end":
                    select_idx = len(self.programme_rows) - 1
            elif keep_kind == "start":
                select_idx = 0

            lv.index = select_idx
            self.refresh_detail()

        @staticmethod
        def _programme_headline(p: Programme) -> str:
            """One self-describing line naming the item: show, season/episode,
            episode title. The labelled fields below it are still the full
            record — this exists so the first line of a copied detail pane
            already says which episode it is, instead of just the series."""
            bits = [escape(p.title or "(untitled)")]
            if p.season is not None and p.episode is not None:
                bits.append(f"S{p.season:02d}E{p.episode:02d}")
            elif p.episode is not None:
                bits.append(f"E{p.episode:02d}")
            if p.sub_title:
                bits.append(f"“{escape(p.sub_title)}”")
            return "  ·  ".join(bits)

        def _programme_fields(self, p: Programme) -> list[str]:
            def field(label: str, value: object) -> str:
                if value in (None, "", []):
                    return f"  {label}: [dim]∅[/dim]"
                return f"  {label}: {escape(str(value))}"

            return [
                field("title", p.title),
                field("sub-title", p.sub_title),
                field("desc", p.desc),
                field(
                    "season/episode",
                    f"S{p.season:02d}E{p.episode:02d}"
                    if p.season is not None and p.episode is not None
                    else None,
                ),
                field("categories", ", ".join(p.categories) if p.categories else None),
                field("rating", p.rating),
                field("star-rating", p.star_rating),
                field("icon", p.icon),
            ]

        def _gap_subtitle(self, tvg_id: str, now: datetime) -> str:
            gap = current_gap(self.programmes, tvg_id, now)
            if gap is None:
                return "(no programmes scheduled)"
            gap_start, gap_end = gap
            if gap_end is None:
                return f"(off-air {_fmt_span((now - gap_start).total_seconds())} — nothing scheduled)"
            return f"(off-air — next in {_fmt_span((gap_end - now).total_seconds())})"

        def _selected_row(self) -> tuple[str, datetime | None, object] | None:
            lv = self.query_one("#programmes", ListView)
            idx = lv.index
            if idx is None or not self.programme_rows or idx >= len(self.programme_rows):
                return None
            return self.programme_rows[idx]

        def on_list_view_highlighted(self, event: ListView.Highlighted) -> None:
            if event.list_view.id == "channels":
                if event.item is not None and event.item.name and event.item.name != self.selected_channel:
                    self.selected_channel = event.item.name
                    self._load_programmes()
                else:
                    self.refresh_detail()
            elif event.list_view.id == "programmes":
                self.refresh_detail()

        def action_focus_channels(self) -> None:
            self.query_one("#channels", ListView).focus()

        def action_focus_programmes(self) -> None:
            self.query_one("#programmes", ListView).focus()

        def action_toggle_history(self) -> None:
            """Flip the list between reading forward from now and reading back
            to the oldest retained programme. Selection is not carried across —
            the two windows only touch at `now`, so the honest landing spot in
            either direction is the now block, which is what a rebuild with
            keep_selection=False picks."""
            self.history = not self.history
            self._sync_mode_subtitle()
            self._load_programmes()
            self.query_one("#programmes", ListView).focus()

        def _sync_mode_subtitle(self) -> None:
            self.sub_title = (
                f"history — last {self.HISTORY_DAYS}d, now at the bottom, scroll up"
                if self.history
                else "live — now at the top, scroll down"
            )

        def refresh_detail(self) -> None:
            body = self.query_one("#detail-body", Static)
            if self.load_error is not None:
                body.update(
                    f"[red]Failed to fetch lineup from {escape(self.host)}[/red]\n"
                    f"[dim]{escape(self.load_error)}[/dim]\n\npress r to retry"
                )
                return
            if self.loading:
                body.update(f"Loading lineup from {escape(self.host)}…")
                return
            if self.selected_channel is None:
                body.update("No channel selected.")
                return
            chan = self.channels[self.selected_channel]
            chan_no = f"ch{chan.number}  " if chan.number is not None else ""
            lines = [
                f"[b]{chan_no}{escape(chan.name)}[/b]  ({escape(chan.tvg_id)})",
                f'stream: [link="{chan.stream_url}"]{escape(chan.stream_url)}[/link]',
                f'guide:  [link="{self.host}/xmltv.xml"]{escape(self.host)}/xmltv.xml[/link]',
                "",
            ]
            now = datetime.now(timezone.utc)
            lines.append(f"[b][yellow]now: {_fmt_dt_sec(now)} UTC[/yellow][/b]")
            lines.append("")

            row = self._selected_row()
            if row is None:
                lines.append("[dim]No programme selected.[/dim]")
            else:
                # The bound rows carry their timestamp in the middle slot and
                # nothing in the payload — reading `payload` here crashed on
                # None the moment the "Last EPG data" row was selected.
                kind, row_ts, payload = row
                if kind == "empty":
                    lines.append("[b]status: no live schedule[/b]")
                    if payload is not None:
                        lines.append("last known EPG entry:")
                        lines.extend(self._programme_fields(payload))
                    else:
                        lines.append("[dim]No EPG data for this channel at all.[/dim]")
                elif kind == "end":
                    lines.append(f"[b]End of known EPG data[/b]  ({_fmt_dt_sec(row_ts)})")
                    lines.append("[dim]Nothing has been generated past this point.[/dim]")
                elif kind == "start":
                    lines.append(f"[b]Start of retained EPG[/b]  ({_fmt_dt_sec(row_ts, with_date=True)})")
                    lines.append(
                        f"[dim]The guide holds nothing older. History mode reaches back at most "
                        f"{self.HISTORY_DAYS} days; the retention sweep may have cut it shorter.[/dim]"
                    )
                else:  # "item"
                    p, s, e = payload
                    if p is None:
                        lines.append("[b]status: off-air[/b]")
                        lines.append(f"[dim]— off-air {_fmt_dt_sec(s)} … {_fmt_dt_sec(e)} —[/dim]")
                    else:
                        # The row addresses the whole programme, not the
                        # window-clamped segment — a row whose first line was
                        # clamped to `now` must still show the film's real
                        # start, not the moment the guide started rendering.
                        lines.append(f"[b]Selected show[/b]  ({_fmt_dt_sec(p.start)} → {_fmt_dt_sec(p.stop)})")
                        lines.append(f"  [b]{self._programme_headline(p)}[/b]")
                        lines.extend(self._programme_fields(p))
            body.update("\n".join(lines))

        def action_open_vlc(self) -> None:
            if self.selected_channel is None:
                return
            chan = self.channels[self.selected_channel]
            subprocess.run(["open", "-a", "VLC", chan.stream_url])

        def action_open_artwork(self) -> None:
            url = None
            row = self._selected_row()
            if row is not None:
                kind, _, payload = row
                if kind == "item":
                    p = payload[0]
                    if p is not None and p.icon:
                        url = p.icon
                elif kind == "empty" and payload is not None and payload.icon:
                    url = payload.icon
            if url is None and self.selected_channel is not None:
                url = self.channels[self.selected_channel].logo
            if url is None:
                self.notify("No artwork URL for the selected show.", severity="warning")
                return
            subprocess.run(["open", url])

        def action_open_sublime(self) -> None:
            if self.selected_channel is None:
                return
            chan = self.channels[self.selected_channel]
            root = ET.Element("tv")
            channel_el = ET.SubElement(root, "channel", id=chan.tvg_id)
            ET.SubElement(channel_el, "display-name").text = chan.name
            for p in programmes_for(self.programmes, self.selected_channel):
                el = ET.SubElement(
                    root,
                    "programme",
                    start=p.start.strftime(XMLTV_DATETIME),
                    stop=p.stop.strftime(XMLTV_DATETIME),
                    channel=p.channel_id,
                )
                if p.title:
                    ET.SubElement(el, "title").text = p.title
                if p.desc:
                    ET.SubElement(el, "desc").text = p.desc
            out_dir = REPO_ROOT / "tmp" / "claude" / "epg-browser"
            out_dir.mkdir(parents=True, exist_ok=True)
            out_path = out_dir / f"{chan.tvg_id}.xml"
            ET.ElementTree(root).write(out_path, encoding="unicode", xml_declaration=True)
            subprocess.run(["subl", str(out_path)])

    return EpgBrowserApp(host)


def run_tui(host: str) -> None:
    build_app(host).run()


# --- entry point -------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", help="explicit station URL, e.g. http://station.example:8419")
    parser.add_argument("--local", action="store_true", help="target local dev (127.0.0.1:8409)")
    parser.add_argument("--prod", action="store_true", help="target prod (PROD_URL, the default)")
    sub = parser.add_subparsers(dest="command")

    sub.add_parser("channels", help="JSON: every channel + its current programme")

    p_channel = sub.add_parser("channel", help="JSON: full programme list for one channel")
    p_channel.add_argument("channel_id")

    p_current = sub.add_parser("current", help="JSON: single current programme for one channel")
    p_current.add_argument("channel_id")

    p_artwork = sub.add_parser("artwork-check", help="JSON: HEAD-check every icon URL in scope")
    p_artwork.add_argument("channel_id", nargs="?")

    args = parser.parse_args()
    host = resolve_host(args.host, args.local)

    if args.command == "channels":
        cmd_channels(host)
    elif args.command == "channel":
        cmd_channel(host, args.channel_id)
    elif args.command == "current":
        cmd_current(host, args.channel_id)
    elif args.command == "artwork-check":
        cmd_artwork_check(host, args.channel_id)
    else:
        run_tui(host)


if __name__ == "__main__":
    main()
