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
import tempfile
import time
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


# --- Audit screen helpers ----------------------------------------------------
#
# Pure functions with no Textual dependency, backing the AuditScreen defined
# inside build_app below — kept free-standing so they run on a plain thread
# worker with nothing but subprocess and json, and so a test could call them
# with no App instance at all.


def find_etv_overlay_binary() -> Path | None:
    """Locate the `etv-overlay` binary via `cargo metadata`'s own
    `target_directory` — the only way to find it that also honors a swarm
    worktree's shared `.cargo/config.toml` target-dir override (see
    CLAUDE.local.md's "Swarm worktrees share one cargo target dir"). Falls
    back to `<repo>/target` if `cargo metadata` fails. Prefers a release
    build; returns None if neither profile has been built."""
    target_dir: Path | None = None
    try:
        result = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=15,
        )
        if result.returncode == 0:
            target_dir = Path(json.loads(result.stdout)["target_directory"])
    except Exception:  # noqa: BLE001 - falls back to the default below
        target_dir = None
    if target_dir is None:
        target_dir = REPO_ROOT / "target"
    for profile in ("release", "debug"):
        candidate = target_dir / profile / "etv-overlay"
        if candidate.is_file():
            return candidate
    return None


def fetch_channel_number_map() -> dict[int, str]:
    """Run `tools/audit-report.sh --list --format json` and return
    {dial number: channel folder name}. Raises RuntimeError, with a message
    fit to show on screen, on any failure — a non-zero exit, or stdout that
    isn't the JSON array the binary's `--audit --list --format json` prints."""
    script = REPO_ROOT / "tools" / "audit-report.sh"
    result = subprocess.run(
        [str(script), "--list", "--format", "json"],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or f"audit-report.sh --list exited {result.returncode}")
    try:
        listing = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"audit-report.sh --list did not print JSON: {exc}") from exc
    out: dict[int, str] = {}
    for entry in listing:
        try:
            out[int(entry["number"])] = entry["name"]
        except (KeyError, TypeError, ValueError):
            continue
    return out


