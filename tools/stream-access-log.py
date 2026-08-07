#!/usr/bin/env python3
"""Record every HLS request clients make to the deployed etv-next server.

Why this exists: etv-next keeps no record of who is watching. The only trace a
viewer leaves is the mtime on a zero-byte ``.heartbeat`` file, which says
"somebody asked for something in the last 90 seconds" and nothing else — not
which client, not which segment, not when they stopped. When a player freezes
but the server keeps transcoding happily, that file cannot tell you whether the
player gave up or is still asking and getting something it cannot play.

So read the wire instead. tcpdump is pointed at the published port, filtered to
packets travelling *towards* the server (requests only, never the video coming
back), and each HTTP request line is flattened to one tab-separated row::

    2026-08-07T16:23:01Z<TAB>192.168.0.252<TAB>GET<TAB>/session/1/live000855.ts<TAB>AppleCoreMedia/1.0.0…

That is enough to answer "what did this player ask for last, and when did it
stop asking" months after the fact, which is the question a freeze poses.

Runs ON the Unraid host, not in the container — the container never sees the
client's address, only the docker bridge's. Install and start it with
tools/diag-install.sh rather than by hand.

Usage::

    tools/stream-access-log.py
    PORT=8419 LOG_FILE=/mnt/user/appdata/etv-station/data/diag/access.log \
        MAX_BYTES=52428800 tools/stream-access-log.py
"""

import os
import re
import shutil
import subprocess
import sys

PORT = os.environ.get("PORT", "8419")
LOG_FILE = os.environ.get(
    "LOG_FILE", "/mnt/user/appdata/etv-station/data/diag/access.log"
)
# Rotated at this size, one generation kept (access.log.1). 50MB of these rows
# is roughly a month of two clients streaming continuously.
MAX_BYTES = int(os.environ.get("MAX_BYTES", 50 * 1024 * 1024))
IFACE = os.environ.get("IFACE", "any")

# tcpdump -i any prefixes the interface and direction between the timestamp and
# the "IP" token ("... eth0  In  IP 1.2.3.4.5 > ..."), so anchor on the
# timestamp and skip whatever sits in the middle. The source port and TCP
# sequence number are captured too — see DedupeWindow for why.
HEADER = re.compile(
    r"^(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d)\.\d+"
    r".*?IP (\d+\.\d+\.\d+\.\d+)\.(\d+) >"
)
SEQ = re.compile(r"seq (\d+)[:,]")
REQUEST = re.compile(r"(GET|HEAD|POST) (/\S*) HTTP/")
AGENT = re.compile(r"[Uu]ser-[Aa]gent: *(.+)")


class DedupeWindow:
    """Drop packets we have already recorded.

    Listening on every interface at once means one arriving packet can be seen
    more than once: on this host the LAN card `eth0` is enslaved to `bond0`, so
    a request from a TV on the LAN is captured twice, while a request over
    Tailscale crosses one interface and is captured once. Counting raw captures
    would report the LAN client making exactly double the requests it really
    makes — an invented pattern that looks like a client bug.

    A packet's identity is its sender's address and port plus its TCP sequence
    number, and no two distinct requests on one connection ever share that. A
    genuine retransmission does share it, and folding those together is right
    here: a retransmitted request is still one request asked for one file.

    Bounded so a capture left running for months cannot grow without limit.
    """

    def __init__(self, size=8192):
        self.size = size
        self.seen = {}

    def is_new(self, key):
        if key in self.seen:
            return False
        self.seen[key] = None
        if len(self.seen) > self.size:
            # dicts keep insertion order, so this drops the oldest keys
            for old in list(self.seen)[: self.size // 2]:
                del self.seen[old]
        return True


class RotatingLog:
    """Append rows, keeping one previous generation so a capture left running
    for months cannot fill the disk. Size is the only honest trigger: the row
    rate tracks viewer count, so a time-based rotation would cut a busy day
    into the same slice as an idle one."""

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


def tcpdump_command():
    """
    -tttt   full date+time per packet, so rows stay meaningful across days
    -A      print payload as ASCII, which is where the request line lives
    -s 700  enough of each packet to carry the request line and User-Agent
    -l      line-buffered, so rows appear as they happen, not in blocks

    ``dst port`` keeps this to client -> server packets. Without it the capture
    also carries every video segment flowing back out, which is ~99% of the
    bytes and contains nothing we want.
    """
    return [
        "tcpdump", "-i", IFACE, "-nn", "-A", "-s", "700", "-l", "-tttt",
        f"tcp dst port {PORT}",
    ]


def main():
    for tool in ("tcpdump",):
        if shutil.which(tool) is None:
            print(f"fatal: required tool not found: {tool}", file=sys.stderr)
            return 2

    os.makedirs(os.path.dirname(LOG_FILE), exist_ok=True)
    log = RotatingLog(LOG_FILE, MAX_BYTES)

    # TZ=UTC because -tttt stamps in local time, and these rows get read
    # alongside the container's log, which is UTC. The Unraid host runs on
    # Chicago time, so without this every row would sit five hours off its
    # matching daemon entry.
    env = dict(os.environ, TZ="UTC")
    proc = subprocess.Popen(
        tcpdump_command(),
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        env=env,
        text=True,
        bufsize=1,
    )

    dedupe = DedupeWindow()
    stamp = None
    src = None
    port = None
    header_line = ""
    packet = []

    def flush():
        """Emit one row if the packet just read was an HTTP request. The
        request line and User-Agent almost always share a packet, so no
        cross-packet reassembly is needed — and a request whose agent landed
        elsewhere is still worth recording without it."""
        if not packet or stamp is None:
            return
        body = "\n".join(packet)
        request = REQUEST.search(body)
        if not request:
            return
        seq = SEQ.search(header_line)
        key = (src, port, seq.group(1) if seq else request.group(2))
        if not dedupe.is_new(key):
            return
        agent = AGENT.search(body)
        log.write(
            "\t".join([
                stamp.replace(" ", "T") + "Z",
                src or "?",
                request.group(1),
                request.group(2),
                agent.group(1).strip() if agent else "-",
            ])
        )

    try:
        for line in proc.stdout:
            match = HEADER.match(line)
            if match:
                flush()
                stamp, src, port = match.group(1), match.group(2), match.group(3)
                header_line = line
                packet = []
            else:
                packet.append(line.rstrip("\n"))
        flush()
    except KeyboardInterrupt:
        pass
    finally:
        proc.terminate()

    return 0


if __name__ == "__main__":
    sys.exit(main())
