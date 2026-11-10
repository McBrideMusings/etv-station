#!/usr/bin/env python3
"""Measure one channel's schedule from the dashboard's Review JSON.

Prints the numbers `schedule-analysis` grades and writes about: depth per pool
and per show, one measure per principle in docs/programming.md, the airings it
shares with other channels, and what moved since a previous run. It prints
measurements only; the grade thresholds live in docs/programming.md and the
agent applies them.

Usage:
  measure.py REVIEW.json [--previous OLD.json] [--others DIR]
             [--catalog catalog.db] [--random-pools a,b]

REVIEW.json is `tools/ui-get.sh '/api/channels/<key>/review?project=14d'`.
--previous is an earlier answer for the same channel and span.
--others is a directory of other channels' review JSONs (any span).
--catalog is a catalog.db, for each show's episode count (stripping).
--random-pools names pools whose `order` is `random` in channel.yaml.
"""

import argparse
import json
import sqlite3
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone
from pathlib import Path
from zoneinfo import ZoneInfo

INTERSTITIAL_SECS = 5 * 60  # docs/programming.md, Interstitials and bumpers
STRIP_MIN_EPISODES = 75  # docs/programming.md, Stripping
STRIP_WINDOW_MIN = 30


def load(path):
    d = json.loads(Path(path).read_text())
    if "deployed" in d:  # a draft answer: measure the draft
        d = d["draft"]
    if "load_error" in d:
        raise SystemExit(f"{path}: the channel failed to load: {d['load_error']}")
    return d


def days(d, ts):
    return (ts - d["start"]) / 86400


def first_repeat(d, airings):
    seen = set()
    for a in airings:
        if a["id"] in seen:
            return a
        seen.add(a["id"])
    return None


def depth(d):
    rows = []
    pools = list(dict.fromkeys(a["pool"] for a in d["airings"]))
    for pool in pools:
        groups = [("pool", pool, [a for a in d["airings"] if a["pool"] == pool])]
        shows = list(dict.fromkeys(a["show"] for a in groups[0][2] if a.get("show")))
        groups += [("  show", s, [a for a in d["airings"] if a.get("show") == s]) for s in shows]
        for kind, name, airs in groups:
            rep = first_repeat(d, airs)
            rows.append((kind, name, len(airs), len({a["id"] for a in airs}),
                         f"{days(d, rep['start']):.1f} d ({rep['label']})" if rep else f">{d['days']} d"))
    return rows


def show_runs(d):
    runs, last = [], None
    for a in d["airings"]:
        if a["duration_secs"] < INTERSTITIAL_SECS:
            continue
        show = a.get("show") if a["kind"] == "episode" else None
        if show and show == last:
            runs[-1][1] += 1
        elif show:
            runs.append([show, 1])
        last = show
    return runs


