#!/usr/bin/env python3
"""Audit one channel on the deployed station for schedule and playback bugs.

Why this exists: a channel can look healthy in every place a person normally
glances — the guide fills, the stream plays, no channel is FAILED — while it
airs an hour of "PLAYBACK ERROR" twice a day, a French dub of an English show,
or a Tamil film with no subtitles. Each of those is a fact sitting in a
different place on the host: the playout chunks, the container log, the media
file's own streams, history.db, the lineup. This reads all of them for one
channel and prints every mismatch, so the `audit-channel` skill starts from
facts rather than from someone noticing.

Read-only. It never asks for a stream: fetching `/session/...` or
`/channel/N.m3u8` makes ETV-next start a transcode, so the lineup and guide are
read from `/channels.m3u` and `/xmltv.xml`, which start nothing.

What it reads, all over one ssh host:

  * every playout chunk whose span has not finished yet, in
    `/data/playout/<folder>/` inside the container;
  * `ffprobe` of every distinct file those chunks name, in one `docker exec`;
  * the container log since its start, for this channel's WARN/ERROR events;
  * `history.db` for the last `--days` days of airings;
  * `/channels.m3u`, `/xmltv.xml` and `/health/channels.json`;
  * the overlay's heartbeat and resolved spec.

Usage::

    tools/channel-audit.py 002-for-pierce
    tools/channel-audit.py 002-for-pierce --json
    tools/channel-audit.py 034-syfy --lang eng --days 7

`UNRAID_HOST` / `UNRAID_USER` come from the environment, or from the repo's
`.env` for any that are unset.

Exit 0 when nothing was found, 1 when any finding is an error, 2 when the host,
the container or one of its files could not answer — then no finding printed.
WARN/ERROR log events and history count over the last `--days` days.
"""

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA = "/mnt/user/appdata/etv-station/data"
PORT = "8409"

# Finding severities. `error` is something a viewer sees go wrong; `warn` is
# a likely problem worth a look; `info` is context the reader needs.
ERROR, WARN, INFO = "error", "warn", "info"


class HostError(Exception):
    """The host or container could not answer; no finding can be trusted."""


def load_dotenv():
    path = os.path.join(REPO, ".env")
    if not os.path.exists(path):
        return
    for line in open(path):
        m = re.match(r"\s*([A-Za-z_][A-Za-z0-9_]*)=(.*)$", line)
        if m and m.group(1) not in os.environ:
            os.environ[m.group(1)] = m.group(2).strip().strip("'\"")


