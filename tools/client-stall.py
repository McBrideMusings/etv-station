#!/usr/bin/env python3
"""Log a client that stopped fetching segments while the server kept publishing.

Why this exists: a player can freeze with every server-side clock healthy. The
encoder keeps advancing, the segment index keeps moving, so neither
two-clock-capture.sh nor stream-watch.py has anything to report. What the
client does leave is a shape in the access log: it keeps asking for
``live.m3u8`` and stops asking for ``.ts`` segments.

This tails the log tools/stream-access-log.py writes and logs one row when, for
one (client address, session):

  * the last segment request is older than STALL_SECS, and
  * a playlist request arrived within the last POLL_SECS, and
  * the newest segment in the session's playlist is past the last one the
    client took (so there was something to fetch).

The last clause separates "the client gave up" from "there was nothing to
fetch". Clients are keyed by address and session, never by connection: VLC uses
one socket to poll the playlist and another to fetch segments, so a
per-connection detector would report a permanent stall.

One row per stall, re-armed when the client fetches a segment again::

    2026-08-21T14:49:46Z<TAB>192.168.0.252<TAB>10<TAB>client.stall<TAB>stalled_for=92s newest=live000900.ts last_taken=live000855.ts behind=45

Runs inside the container, started by docker/entrypoint.sh beside
stream-access-log.py. Reads playlists off disk, never over HTTP, because a
request under /session makes the server spawn a worker.

Usage::

    tools/client-stall.py
    tools/client-stall.py --self-test
    ACCESS_LOG=/data/diag/access.log HLS_ROOT=/data/hls \
        LOG_FILE=/data/diag/client-stall.log tools/client-stall.py
"""

import calendar
import os
import re
import sys
import time

ACCESS_LOG = os.environ.get(
    "ACCESS_LOG", "/mnt/user/appdata/etv-station/data/diag/access.log"
)
HLS_ROOT = os.environ.get("HLS_ROOT", "/mnt/user/appdata/etv-station/data/hls")
LOG_FILE = os.environ.get(
    "LOG_FILE", "/mnt/user/appdata/etv-station/data/diag/client-stall.log"
)
# Four target durations of 4 seconds.
STALL_SECS = float(os.environ.get("STALL_SECS", 16))
POLL_SECS = float(os.environ.get("POLL_SECS", 15))
INTERVAL = float(os.environ.get("INTERVAL", 2))
MAX_BYTES = int(os.environ.get("MAX_BYTES", 10 * 1024 * 1024))
# Clients not heard from for this long are dropped, so the table stays bounded.
FORGET_SECS = float(os.environ.get("FORGET_SECS", 3600))

PLAYLIST_PATH = re.compile(r"^GET /session/(\d+)/live\.m3u8")
SEGMENT_PATH = re.compile(r"^GET /session/(\d+)/(live(\d+)\.ts)")
SEGMENT_LINE = re.compile(r"^(live(\d+)\.ts)\s*$", re.M)


def parse_stamp(text):
    return calendar.timegm(time.strptime(text, "%Y-%m-%dT%H:%M:%SZ"))


class Client:
    def __init__(self):
        self.last_ts = None  # epoch seconds of the last segment request
        self.last_index = None  # its segment number
        self.last_name = None
        self.last_m3u8 = None
        self.fired = False


def observe(clients, row):
    """Fold one access-log row into the table. Replies and unrelated paths are
    ignored; only requests move the clocks."""
    fields = row.rstrip("\n").split("\t")
    if len(fields) < 5 or fields[3] != ">":
        return
    try:
        stamp = parse_stamp(fields[0])
    except ValueError:
        return
    ip, what = fields[1], fields[4]
    match = SEGMENT_PATH.match(what)
    if match:
        client = clients.setdefault((ip, match.group(1)), Client())
        client.last_ts = stamp
        client.last_name = match.group(2)
        client.last_index = int(match.group(3))
        client.fired = False
        return
    match = PLAYLIST_PATH.match(what)
    if match:
        clients.setdefault((ip, match.group(1)), Client()).last_m3u8 = stamp


def newest_segment(playlist_text):
    """(index, name) of the last segment the playlist lists, or None."""
    found = SEGMENT_LINE.findall(playlist_text)
    if not found:
        return None
    name, index = found[-1]
    return int(index), name


def stalled(client, now, newest):
    """Seconds the client has gone without a segment, or None if this is not a
    client-side stall."""
    if client.fired or client.last_ts is None or client.last_m3u8 is None:
        return None
    gap = now - client.last_ts
    if gap <= STALL_SECS or now - client.last_m3u8 >= POLL_SECS:
        return None
    # Equal means nothing new. Lower means the session restarted and its
    # numbering began again, which is still something the client could fetch.
    if newest is None or newest[0] == client.last_index:
        return None
    return gap


