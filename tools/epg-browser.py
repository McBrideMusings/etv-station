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
from datetime import datetime, timedelta, timezone
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
    r'(?:\s+tvg-name="(?P<tvg_name>[^"]*)")?'
    r'(?:\s+tvg-logo="(?P<tvg_logo>[^"]*)")?'
    r'(?:\s+group-title="(?P<group>[^"]*)")?'
    r",\s*(?P<display>.*)"
)


def parse_channels_m3u(text: str) -> dict[str, Channel]:
    channels: dict[str, Channel] = {}
    lines = [ln for ln in text.splitlines() if ln.strip()]
    i = 0
    while i < len(lines):
        line = lines[i]
        if line.startswith("#EXTINF"):
            m = EXTINF_RE.match(line)
            stream_url = lines[i + 1].strip() if i + 1 < len(lines) else ""
            if m and m.group("tvg_id"):
                channels[m.group("tvg_id")] = Channel(
                    tvg_id=m.group("tvg_id"),
                    name=m.group("tvg_name") or m.group("display") or m.group("tvg_id"),
                    logo=m.group("tvg_logo") or None,
                    stream_url=stream_url,
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


def _fmt_dt(dt: datetime) -> str:
    """Weekday-qualified so a two-day-old entry can't be misread as "later
    today" — a bare %H:%M is exactly what made the schedule gap in #292 look
    like an upcoming show instead of a stale listing days in the past."""
    return dt.strftime("%a %H:%M")


def _fmt_dt_sec(dt: datetime) -> str:
    """Same as _fmt_dt but with seconds — used for real programme-boundary
    timestamps (changeovers, EPG-data cutoffs), never for block labels."""
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
    from textual.app import App, ComposeResult
    from textual.containers import Horizontal, Vertical, VerticalScroll
    from textual.widgets import Footer, Header, ListItem, ListView, Label, Static

    class EpgBrowserApp(App):
        ENABLE_COMMAND_PALETTE = False
        # Two rows, not three columns. Three side-by-side panes needed 128
        # columns before anything clipped (34+1 channels, 40+1 programmes, 52
        # for the widest detail line — the full stream URL), which is wider
        # than a laptop terminal or a split pane. Stacking detail underneath
        # drops the minimum to 76 and hands the detail pane the full terminal
        # width, which is the pane that actually wanted it: URLs, a `desc`
        # paragraph, a comma-joined category list. Vertical space is the cheap
        # axis — the programme list never fills a terminal's height.
        CSS = """
        #top { height: 2fr; }
        #channels { width: 34; border-right: solid $accent; }
        #programmes { width: 1fr; min-width: 40; }
        #detail { height: 1fr; border-top: solid $accent; padding: 0 1; }
        """
        BINDINGS = [
            ("r", "refresh", "Refresh"),
            ("v", "open_vlc", "Stream in VLC"),
            ("a", "open_artwork", "Artwork preview"),
            ("s", "open_sublime", "XML in Sublime"),
            ("left", "focus_channels", "◀ Channels"),
            ("right", "focus_programmes", "Shows ▶"),
            ("q", "quit", "Quit"),
            ("escape", "quit", "Quit"),
        ]

        REFRESH_SECS = 60  # re-fetch lineup from the station this often
        TICK_SECS = 15  # rebuild the block list (cheap, no fetch) this often —
        # this is what makes the top block roll from e.g. 15:45 to 16:00 live
        BLOCK_MINUTES = 15  # width of one row in the shows sidebar
        BLOCK_SAFETY_CAP = 2000  # ~20 days of blocks — a runaway-loop backstop, not a UX truncation

        def __init__(self, host: str) -> None:
            super().__init__()
            self.host = host
            self.channels: dict[str, Channel] = {}
            self.programmes: list[Programme] = []
            self.selected_channel: str | None = None
            # Parallel to #programmes items. Each entry is either:
            #   ("block", block_start, segments)  — segments: list[(Programme|None, start, stop)]
            #   ("end", end_bound, None)          — the "Last EPG data: ..." closing row
            #   ("empty", None, last_known_programme_or_None)  — nothing live/future at all
            self.programme_rows: list[tuple[str, datetime | None, object]] = []

        def compose(self) -> ComposeResult:
            yield Header()
            with Vertical():
                with Horizontal(id="top"):
                    yield ListView(id="channels")
                    yield ListView(id="programmes")
                with VerticalScroll(id="detail"):
                    yield Static("Loading…", id="detail-body", markup=True)
            yield Footer()

        def on_mount(self) -> None:
            self.title = f"epg-browser — {self.host}"
            self.action_refresh()
            self.query_one("#channels", ListView).focus()
            self.set_interval(self.REFRESH_SECS, self.action_refresh)
            self.set_interval(self.TICK_SECS, lambda: self._load_programmes(keep_selection=True))

        def action_refresh(self) -> None:
            self.channels, self.programmes = fetch_lineup(self.host)
            lv = self.query_one("#channels", ListView)
            keep_index = lv.index
            lv.clear()
            now = datetime.now(timezone.utc)
            for tvg_id, chan in self.channels.items():
                current, _ = current_and_next(self.programmes, tvg_id, now)
                subtitle = current.title if current and current.title else self._gap_subtitle(tvg_id, now)
                lv.append(
                    ListItem(
                        Label(f"{escape(chan.name)}\n[dim]{escape(subtitle)}[/dim]"),
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
            """(Re)populate the #programmes sidebar as fixed BLOCK_MINUTES-wide
            rows, clock-aligned, starting at the block containing `now` and
            reading forward only — an EPG never scrolls back through history to
            find the live edge. Every block is rendered, on-air or off-air, all
            the way to the real end of known EPG data; nothing is summarized or
            capped, so a scheduling gap is exactly as long on screen as it is
            in reality. On a fresh channel switch the top (now) block is
            selected; on a periodic rebuild, whatever block the user had
            highlighted stays highlighted (by its real start time, not row
            index, since the list shifts every time the top block rolls over)."""
            lv = self.query_one("#programmes", ListView)
            keep_kind = None
            keep_block_start = None
            if keep_selection and lv.index is not None and self.programme_rows and lv.index < len(self.programme_rows):
                keep_kind, keep_block_start, _ = self.programme_rows[lv.index]
            lv.clear()
            self.programme_rows = []
            if self.selected_channel is None:
                self.refresh_detail()
                return
            now = datetime.now(timezone.utc)
            all_progs = programmes_for(self.programmes, self.selected_channel)
            progs_fwd = [p for p in all_progs if p.stop > now]

            if not progs_fwd:
                last = max(all_progs, key=lambda p: p.stop) if all_progs else None
                if last is None:
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

            block_start = self._floor_block(now, self.BLOCK_MINUTES)
            end_bound = progs_fwd[-1].stop  # real timestamp where known EPG data actually ends

            # Sweep progs_fwd into a flat list of (programme_or_None, start, stop)
            # segments covering [now, end_bound) with zero gaps between them and
            # real (unrounded) boundary timestamps — this is what lets a single
            # 15m block contain a mid-block changeover. Anchored at `now`, not
            # `block_start`: a show that already finished airing before `now`
            # but whose slot falls inside the still-visible current block must
            # never render as a phantom "off-air" gap — nothing before `now`
            # is real content or a real gap, it's just history, and history
            # doesn't get shown here at all.
            segments: list[tuple[Programme | None, datetime, datetime]] = []
            cursor = now
            for p in progs_fwd:
                if p.start > cursor:
                    segments.append((None, cursor, min(p.start, end_bound)))
                segments.append((p, max(p.start, now), p.stop))
                cursor = p.stop
                if cursor >= end_bound:
                    break

            def _title(seg: tuple[Programme | None, datetime, datetime]) -> str:
                return (seg[0].title or "(untitled)") if seg[0] else "— off-air —"

            select_idx = 0
            seg_i = 0
            cur = block_start
            step = timedelta(minutes=self.BLOCK_MINUTES)
            row_count = 0
            while cur < end_bound and row_count < self.BLOCK_SAFETY_CAP:
                block_end = cur + step
                overlapping = []
                i = seg_i
                while i < len(segments) and segments[i][1] < block_end and segments[i][2] > cur:
                    overlapping.append(segments[i])
                    i += 1
                while seg_i < len(segments) and segments[seg_i][2] <= block_end:
                    seg_i += 1
                is_first = row_count == 0
                marker = "▶" if is_first else " "
                # One line per block, always — no separate CHANGEOVER row. A
                # block a single show fully occupies just shows its title; a
                # block where a boundary falls shows "A → B" inline. The exact
                # boundary timestamp(s) and full fields for both shows live in
                # the detail pane only, not here.
                if len(overlapping) <= 1:
                    text = escape(_title(overlapping[0])) if overlapping else "[red]— off-air —[/red]"
                else:
                    text = escape(" → ".join(_title(seg) for seg in overlapping))
                label = f"{marker} {_fmt_dt(cur)}  {text}"
                lv.append(ListItem(Label(label), name="block"))
                self.programme_rows.append(("block", cur, overlapping))
                if keep_kind == "block" and cur == keep_block_start:
                    select_idx = row_count
                elif keep_kind != "block" and keep_kind != "end" and is_first:
                    select_idx = row_count
                cur = block_end
                row_count += 1

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

            lv.index = select_idx
            self.refresh_detail()

        def _programme_fields(self, p: Programme) -> list[str]:
            def field(label: str, value: object) -> str:
                if value in (None, "", []):
                    return f"  {label}: [dim]∅[/dim]"
                return f"  {label}: {escape(str(value))}"

            return [
                field("title", p.title),
                field("sub-title", p.sub_title),
                field("desc", p.desc),
                field("season/episode", f"S{p.season}E{p.episode}" if p.season and p.episode else None),
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

        def refresh_detail(self) -> None:
            body = self.query_one("#detail-body", Static)
            if self.selected_channel is None:
                body.update("No channel selected.")
                return
            chan = self.channels[self.selected_channel]
            lines = [
                f"[b]{escape(chan.name)}[/b]  ({escape(chan.tvg_id)})",
                f'stream: [link="{chan.stream_url}"]{escape(chan.stream_url)}[/link]',
                f'guide:  [link="{self.host}/xmltv.xml"]{escape(self.host)}/xmltv.xml[/link]',
                "",
            ]
            now = datetime.now(timezone.utc)
            lines.append(f"[b][yellow]now: {_fmt_dt_sec(now)} UTC[/yellow][/b]")
            lines.append("")

            row = self._selected_row()
            if row is None:
                lines.append("[dim]No block selected.[/dim]")
            else:
                kind, _, payload = row
                if kind == "empty":
                    lines.append("[b]status: no live schedule[/b]")
                    if payload is not None:
                        lines.append("last known EPG entry:")
                        lines.extend(self._programme_fields(payload))
                    else:
                        lines.append("[dim]No EPG data for this channel at all.[/dim]")
                elif kind == "end":
                    lines.append(f"[b]End of known EPG data[/b]  ({_fmt_dt_sec(payload)})")
                    lines.append("[dim]Nothing has been generated past this point.[/dim]")
                else:  # "block"
                    segments: list[tuple[Programme | None, datetime, datetime]] = payload
                    if len(segments) == 1 and segments[0][0] is None:
                        lines.append("[b]status: off-air[/b] — no programme scheduled in this block")
                    else:
                        for i, (p, s, e) in enumerate(segments):
                            if p is None:
                                lines.append(f"[dim]— off-air {_fmt_dt_sec(s)} … {_fmt_dt_sec(e)} —[/dim]")
                                lines.append("")
                                continue
                            heading = "Selected show" if len(segments) == 1 else ("Ends this block" if i == 0 else "Begins this block")
                            lines.append(f"[b]{heading}[/b]  ({_fmt_dt_sec(s)} → {_fmt_dt_sec(e)})")
                            lines.extend(self._programme_fields(p))
                            lines.append("")
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
                if kind == "block":
                    for p, _, _ in payload:
                        if p is not None and p.icon:
                            url = p.icon
                            break
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