def fetch_audit_report(channel_name: str, next_n: int = 100) -> dict:
    """Run `tools/audit-report.sh <channel_name> --next N --format json` and
    return the parsed report. Raises RuntimeError, with a message fit to show
    on screen, on any failure."""
    script = REPO_ROOT / "tools" / "audit-report.sh"
    result = subprocess.run(
        [str(script), channel_name, "--next", str(next_n), "--format", "json"],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or f"audit-report.sh {channel_name} exited {result.returncode}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"audit-report.sh {channel_name} did not print JSON: {exc}") from exc


def find_matching_audit_item(report: dict, start: datetime) -> dict | None:
    """The report item whose `start` names the same instant as `start` — the
    EPG's tz-aware Python datetime and the report's RFC3339 text are two
    different wire formats for the same clock, so this compares instants
    (both converted to UTC), never the raw strings.

    Compared to whole-second resolution, not exact equality: XMLTV's
    datetime format (XMLTV_DATETIME above) can only express whole seconds,
    while the audit report's RFC3339 `start` keeps microseconds
    (crates/etv-station/src/audit_report.rs's `rfc3339`). The two can name
    the same instant — '2026-09-04T01:26:04.678777Z' and
    '2026-09-04T01:26:04+00:00' — and never compare equal at full
    precision, so the whole second is the real resolution of this match,
    not a tolerance window."""
    target = start.astimezone(timezone.utc).replace(microsecond=0)
    for item in report.get("items", []) if isinstance(report, dict) else []:
        raw = item.get("start") if isinstance(item, dict) else None
        if not raw:
            continue
        try:
            item_start = datetime.fromisoformat(raw.replace("Z", "+00:00"))
        except ValueError:
            continue
        if item_start.astimezone(timezone.utc).replace(microsecond=0) == target:
            return item
    return None


def render_classification(audit: object) -> str:
    """The `select`-stage record's `detail.pool/source/take/from/expr`,
    labelling the item by its pool. `audit` is `metadata.audit` verbatim from
    the station (see split_audit's doc comment in
    crates/etv-station/src/audit_report.rs) — not guaranteed to be a list."""
    if not isinstance(audit, list):
        return f"the audit trail is malformed (not a list): {audit!r}"
    if not audit:
        return "unclassified — nothing wrote an audit record for this item"
    select_record = next(
        (r for r in audit if isinstance(r, dict) and r.get("stage") == "select"),
        None,
    )
    if select_record is None:
        return "no 'select' stage record in the audit trail — nothing classified this item"
    detail = select_record.get("detail")
    detail = detail if isinstance(detail, dict) else {}
    lines = [f"pool: {detail.get('pool', '∅')}"]
    for key in ("source", "take", "from", "expr"):
        lines.append(f"{key}: {detail.get(key, '∅')}")
    return "\n".join(lines)


def render_audit_trail(audit: object) -> str:
    """Every record in `audit`, in order — `stage`/`by`/`verdict` then each
    `detail` key indented, generically: no key name is hard-coded here, so an
    unknown `detail` key renders exactly like a known one."""
    if not isinstance(audit, list):
        return f"the audit trail is malformed (not a list): {audit!r}"
    if not audit:
        return "unclassified — nothing wrote an audit record for this item"
    blocks = []
    for record in audit:
        if not isinstance(record, dict):
            blocks.append(f"(malformed audit record: {record!r})")
            continue
        stage = record.get("stage", "∅")
        by = record.get("by", "∅")
        verdict = record.get("verdict", "∅")
        lines = [f"[{stage}] {by}: {verdict}"]
        detail = record.get("detail")
        if isinstance(detail, dict):
            for key, value in detail.items():
                lines.append(f"    {key}: {value}")
        blocks.append("\n".join(lines))
    return "\n\n".join(blocks)


def rewrite_script_path(script: str) -> tuple[str, str | None]:
    """A `script` of `/config/<rest>` names a path inside the station
    container, mounted from this checkout's `deploy/appdata/`. Rewrite it to
    that local path and return (local_path, None) when the file is actually
    there, or (attempted_local_path, error_message) when it is not — the
    caller shows both paths and skips the invocation rather than failing."""
    if script.startswith("/config/"):
        local = REPO_ROOT / "deploy" / "appdata" / script[len("/config/"):]
    else:
        local = Path(script)
    # Absolute, always. The spec this path is written into lands in a scratch
    # directory, not beside whatever the path was originally relative to, so a
    # relative path that resolves here resolves to nothing once `etv-overlay`
    # reads it back from there. A station-written spec is absolute already
    # (`ChannelOverlays` is loaded "with every path absolute", see
    # crates/etv-station/src/config/overlay.rs), so this only catches a
    # hand-written or fixture spec — which is exactly the case that hit it.
    local = local.resolve()
    if local.exists():
        return str(local), None
    return str(local), f"container path {script!r} does not resolve to a local file"


def run_overlay_dump_text(overlay_spec: dict, programme: "Programme", next_title: str | None) -> dict:
    """Write `overlay_spec` to a scratch file (rewriting a `/config/` script
    path to this checkout's `deploy/appdata/` first), invoke `etv-overlay
    dump-text` against it, and return {"ok": bool, "text": str} — this never
    raises; every failure mode is turned into text for the Overlay section to
    show, per the item's acceptance bullets."""
    binary = find_etv_overlay_binary()
    if binary is None:
        return {
            "ok": False,
            "text": "etv-overlay binary not found — build it first: cargo build -p etv-overlay",
        }

    spec = dict(overlay_spec)
    script = spec.get("script")
    if script:
        local_path, err = rewrite_script_path(script)
        if err is not None:
            return {
                "ok": False,
                "text": (
                    "script does not resolve locally — skipping dump-text\n"
                    f"  container path: {script}\n"
                    f"  local path:     {local_path}"
                ),
            }
        spec["script"] = local_path

    out_dir = REPO_ROOT / "tmp" / "claude" / "epg-browser"
    out_dir.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(dir=out_dir, prefix="overlay-spec-", suffix=".json")
    spec_path = Path(tmp_name)
    with os.fdopen(fd, "w") as f:
        json.dump(spec, f)

    duration = max(0.0, (programme.stop - programme.start).total_seconds())
    args = [
        str(binary),
        "dump-text",
        "--spec", str(spec_path),
        "--title", programme.title or "",
        "--sub-title", programme.sub_title or "",
        "--description", programme.desc or "",
        "--next-title", next_title or "",
        "--duration", str(duration),
    ]
    try:
        result = subprocess.run(args, capture_output=True, text=True, timeout=30)
    except Exception as exc:  # noqa: BLE001 - surfaced to the Overlay section, not crashed on
        return {"ok": False, "text": f"failed to run etv-overlay dump-text: {exc}"}
    finally:
        spec_path.unlink(missing_ok=True)

    if result.returncode != 0:
        return {
            "ok": False,
            "text": f"etv-overlay dump-text exited {result.returncode}\n{result.stderr.strip()}",
        }
    try:
        parsed = json.loads(result.stdout)
    except json.JSONDecodeError:
        return {"ok": False, "text": f"etv-overlay dump-text printed non-JSON output:\n{result.stdout.strip()}"}
    texts = parsed.get("texts", [])
    if not texts:
        return {"ok": True, "text": "(no text layers drawn over this run)"}
    lines = [
        f"[{t.get('first_at', 0):.1f}s–{t.get('last_at', 0):.1f}s] {t.get('content', '')!r}"
        for t in texts
    ]
    return {"ok": True, "text": "\n".join(lines)}


# --- Textual TUI -------------------------------------------------------------


def build_app(host: str):
    """Construct the TUI without running it. Split from run_tui so
    tools/epg-layout-check.py can drive the same app under Textual's headless
    test driver and assert the layout still fits a narrow terminal."""
    from rich.markup import escape
    from textual import work
    from textual.app import App, ComposeResult
    from textual.containers import Container, VerticalScroll
    from textual.screen import Screen
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
            ("d", "open_audit", "Audit"),
            ("left", "focus_channels", "◀ Channels"),
            ("right", "focus_programmes", "Shows ▶"),
            ("q", "quit", "Quit"),
            ("escape", "quit", "Quit"),
        ]

        REFRESH_SECS = 60  # re-fetch lineup from the station this often
        TICK_SECS = 15  # rebuild the item list (cheap, no fetch) this often —
        # this is what makes the top row's clock roll from e.g. 15:45 to 16:00 live
        # A terminal emits a resize event per intermediate width while a pane
        # divider is dragged — dozens per drag — and each _apply_layout costs
        # 55ms (Textual's own per-resize floor) plus ~110ms more when the
        # resize crosses the WIDE_COLUMNS threshold (#424). on_resize defers
        # to _flush_resize instead of relaying out on every event, so a drag
        # produces one relayout when the width settles. Cost: a resize that
        # settles exactly on WIDE_COLUMNS shows the previous layout for this
        # long. 0.1s is above a terminal's inter-event gap during a drag and
        # below the perceptual threshold for a settled resize.
        RESIZE_DEBOUNCE_SECS = 0.1
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
            # Pending single-shot debounce timer for on_resize/_flush_resize
            # (see RESIZE_DEBOUNCE_SECS), or None when nothing is pending.
            self._resize_timer = None
            self._pending_width: int | None = None
            # AuditScreen's session-scoped caches. None until the first
            # AuditScreen open fetches it; a failed fetch leaves it None so
            # the next open retries rather than pinning the failure forever.
            self.channel_name_by_number: dict[int, str] | None = None
            # channel folder name -> parsed `--audit --format json` report.
            self.audit_cache: dict[str, dict] = {}

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
            wide = width >= self.WIDE_COLUMNS
            layout = self.query_one("#layout")
            if layout.has_class("wide") == wide:
                return
            layout.set_class(wide, "wide")

        def on_resize(self, event) -> None:
            # Record the latest width and (re)arm a single-shot timer instead
            # of relaying out immediately — see RESIZE_DEBOUNCE_SECS.
            self._pending_width = event.size.width
            if self._resize_timer is not None:
                self._resize_timer.stop()
            self._resize_timer = self.set_timer(self.RESIZE_DEBOUNCE_SECS, self._flush_resize)

        def _flush_resize(self) -> None:
            self._resize_timer = None
            if self._pending_width is not None:
                self._apply_layout(self._pending_width)

        async def _shutdown(self) -> None:
            # Cancel here, not in on_unmount: App._shutdown() closes every
            # screen (removing #layout's children) BEFORE it dispatches the
            # Unmount event, so a timer still pending at that point already
            # has time to fire mid-teardown and crash _apply_layout's
            # query_one("#layout") against a screen with no children left.
            # Cancelling at the top of _shutdown, before any of that runs,
            # is the only point that is actually before the race.
            if self._resize_timer is not None:
                self._resize_timer.stop()
                self._resize_timer = None
            await super()._shutdown()

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
            # exclusive=True cancels the previous worker but a thread worker
            # keeps running to completion — this is what stops a slow fetch
            # a newer refresh superseded from clobbering the newer one.
            # A superseded worker should report nothing at all — neither its
            # result nor its error.
            worker = get_current_worker()
            try:
                channels, programmes = fetch_lineup(self.host)
            except Exception as exc:  # noqa: BLE001 - surfaced to the UI, not crashed on
                if worker.is_cancelled:
                    return
                self.call_from_thread(self._lineup_failed, str(exc))
                return
            if worker.is_cancelled:
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

        def action_open_audit(self) -> None:
            row = self._selected_row()
            if row is None:
                self.notify("No programme selected.", severity="warning")
                return
            kind, _, payload = row
            programme = payload[0] if kind == "item" else None
            if kind != "item" or programme is None:
                self.notify(
                    "Select an on-air programme to audit — this row has nothing to audit.",
                    severity="warning",
                )
                return
            if self.selected_channel is None:
                return
            chan = self.channels[self.selected_channel]
            self.push_screen(AuditScreen(chan, programme, self._next_programme_title(programme)))

        def _next_programme_title(self, programme: Programme) -> str | None:
            """The title of the next real EPG entry after `programme` on the
            current channel — what `etv-overlay dump-text --next-title`
            wants, not the next visual row (which may be an off-air gap)."""
            if self.selected_channel is None:
                return None
            chan_progs = programmes_for(self.programmes, self.selected_channel)
            upcoming = [p for p in chan_progs if p.start > programme.start]
            return upcoming[0].title if upcoming else None

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

    class AuditScreen(Screen):
        """Full-screen audit of one highlighted programme: what it is, why the
        plugin picked it, and what its overlay draws — pushed by `d` on the
        programme list, closed by `escape`. `r` here refreshes the audit and
        overlay sections only; it does not fall through to the app's own `r`
        (lineup refresh) because a screen binding shadows the app's for the
        same key while this screen is on top."""

        BINDINGS = [
            ("escape", "dismiss", "Close"),
            ("r", "do_refresh", "Refresh audit"),
        ]

        CSS = """
        AuditScreen VerticalScroll { padding: 1 2; }
        AuditScreen .heading { text-style: bold; margin-top: 1; }
        """

        SECTION_IDS = ("audit-classification", "audit-trail", "audit-overlay")

        def __init__(self, chan: Channel, programme: Programme, next_title: str | None) -> None:
            super().__init__()
            self.chan = chan
            self.programme = programme
            self.next_title = next_title

        def compose(self) -> ComposeResult:
            yield Header()
            with VerticalScroll():
                yield Label("Programme", classes="heading")
                yield Static(self._programme_text(), id="audit-programme", markup=False)
                yield Label("Classification", classes="heading")
                yield Static("Loading…", id="audit-classification", markup=False)
                yield Label("Audit trail", classes="heading")
                yield Static("Loading…", id="audit-trail", markup=False)
                yield Label("Overlay", classes="heading")
                yield Static("Loading…", id="audit-overlay", markup=False)
            yield Footer()

        def _programme_text(self) -> str:
            p = self.programme
            window = f"{_fmt_dt_sec(p.start, with_date=True)} → {_fmt_dt_sec(p.stop, with_date=True)}"
            chan_label = f"ch{self.chan.number}  {self.chan.name}" if self.chan.number is not None else self.chan.name
            return "\n".join(
                [
                    f"title: {p.title or '(untitled)'}",
                    f"sub-title: {p.sub_title or '∅'}",
                    f"window: {window}",
                    f"channel: {chan_label}",
                ]
            )

        def action_dismiss(self) -> None:
            self.dismiss()

        def on_mount(self) -> None:
            self._load()

        def action_do_refresh(self) -> None:
            app: EpgBrowserApp = self.app  # type: ignore[assignment]
            name = (
                app.channel_name_by_number.get(self.chan.number)
                if app.channel_name_by_number is not None and self.chan.number is not None
                else None
            )
            if name is not None:
                app.audit_cache.pop(name, None)
            self._load()

        def _load(self) -> None:
            for section_id in self.SECTION_IDS:
                self.query_one(f"#{section_id}", Static).update("Loading…")
            self._load_worker()

        def _apply_message(self, message: str) -> None:
            for section_id in self.SECTION_IDS:
                self.query_one(f"#{section_id}", Static).update(message)

        def _apply_result(self, classification: str, trail: str, overlay: str) -> None:
            self.query_one("#audit-classification", Static).update(classification)
            self.query_one("#audit-trail", Static).update(trail)
            self.query_one("#audit-overlay", Static).update(overlay)

        @work(thread=True, exclusive=True, group="audit-screen")
        def _load_worker(self) -> None:
            app: EpgBrowserApp = self.app  # type: ignore[assignment]
            worker = get_current_worker()
            number = self.chan.number
            if number is None:
                app.call_from_thread(
                    self._apply_message,
                    "This channel has no dial number to resolve against the audit tool.",
                )
                return

            if app.channel_name_by_number is None:
                try:
                    channel_map = fetch_channel_number_map()
                except RuntimeError as exc:
                    if worker.is_cancelled:
                        return
                    app.call_from_thread(
                        self._apply_message,
                        f"Could not fetch the channel number→name map: {exc}",
                    )
                    return
                if worker.is_cancelled:
                    return
                app.channel_name_by_number = channel_map

            name = app.channel_name_by_number.get(number)
            if name is None:
                app.call_from_thread(
                    self._apply_message,
                    f"Channel number {number} has no entry in the audit tool's channel listing "
                    f"({len(app.channel_name_by_number)} known).",
                )
                return

            report = app.audit_cache.get(name)
            if report is None:
                try:
                    report = fetch_audit_report(name)
                except RuntimeError as exc:
                    if worker.is_cancelled:
                        return
                    app.call_from_thread(
                        self._apply_message,
                        f"Could not fetch the audit report for {name!r}: {exc}",
                    )
                    return
                if worker.is_cancelled:
                    return
                app.audit_cache[name] = report

            item = find_matching_audit_item(report, self.programme.start)
            if worker.is_cancelled:
                return
            if item is None:
                app.call_from_thread(
                    self._apply_message,
                    "No audit item matches this programme's start time — "
                    "the schedule moved under the guide.",
                )
                return

            raw_audit = item.get("audit", []) if isinstance(item, dict) else []
            classification_text = render_classification(raw_audit)
            trail_text = render_audit_trail(raw_audit)

            overlay_spec = item.get("overlay_spec") if isinstance(item, dict) else None
            if overlay_spec is None:
                overlay_text = "this channel draws no overlay here"
            else:
                overlay_result = run_overlay_dump_text(overlay_spec, self.programme, self.next_title)
                overlay_text = overlay_result["text"]

            if worker.is_cancelled:
                return
            app.call_from_thread(self._apply_result, classification_text, trail_text, overlay_text)

    return EpgBrowserApp(host)