def ssh(target, remote, stdin=None, timeout=300):
    # ssh exits 255 when it cannot reach the host; any other code is the remote
    # command's own, which callers read as an answer.
    try:
        r = subprocess.run(
            ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", target, remote],
            input=stdin, capture_output=True, text=True, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        raise HostError(f"ssh {target} timed out after {timeout}s")
    if r.returncode == 255:
        raise HostError(f"ssh {target} failed: {r.stderr.strip()[:200]}")
    return r.returncode, r.stdout, r.stderr


def ssh_ok(target, remote, what, **kw):
    rc, out, err = ssh(target, remote, **kw)
    if rc != 0:
        raise HostError(f"{what}: exit {rc}: {err.strip()[:200]}")
    return out


def sql_rows(target, db, query):
    # One statement per call: `.mode json` prints nothing for an empty result,
    # so several result sets in one output cannot be told apart.
    out = ssh_ok(target, f"sqlite3 -readonly {db}", f"sqlite3 {db}",
                 stdin=".mode json\n" + query + "\n")
    return json.loads(out) if out.strip() else []


def in_container(cmd):
    return "docker exec -i etv-station sh -c " + shlex.quote(cmd)


def parse_ts(s):
    # Chunk timestamps carry nanoseconds; datetime takes microseconds.
    m = re.match(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.(\d+))?Z$", s)
    frac = (m.group(2) or "0")[:6].ljust(6, "0")
    return datetime.fromisoformat(m.group(1) + "." + frac).replace(tzinfo=timezone.utc)


def chunk_finish(name):
    # `{start}_{finish}.json`, each `YYYYMMDDTHHMMSS.nnnnnnnnnZ`.
    m = re.match(r"\d{8}T\d{6}\.\d+Z_(\d{8}T\d{6})\.\d+Z\.json$", name)
    if not m:
        return None
    return datetime.strptime(m.group(1), "%Y%m%dT%H%M%S").replace(tzinfo=timezone.utc)


def error_card_reason(params):
    # The lavfi card's last drawtext line carries the reason, e.g. "file not found".
    texts = re.findall(r"text=([^:]*)", params or "")
    texts = [t for t in texts if not t.startswith("fontfile")]
    return texts[-1] if texts else "?"


class Audit:
    def __init__(self, folder, lang, days):
        self.folder = folder
        self.lang = lang
        self.days = days
        self.findings = []
        self.facts = {}

    def add(self, severity, element, what, **detail):
        self.findings.append({"severity": severity, "element": element, "what": what, **detail})


def read_chunks(audit, target):
    d = f"/data/playout/{audit.folder}"
    out = ssh_ok(target, in_container(f"ls {d}"), f"list {d} in the container")
    now = datetime.now(timezone.utc)
    names = sorted(n for n in out.split() if chunk_finish(n) and chunk_finish(n) > now)
    if not names:
        audit.add(ERROR, "schedule", "no playout chunk covers now or later", dir=d)
        return []
    out = ssh_ok(target, in_container(
        "for f in " + " ".join(shlex.quote(f"{d}/{n}") for n in names)
        + "; do cat \"$f\" || exit 1; echo; echo '#--chunk--#'; done"), f"read chunks in {d}")
    items = []
    for name, body in zip(names, out.split("#--chunk--#")):
        try:
            items += json.loads(body)["items"]
        except (json.JSONDecodeError, KeyError) as e:
            raise HostError(f"chunk {name} is not playout JSON: {e}")
    # An item straddling a chunk boundary is written whole into both chunks.
    seen, unique = set(), []
    for it in sorted(items, key=lambda i: i["start"]):
        key = (it["id"], it["start"])
        if key not in seen:
            seen.add(key)
            unique.append(it)
    audit.facts["chunks"] = names
    audit.facts["items"] = len(unique)
    audit.facts["span"] = [unique[0]["start"], unique[-1]["finish"]] if unique else None
    return unique


def title_of(it):
    p = it.get("program") or {}
    t = p.get("title") or it["id"]
    if p.get("season") is not None and p.get("episode") is not None:
        t += f" S{p['season']:02d}E{p['episode']:02d}"
    return t


def check_schedule(audit, items):
    for a, b in zip(items, items[1:]):
        gap = (parse_ts(b["start"]) - parse_ts(a["finish"])).total_seconds()
        if abs(gap) > 1:
            audit.add(ERROR, "schedule", "gap" if gap > 0 else "overlap",
                      at=a["finish"], seconds=round(gap, 1), after=title_of(a), before=title_of(b))

    by_id = defaultdict(list)
    for it in items:
        by_id[it["id"]].append(it)
    for ident, its in by_id.items():
        if len(its) < 2:
            continue
        starts = [parse_ts(i["start"]) for i in its]
        closest = min((b - a for a, b in zip(starts, starts[1:])), default=timedelta(0))
        audit.add(WARN, "schedule", "same item scheduled more than once in the window",
                  id=ident, title=title_of(its[0]), times=len(its),
                  closest_hours=round(closest.total_seconds() / 3600, 1),
                  starts=[i["start"][:16] for i in its])

    for it in items:
        src = it.get("source") or {}
        if src.get("source_type") == "lavfi" and "PLAYBACK ERROR" in (src.get("params") or ""):
            audit.add(ERROR, "programme", "error card scheduled in place of the item",
                      id=it["id"], title=title_of(it), start=it["start"][:16],
                      minutes=round((parse_ts(it["finish"]) - parse_ts(it["start"])).total_seconds() / 60),
                      reason=error_card_reason(src.get("params")))


PROBE = (
    "while IFS= read -r p; do printf '#--file--#%s\\n' \"$p\"; "
    "if [ ! -e \"$p\" ]; then echo '{\"error\": \"file not found\"}'; continue; fi; "
    "ffprobe -v error -print_format json -show_entries "
    "'format=duration:stream=index,codec_type,codec_name,channels:stream_tags=language,title"
    ":stream_disposition=default,forced,comment' \"$p\" 2>/dev/null "
    "|| echo '{\"error\": \"ffprobe could not read the file\"}'; done"
)


def probe_files(target, paths):
    out = ssh_ok(target, in_container(PROBE), "ffprobe in the container",
                 stdin="\n".join(paths) + "\n", timeout=900)
    result = {}
    for block in out.split("#--file--#")[1:]:
        path, _, body = block.partition("\n")
        try:
            result[path] = json.loads(body)
        except json.JSONDecodeError:
            result[path] = {"error": body.strip()[:300]}
    return result


def lang_of(stream):
    return ((stream or {}).get("tags") or {}).get("language")


def check_playback(audit, target, items):
    paths = sorted({(it.get("source") or {}).get("path") for it in items} - {None})
    probes = probe_files(target, paths)
    audit.facts["files_probed"] = len(probes)
    for it in items:
        src = it.get("source") or {}
        path = src.get("path")
        if not path:
            continue
        p = probes.get(path) or {"error": "not probed"}
        where = {"title": title_of(it), "start": it["start"][:16]}
        if "error" in p:
            audit.add(ERROR, "programme", "file cannot be probed", path=path, error=p["error"], **where)
            continue
        streams = {s["index"]: s for s in p.get("streams", [])}
        kinds = defaultdict(list)
        for s in streams.values():
            kinds[s["codec_type"]].append(s)
        if not kinds["video"]:
            audit.add(ERROR, "programme", "file has no video stream", path=path, **where)
        slot = (parse_ts(it["finish"]) - parse_ts(it["start"])).total_seconds()
        dur = float((p.get("format") or {}).get("duration") or 0)
        # A trimmed source plays only in_point..out_point of its file.
        if src.get("out_point_ms") is not None:
            dur = src["out_point_ms"] / 1000
        if src.get("in_point_ms") is not None:
            dur -= src["in_point_ms"] / 1000
        if dur and abs(slot - dur) > 5:
            audit.add(WARN, "programme", "slot length differs from the file's length",
                      slot_s=round(slot), file_s=round(dur), **where)

        tracks = it.get("tracks") or {}
        sub = tracks.get("subtitle")
        if sub and sub.get("stream_index") is not None and sub.get("source") is None:
            s = streams.get(sub["stream_index"])
            if s is None or s["codec_type"] != "subtitle":
                audit.add(ERROR, "subtitles", "selected subtitle stream is not a subtitle stream",
                          stream_index=sub["stream_index"], **where)
        # A track with its own `source` (a bumper's bed or silence) indexes streams
        # in that source, not in this file, so this file's audio says nothing about it.
        if (tracks.get("audio") or {}).get("source") is not None:
            continue
        a_idx = (tracks.get("audio") or {}).get("stream_index")
        audio = streams.get(a_idx) if a_idx is not None else (kinds["audio"] or [None])[0]
        if a_idx is not None and (audio is None or audio["codec_type"] != "audio"):
            audit.add(ERROR, "audio", "selected audio stream is not an audio stream",
                      stream_index=a_idx, **where)
            continue
        if audio is None:
            audit.add(ERROR, "audio", "file has no audio stream", **where)
            continue
        alang = lang_of(audio)
        wanted = [s for s in kinds["audio"] if lang_of(s) == audit.lang]
        if alang not in (audit.lang, None, "und") and wanted:
            audit.add(ERROR, "audio", f"plays {alang} audio although a {audit.lang} track exists",
                      selected=audio["index"], selected_title=(audio.get("tags") or {}).get("title"),
                      available=[s["index"] for s in wanted], **where)
        elif alang not in (audit.lang, None, "und"):
            sub = tracks.get("subtitle")
            subs_in_lang = [s["index"] for s in kinds["subtitle"] if lang_of(s) == audit.lang]
            if not sub:
                audit.add(ERROR, "subtitles", f"{alang} audio with no subtitle track selected",
                          subtitle_streams_in_lang=subs_in_lang, **where)


def check_log(audit, target):
    out = ssh_ok(target, "docker inspect etv-station --format '{{.State.StartedAt}}'", "docker inspect")
    audit.facts["container_started"] = out.strip()
    # grep exits 1 on no match, which is an answer, not a failure.
    rc, out, err = ssh(target, "docker logs etv-station 2>&1 | grep -F " + shlex.quote(audit.folder),
                       timeout=300)
    if rc not in (0, 1):
        raise HostError(f"docker logs: exit {rc}: {err.strip()[:200]}")
    since = (datetime.now(timezone.utc) - timedelta(days=audit.days)).isoformat()[:19]
    events = Counter()
    unsatisfied = []
    for line in out.splitlines():
        try:
            rec = json.loads(line[line.index("{"):])
        except (ValueError, json.JSONDecodeError):
            continue
        if rec.get("level") not in ("WARN", "ERROR") or rec.get("timestamp", "")[:19] < since:
            continue
        f = rec.get("fields") or {}
        events[(rec.get("level"), f.get("event") or f.get("message", "")[:80])] += 1
        if f.get("event") == "constraints.unsatisfied":
            unsatisfied.append((rec["timestamp"][:16], f.get("pool"), f.get("violations"), f.get("items")))
    for (level, event), n in sorted(events.items(), key=lambda kv: -kv[1]):
        audit.add(WARN if level == "WARN" else ERROR, "log", event, count=n)
    for at, pool, v, n in unsatisfied[-6:]:
        audit.add(WARN, "schedule", "pool's no-repeat constraint could not be met",
                  at=at, pool=pool, violations=v, items=n)


def check_history(audit, target):
    since_ns = int((datetime.now(timezone.utc) - timedelta(days=audit.days)).timestamp()) * 10**9
    ch = audit.folder.replace("'", "''")
    db = f"{DATA}/playout/history.db"
    # Bumper segments share the ledger; `kind` keeps them out of programme counts.
    where = f"channel='{ch}' and start_ns > {since_ns} and kind='programme'"
    totals = sql_rows(target, db, "select count(*) airings, sum(error_card) error_cards,"
                      f" count(distinct entry_id) distinct_items from airings where {where};")
    audit.facts["history"] = {"days": audit.days, **(totals[0] if totals else {})}
    repeats = sql_rows(target, db, "select entry_id, count(*) n, sum(error_card) cards from airings"
                       f" where {where} and show_id is null group by entry_id having n > 1"
                       " order by n desc limit 10;")
    cards = sql_rows(target, db, "select entry_id, count(*) n from airings"
                     f" where {where} and error_card = 1 group by entry_id order by n desc;")
    for r in cards:
        audit.add(ERROR, "programme", f"aired as an error card {r['n']}x in {audit.days} days",
                  id=r["entry_id"])
    for r in repeats:
        if r["n"] >= 3 and not r["cards"]:
            audit.add(INFO, "history", f"film aired {r['n']}x in {audit.days} days", id=r["entry_id"])


def check_lineup(audit, target, number):
    base = f"http://127.0.0.1:{PORT}"
    m3u = ssh_ok(target, in_container(f"curl -sf {base}/channels.m3u"), "GET /channels.m3u")
    health = ssh_ok(target, in_container(f"curl -sf {base}/health/channels.json"), "GET /health/channels.json")
    xml = ssh_ok(target, in_container(f"curl -sf {base}/xmltv.xml"), "GET /xmltv.xml")
    if number is None:
        audit.add(WARN, "logo", "channel number unknown; lineup, guide and health not checked")
        return
    entry = next((l for l in m3u.splitlines() if l.startswith("#EXTINF") and
                  re.search(r'tvg-name="[^"]*"', l) and
                  (f'tvg-id="ersatztv.{number}"' in l)), None)
    if entry is None:
        audit.add(ERROR, "logo", "channel is not in /channels.m3u", number=number)
        return
    audit.facts["lineup_entry"] = entry
    if "tvg-logo=" not in entry:
        audit.add(WARN, "logo", "no tvg-logo in the lineup; the guide shows no channel logo",
                  hint="set `logo:` in channel.yaml (001-for-you does)")
    chan = re.search(rf'<channel id="ersatztv\.{number}">(.*?)</channel>', xml, re.S)
    if chan and "<icon" not in chan.group(1):
        audit.add(WARN, "logo", "no <icon> in the XMLTV <channel>")
    progs = re.findall(rf'<programme[^>]*channel="ersatztv\.{number}"', xml)
    audit.facts["xmltv_programmes"] = len(progs)
    if not progs:
        audit.add(ERROR, "schedule", "no programmes for the channel in /xmltv.xml")
    try:
        rows = json.loads(health)
        rows = rows.get("channels", rows) if isinstance(rows, dict) else rows
        row = next((r for r in rows if str(r.get("number")) == str(number)), None)
        audit.facts["health"] = row
        if row and row.get("failed"):
            audit.add(ERROR, "playback", "channel FAILED on /health/channels.json", row=row)
        elif row and row.get("consecutive_failures"):
            audit.add(WARN, "playback", "channel has recent failures on /health/channels.json", row=row)
    except (json.JSONDecodeError, AttributeError, TypeError):
        audit.add(WARN, "playback", "/health/channels.json unreadable")


def check_overlay(audit, target):
    d = f"/data/playout/{audit.folder}"
    _, beat, _ = ssh(target, in_container(f"cat {d}/overlay.heartbeat 2>/dev/null"))
    _, spec, _ = ssh(target, in_container(f"cat {d}/overlay.json 2>/dev/null"))
    if not spec.strip():
        audit.add(INFO, "overlay", "no overlay.json; the channel has no overlay")
        return
    try:
        audit.facts["overlay_heartbeat"] = json.loads(beat) if beat.strip() else None
    except json.JSONDecodeError:
        audit.facts["overlay_heartbeat"] = beat.strip()
    images = re.findall(r'"path":\s*"([^"]+\.(?:png|jpg|jpeg|webp|svg))"', spec)
    scripts = re.findall(r'"script":\s*"([^"]+)"', spec)
    audit.facts["overlay_assets"] = sorted(set(images + scripts))
    for path in sorted(set(images + scripts)):
        if not path.startswith("/"):
            audit.add(WARN, "overlay", "overlay asset path is not absolute; not checked", path=path)
            continue
        rc, _, _ = ssh(target, in_container(f"test -s {shlex.quote(path)}"))
        if rc != 0:
            audit.add(ERROR, "overlay", "overlay asset missing in the container", path=path)


def name_ids(audit, target):
    # history.db holds ids only; name them from the host's catalog.
    ids = sorted({f["id"] for f in audit.findings if "id" in f and "title" not in f})
    if not ids:
        return
    quoted = ",".join("'" + i.replace("'", "''") + "'" for i in ids)
    rows = sql_rows(target, f"{DATA}/catalog.db",
                    "select entry_id, trim(coalesce(show || ' ', '') || title) title"
                    f" from entries where entry_id in ({quoted});")
    titles = {r["entry_id"]: r["title"] for r in rows}
    for f in audit.findings:
        if f.get("id") in titles and "title" not in f:
            f["title"] = titles[f["id"]]


def channel_number(folder):
    path = os.path.join(REPO, "deploy/appdata/channels", folder, "channel.yaml")
    if os.path.exists(path):
        m = re.search(r"^number:\s*(\d+)", open(path).read(), re.M)
        if m:
            return int(m.group(1))
    m = re.match(r"(\d+)-", folder)
    return int(m.group(1)) if m else None


def report_text(audit):
    order = {ERROR: 0, WARN: 1, INFO: 2}
    print(f"== {audit.folder}: {len(audit.findings)} finding(s)")
    for k, v in audit.facts.items():
        if k not in ("chunks",):
            print(f"   {k}: {json.dumps(v) if not isinstance(v, str) else v}")
    for f in sorted(audit.findings, key=lambda f: (order[f["severity"]], f["element"])):
        rest = {k: v for k, v in f.items() if k not in ("severity", "element", "what")}
        print(f"{f['severity'].upper():5} {f['element']:9} {f['what']}")
        if rest:
            print("      " + json.dumps(rest, ensure_ascii=False))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("folder", help="channel folder under deploy/appdata/channels, e.g. 002-for-pierce")
    ap.add_argument("--lang", default="eng", help="audio language a viewer expects (ISO 639-2), default eng")
    ap.add_argument("--days", type=int, default=14, help="history window in days, default 14")
    ap.add_argument("--json", action="store_true", help="print findings and facts as JSON")
    args = ap.parse_args()

    load_dotenv()
    host = os.environ.get("UNRAID_HOST")
    if not host:
        print("UNRAID_HOST is unset — it names the deploy host and lives in .env", file=sys.stderr)
        sys.exit(2)
    target = f"{os.environ.get('UNRAID_USER') or 'root'}@{host}"
    number = channel_number(args.folder)

    audit = Audit(args.folder, args.lang, args.days)
    try:
        items = read_chunks(audit, target)
        check_schedule(audit, items)
        check_playback(audit, target, items)
        check_log(audit, target)
        check_history(audit, target)
        check_lineup(audit, target, number)
        check_overlay(audit, target)
        name_ids(audit, target)
    except HostError as e:
        print(f"channel-audit: {e}", file=sys.stderr)
        sys.exit(2)

    if args.json:
        print(json.dumps({"channel": args.folder, "number": number, "facts": audit.facts,
                          "findings": audit.findings}, indent=1, ensure_ascii=False))
    else:
        report_text(audit)
    sys.exit(1 if any(f["severity"] == ERROR for f in audit.findings) else 0)


if __name__ == "__main__":
    main()