def read_playlist(session):
    path = os.path.join(HLS_ROOT, session, "live.m3u8")
    try:
        with open(path) as handle:
            return newest_segment(handle.read())
    except OSError:
        return None


def check(clients, now, emit, playlist=read_playlist):
    for (ip, session), client in list(clients.items()):
        newest = playlist(session) if client.last_ts is not None else None
        gap = stalled(client, now, newest)
        if gap is not None:
            client.fired = True
            emit(
                ip,
                session,
                f"stalled_for={gap:.0f}s newest={newest[1]} "
                f"last_taken={client.last_name} behind={newest[0] - client.last_index}",
            )
        last_heard = max(t for t in (client.last_ts, client.last_m3u8) if t is not None)
        if now - last_heard > FORGET_SECS:
            del clients[(ip, session)]


class RotatingLog:
    def __init__(self, path, max_bytes):
        self.path = path
        self.max_bytes = max_bytes
        self.handle = open(path, "a", buffering=1)

    def write(self, row):
        self.handle.write(row + "\n")
        if self.handle.tell() >= self.max_bytes:
            self.handle.close()
            try:
                os.replace(self.path, self.path + ".1")
            except OSError:
                pass
            self.handle = open(self.path, "a", buffering=1)


def follow(path):
    """Yield new lines appended to ``path`` forever, starting at its end so the
    history already in the file is not judged against today's clock. Reopens
    after stream-access-log.py rotates it."""
    handle = None
    inode = None
    while True:
        try:
            current = os.stat(path)
        except OSError:
            current = None
        if current is not None and (handle is None or current.st_ino != inode):
            if handle is not None:
                handle.close()
            handle = open(path)
            if inode is None:
                handle.seek(0, os.SEEK_END)
            inode = current.st_ino
        if handle is None:
            yield None
            continue
        line = handle.readline()
        # A write caught half-way has no newline yet; drop it rather than
        # parse a truncated row.
        yield line if line.endswith("\n") else None


def main():
    os.makedirs(os.path.dirname(LOG_FILE), exist_ok=True)
    log = RotatingLog(LOG_FILE, MAX_BYTES)

    def emit(ip, session, detail):
        stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        log.write(f"{stamp}\t{ip}\t{session}\tclient.stall\t{detail}")

    clients = {}
    last_check = 0.0
    for line in follow(ACCESS_LOG):
        if line:
            observe(clients, line)
            continue
        time.sleep(0.25)
        if time.monotonic() - last_check >= INTERVAL:
            last_check = time.monotonic()
            check(clients, time.time(), emit)


def self_test():
    """Replay the 2026-08-21 shape and two near-misses. Returns the failures."""
    failures = []
    base = parse_stamp("2026-08-21T14:47:00Z")

    def row(offset, ip, port, what):
        stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(base + offset))
        return f"{stamp}\t{ip}\t{port}\t>\tGET {what}\tVLC/3.0.4"

    def run(rows, now_offset, newest):
        clients, out = {}, []
        for r in rows:
            observe(clients, r)
        check(clients, base + now_offset,
              lambda ip, s, d: out.append((ip, s, d)),
              playlist=lambda s: newest)
        return out

    # Two sockets, one client: a segment fetcher on 52888 and a poller on 52889.
    frozen = [row(0, "10.0.0.5", 52888, "/session/10/live000855.ts")]
    frozen += [row(t, "10.0.0.5", 52889, "/session/10/live.m3u8") for t in range(0, 95, 5)]
    out = run(frozen, 95, (900, "live000900.ts"))
    if len(out) != 1 or "behind=45" not in out[0][2] or "last_taken=live000855.ts" not in out[0][2]:
        failures.append(f"frozen client not reported once with the gap: {out}")

    # Nothing new to fetch: the server stopped publishing too.
    if run(frozen, 95, (855, "live000855.ts")):
        failures.append("reported a stall while the server had nothing new")

    # Server restarted underneath the client: numbering dropped below its last.
    out = run(frozen, 95, (3, "live000003.ts"))
    if len(out) != 1 or "behind=-852" not in out[0][2]:
        failures.append(f"frozen client after a restart not reported: {out}")

    # Client stopped polling too: it left, it did not freeze.
    left = [row(0, "10.0.0.5", 52888, "/session/10/live000855.ts"),
            row(5, "10.0.0.5", 52889, "/session/10/live.m3u8")]
    if run(left, 95, (900, "live000900.ts")):
        failures.append("reported a client that stopped polling")

    # Healthy: segments keep arriving.
    healthy = frozen + [row(90, "10.0.0.5", 52888, "/session/10/live000899.ts")]
    if run(healthy, 95, (900, "live000900.ts")):
        failures.append("reported a healthy client")
    return failures


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        bad = self_test()
        for line in bad:
            print("FAIL", line)
        print("self-test:", "FAIL" if bad else "ok (5 cases)")
        sys.exit(1 if bad else 0)
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(0)