class _TimingRecorder:
    """Opt-in event timer for a running EpgBrowserApp session. Built and wired
    in only when --timing/ETV_EPG_TIMING is set (see install_timing and
    run_tui below) — nothing here runs on the hot path otherwise.

    Records every (elapsed, kind, name, extra) event handed to it, writes a
    line immediately for anything at or above `threshold_secs` (default
    50ms) so a slow session yields a short file rather than a full
    transcript, and keeps running per-(kind, name) totals for the exit
    summary."""

    def __init__(self, path: str, threshold_secs: float = 0.05) -> None:
        self.threshold_secs = threshold_secs
        self._fh = open(path, "a", buffering=1)  # line-buffered: a slow
        # session's lines land in the file even if the process is later killed
        self._events: list[tuple[float, str, str, str]] = []
        # (kind, name) -> [count: int, total_secs: float] — a two-slot list, not
        # a uniform one, so it is annotated as a bare list rather than
        # list[float], which the int count would make untrue.
        self._totals: dict[tuple[str, str], list] = {}
        self._fh.write(
            f"# epg-browser timing started {datetime.now(timezone.utc).isoformat()} "
            f"threshold={threshold_secs * 1000:.0f}ms\n"
        )

    def record(self, elapsed_secs: float, kind: str, name: str, extra: str = "") -> None:
        self._events.append((elapsed_secs, kind, name, extra))
        totals_key = (kind, name)
        bucket = self._totals.setdefault(totals_key, [0, 0.0])
        bucket[0] += 1
        bucket[1] += elapsed_secs
        if elapsed_secs >= self.threshold_secs:
            self._fh.write(f"{elapsed_secs * 1000:8.1f}ms  {kind:8s} {name:30s}{extra}\n")

    def write_summary(self) -> None:
        """Called once on exit (normal quit or exception) so even a killed
        or short session leaves a usable summary."""
        try:
            self._fh.write("\n# --- slowest 20 events ---\n")
            slowest = sorted(self._events, key=lambda e: e[0], reverse=True)[:20]
            for elapsed_secs, kind, name, extra in slowest:
                self._fh.write(f"{elapsed_secs * 1000:8.1f}ms  {kind:8s} {name:30s}{extra}\n")
            self._fh.write("\n# --- totals per message type ---\n")
            by_total = sorted(self._totals.items(), key=lambda kv: kv[1][1], reverse=True)
            for (kind, name), (count, total_secs) in by_total:
                avg_ms = (total_secs / count) * 1000 if count else 0.0
                self._fh.write(
                    f"{kind:8s} {name:30s} count={count:5d} "
                    f"total={total_secs * 1000:9.1f}ms avg={avg_ms:7.2f}ms\n"
                )
        finally:
            self._fh.close()