def dayparting(d, tz):
    pools = {a["pool"] for a in d["airings"]}
    if len(pools) < 2:
        return "does not apply: one pool fills the whole day"
    secs = [defaultdict(float) for _ in range(4)]
    for a in d["airings"]:
        t, end = a["start"], a["start"] + a["duration_secs"]
        while t < end:
            local = datetime.fromtimestamp(t, tz)
            into = (local.hour % 6) * 3600 + local.minute * 60 + local.second
            nxt = min(end, t + 6 * 3600 - into)
            secs[local.hour // 6][a["pool"]] += nxt - t
            t = nxt
    shares = [{p: s / max(sum(q.values()), 1) * 100 for p, s in q.items()} for q in secs]
    swing = {p: max(q.get(p, 0) for q in shares) - min(q.get(p, 0) for q in shares) for p in pools}
    pool = max(sorted(swing), key=swing.get)
    return f"largest swing between 6-hour quarters: {pool}, {swing[pool]:.0f} points of airtime"


def block_programming(d):
    runs = show_runs(d)
    if not runs:
        return "does not apply: no series aired"
    mean = sum(n for _, n in runs) / len(runs)
    return f"{mean:.2f} episodes per visit over {len(runs)} visits"


def stripping(d, tz, catalog):
    if not catalog:
        return "not measured: pass --catalog for each show's episode count"
    shows = sorted({a["show"] for a in d["airings"] if a.get("show")})
    con = sqlite3.connect(f"file:{catalog}?mode=ro", uri=True)
    marks = ",".join("?" * len(shows))
    counts = dict(con.execute(
        f"SELECT show, COUNT(*) FROM entries WHERE type = 'episode' AND missing_since IS NULL "
        f"AND show IN ({marks}) GROUP BY show", shows)) if shows else {}
    deep = [s for s in shows if counts.get(s, 0) >= STRIP_MIN_EPISODES]
    weekdays = set()
    t = d["start"]
    while t < d["end"]:
        local = datetime.fromtimestamp(t, tz)
        if local.weekday() < 5:
            weekdays.add(local.date())
        t += 3600
    if not deep or not weekdays:
        return f"does not apply: no show that aired has {STRIP_MIN_EPISODES}+ episodes"
    best = None
    for show in deep:
        starts = [datetime.fromtimestamp(a["start"], tz) for a in d["airings"] if a.get("show") == show]
        starts = [(l.date(), l.hour * 60 + l.minute) for l in starts if l.weekday() < 5]
        held = max((len({day for day, m in starts
                         if min(abs(m - anchor), 1440 - abs(m - anchor)) <= STRIP_WINDOW_MIN})
                    for _, anchor in starts), default=0)
        share = held / len(weekdays) * 100
        if best is None or share > best[1]:
            best = (show, share)
    return f"{best[0]} holds its start time on {best[1]:.0f}% of weekdays"


def episode_order(d, random_pools):
    eps = []
    for a in d["airings"]:
        item = d["items"].get(a["id"], {})
        if a["kind"] == "episode" and a["pool"] not in random_pools and a.get("show") \
                and item.get("season") is not None and item.get("episode") is not None:
            eps.append((a["show"], (item["season"], item["episode"])))
    span = {}
    for show, key in eps:
        lo, hi = span.get(show, (key, key))
        span[show] = (min(lo, key), max(hi, key))
    last, steps, out = {}, 0, 0
    for show, (s, e) in eps:
        if show in last:
            steps += 1
            ps, pe = last[show]
            first, final = span[show]
            wrap = first != final and first[1] == 1 and (ps, pe) == final and (s, e) == first
            if not ((s == ps and e == pe + 1) or (s == ps + 1 and e == 1) or wrap):
                out += 1
        last[show] = (s, e)
    if not steps:
        return "does not apply: no show aired twice from an ordered pool"
    return f"{out} of {steps} steps out of order ({out / steps * 100:.1f}%)"


def interstitials(d):
    # Bumpers exist for the published span only: a projection runs no fill pass.
    real = [a for a in d["airings"] if not a["projected"]]
    events = sorted([(a["start"], a["duration_secs"]) for a in real] +
                    [(b["start"], b["duration_secs"]) for b in d.get("bumpers", [])])
    changes = filled = 0
    saw = None
    for _, secs in events:
        if secs < INTERSTITIAL_SECS:
            saw = True if saw is not None else None
            continue
        if saw is not None:
            changes += 1
            filled += saw
        saw = False
    if not changes:
        return "does not apply: fewer than two programs aired in the published span"
    return (f"{filled} of {changes} program changes in the published span have an interstitial "
            f"({filled / changes * 100:.1f}%; the review lists {len(d.get('bumpers', []))} bumpers)")


def rest_periods(d):
    programs = [a for a in d["airings"] if a["duration_secs"] >= INTERSTITIAL_SECS]
    if not programs:
        return "does not apply: no program aired"
    rep = first_repeat(d, programs)
    if not rep:
        return f"nothing repeats in {d['days']} days"
    return f"first repeat after {days(d, rep['start']):.1f} days: {rep['label']}"


def overlap(d, others_dir):
    mine = {a["id"]: a["label"] for a in d["airings"]}
    rows = []
    for path in sorted(Path(others_dir).glob("*.json")):
        o = json.loads(path.read_text())
        if o.get("channel") == d["channel"] or "airings" not in o:
            continue
        shared = sorted({a["id"] for a in o["airings"]} & mine.keys())
        if shared:
            rows.append((len(shared), o.get("number"), o.get("display_name") or o["channel"],
                         o["span"]["label"], [mine[i] for i in shared]))
    return sorted(rows, key=lambda r: -r[0])


def summary(d):
    s = {
        "Span": d["span"]["label"],
        "Days until first repeat": round(d["first_repeat"]["days"], 1) if d.get("first_repeat") else None,
        "Film share of airtime (%)": round(d["airtime"]["film_share"] * 100, 1),
        "Series share of airtime (%)": round(d["airtime"]["series_share"] * 100, 1),
        "Items aired": d["items_aired"],
        "Distinct items": d["distinct_items"],
    }
    for kind, name, _, _, first in depth(d):
        if kind == "pool":
            s[f"Pool {name}: first repeat"] = first.split(" (")[0]
    return s


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("review")
    ap.add_argument("--previous")
    ap.add_argument("--others")
    ap.add_argument("--catalog")
    ap.add_argument("--random-pools", default="")
    args = ap.parse_args()
    d = load(args.review)
    tz = ZoneInfo(d["tz"])
    when = lambda ts: datetime.fromtimestamp(ts, timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    print(f"# {d['number']} {d.get('display_name') or d['name']} — {d['span']['label']}, "
          f"{when(d['start'])} to {when(d['end'])}, {len(d['airings'])} airings\n")

    print("## Depth (aired / distinct / first repeat)")
    for kind, name, aired, distinct, first in depth(d):
        print(f"{kind} {name}: {aired} / {distinct} / {first}")

    print("\n## Principle measures")
    random_pools = {p for p in args.random_pools.split(",") if p}
    longest = max(show_runs(d), key=lambda r: r[1], default=None)
    for name, measure in [
        ("Dayparting", dayparting(d, tz)),
        ("Block programming", block_programming(d)),
        ("Stripping", stripping(d, tz, args.catalog)),
        ("Episode order", episode_order(d, random_pools)),
        ("Marathons", f"longest run: {longest[1]} episodes of {longest[0]}" if longest else "no series aired"),
        ("Interstitials and bumpers", interstitials(d)),
        ("Rest periods", rest_periods(d)),
    ]:
        print(f"{name}: {measure}")

    if args.others:
        print("\n## Overlap (airings shared with other channels)")
        rows = overlap(d, args.others)
        for n, number, name, span, titles in rows:
            print(f"{number} {name} ({span}): {n} shared — {', '.join(titles[:8])}{' …' if n > 8 else ''}")
        if not rows:
            print("No other channel's review shares an airing.")

    if args.previous:
        print("\n## Since the previous run")
        old, new = summary(load(args.previous)), summary(d)
        for label in dict.fromkeys([*new, *old]):
            mark = "  " if old.get(label) == new.get(label) else "* "
            print(f"{mark}{label}: {old.get(label, '—')} → {new.get(label, '—')}")


if __name__ == "__main__":
    main()
