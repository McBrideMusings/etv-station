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
from datetime import datetime, timezone
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


def run_tui(host: str) -> None:
    from rich.markup import escape
    from textual.app import App, ComposeResult
    from textual.containers import Horizontal, VerticalScroll
    from textual.widgets import Footer, Header, ListItem, ListView, Label, Static

    class EpgBrowserApp(App):
        CSS = """
        Horizontal { height: 1fr; }
        #channels { width: 34; border-right: solid $accent; }
        #detail { padding: 0 1; }
        """
        BINDINGS = [
            ("r", "refresh", "Refresh"),
            ("v", "open_vlc", "Artwork in VLC"),
            ("s", "open_sublime", "XML in Sublime"),
        ]

        def __init__(self, host: str) -> None:
            super().__init__()
            self.host = host
            self.channels: dict[str, Channel] = {}
            self.programmes: list[Programme] = []
            self.selected_channel: str | None = None

        def compose(self) -> ComposeResult:
            yield Header()
            with Horizontal():
                yield ListView(id="channels")
                with VerticalScroll(id="detail"):
                    yield Static("Loading…", id="detail-body", markup=True)
            yield Footer()

        def on_mount(self) -> None:
            self.title = f"epg-browser — {self.host}"
            self.action_refresh()

        def action_refresh(self) -> None:
            self.channels, self.programmes = fetch_lineup(self.host)
            lv = self.query_one("#channels", ListView)
            lv.clear()
            now = datetime.now(timezone.utc)
            for tvg_id, chan in self.channels.items():
                current, _ = current_and_next(self.programmes, tvg_id, now)
                subtitle = current.title if current and current.title else "(no current programme)"
                lv.append(
                    ListItem(
                        Label(f"{escape(chan.name)}\n[dim]{escape(subtitle)}[/dim]"),
                        name=tvg_id,
                    )
                )
            if self.channels and self.selected_channel is None:
                self.selected_channel = next(iter(self.channels))
            self.refresh_detail()

        def on_list_view_highlighted(self, event: ListView.Highlighted) -> None:
            if event.item is not None and event.item.name:
                self.selected_channel = event.item.name
                self.refresh_detail()

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
            for p in programmes_for(self.programmes, self.selected_channel):
                title = escape(p.title or "(untitled)")
                span = f"{p.start.strftime('%H:%M')}–{p.stop.strftime('%H:%M')}"
                if p.icon:
                    lines.append(f'{span}  [link="{p.icon}"]{title}[/link]')
                else:
                    lines.append(f"{span}  {title}")
            body.update("\n".join(lines))

        def action_open_vlc(self) -> None:
            if self.selected_channel is None:
                return
            now = datetime.now(timezone.utc)
            current, _ = current_and_next(self.programmes, self.selected_channel, now)
            url = current.icon if current and current.icon else None
            if url is None:
                chan = self.channels[self.selected_channel]
                url = chan.logo
            if url is None:
                self.notify("No artwork URL for the current programme.", severity="warning")
                return
            subprocess.run(["open", "-a", "VLC", url])

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

    EpgBrowserApp(host).run()


# --- entry point -------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", help="explicit station URL, e.g. http://100.114.249.118:8419")
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