def install_timing(recorder: _TimingRecorder) -> None:
    """Monkey-patch the two Textual choke points every event passes through:
    MessagePump._dispatch_message (every message dispatched to the app or any
    widget) and App._display (every screen repaint). Only called when a
    timing path is set — textual is imported here, not at module scope, so
    an unpatched run never even defines these wrappers."""
    from textual.app import App
    from textual.message_pump import MessagePump

    orig_dispatch_message = MessagePump._dispatch_message

    async def timed_dispatch_message(self, message):
        start = time.perf_counter()
        try:
            return await orig_dispatch_message(self, message)
        finally:
            elapsed = time.perf_counter() - start
            recorder.record(elapsed, "message", type(message).__name__, f" widget={type(self).__name__}")

    MessagePump._dispatch_message = timed_dispatch_message

    orig_display = App._display

    def timed_display(self, screen, renderable):
        start = time.perf_counter()
        try:
            return orig_display(self, screen, renderable)
        finally:
            elapsed = time.perf_counter() - start
            # ChopsUpdate.chops (a per-line map of screen fragments) is the
            # honest proxy for how many tty bytes a repaint actually wrote —
            # a full non-chopped render (LayoutUpdate, or None) has no chops.
            chops = getattr(renderable, "chops", None)
            if chops is not None:
                extra = f" chops={sum(len(line) for line in chops)}"
            elif renderable is not None:
                extra = " full-render"
            else:
                extra = ""
            name = type(renderable).__name__ if renderable is not None else "None"
            recorder.record(elapsed, "repaint", name, extra)

    App._display = timed_display


def run_tui(host: str, timing_path: str | None = None) -> None:
    app = build_app(host)
    if not timing_path:
        app.run()
        return
    recorder = _TimingRecorder(timing_path)
    install_timing(recorder)
    try:
        app.run()
    finally:
        recorder.write_summary()


# --- entry point -------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", help="explicit station URL, e.g. http://station.example:8419")
    parser.add_argument("--local", action="store_true", help="target local dev (127.0.0.1:8409)")
    parser.add_argument("--prod", action="store_true", help="target prod (PROD_URL, the default)")
    parser.add_argument(
        "--timing",
        help="write per-message/per-repaint timing to this path (env: ETV_EPG_TIMING); "
        "off by default, costs nothing when unset",
    )
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
    timing_path = args.timing or os.environ.get("ETV_EPG_TIMING")

    if args.command == "channels":
        cmd_channels(host)
    elif args.command == "channel":
        cmd_channel(host, args.channel_id)
    elif args.command == "current":
        cmd_current(host, args.channel_id)
    elif args.command == "artwork-check":
        cmd_artwork_check(host, args.channel_id)
    else:
        run_tui(host, timing_path)


if __name__ == "__main__":
    main()
