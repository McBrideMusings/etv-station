#!/usr/bin/env python3
"""Record every HLS request clients make to the deployed etv-next server, and
the reply the server sent back.

Why this exists: etv-next keeps no record of who is watching. The only trace a
viewer leaves is the mtime on a zero-byte ``.heartbeat`` file, which says
"somebody asked for something in the last 90 seconds" and nothing else — not
which client, not which segment, not when they stopped. When a player freezes
but the server keeps transcoding happily, that file cannot tell you whether the
player gave up or is still asking and getting something it cannot play.

So read the wire instead. tcpdump watches the published port and every HTTP
message — the client's request line and the server's status line — is flattened
to one tab-separated row::

    2026-08-07T16:23:01Z<TAB>192.168.0.252<TAB>52431<TAB>><TAB>GET /session/1/live000855.ts<TAB>AppleCoreMedia/1.0.0…
    2026-08-07T16:23:01Z<TAB>192.168.0.252<TAB>52431<TAB><<TAB>200<TAB>len=1153004 type=video/mp2t

The client's port is column 3 and the arrow is column 4, so a request and the
reply to it pair up by matching address + port and reading down the file. A
request with no reply under it is the server going quiet on that client, which
is exactly the shape a freeze takes.

Only the *header* packet of a reply is recorded, never the video bytes: the
filter keeps server-to-client packets whose payload begins with "HTTP", so the
~99% of traffic that is segment data is dropped by the kernel and never reaches
this process. (That payload test is IPv4-only — libpcap cannot index past IPv6
extension headers — which matches the IPv4-only address parsing below.)

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
# the "IP" token ("... eth0  In  IP 1.2.3.4.5 > 6.7.8.9.10: ..."), so anchor on
# the timestamp and skip whatever sits in the middle. Both endpoints are needed:
# on a request the client is the sender, on a reply it is the receiver. The TCP
# sequence number is captured too — see DedupeWindow for why.
HEADER = re.compile(
    r"^(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d)\.\d+"
    r".*?IP (\d+\.\d+\.\d+\.\d+)\.(\d+) > (\d+\.\d+\.\d+\.\d+)\.(\d+):"
)
SEQ = re.compile(r"seq (\d+)[:,]")
REQUEST = re.compile(r"(GET|HEAD|POST) (/\S*) HTTP/")
RESPONSE = re.compile(r"HTTP/\d\.\d (\d{3})")
AGENT = re.compile(r"[Uu]ser-[Aa]gent: *(.+)")
LENGTH = re.compile(r"[Cc]ontent-[Ll]ength: *(\d+)")
CTYPE = re.compile(r"[Cc]ontent-[Tt]ype: *([^;\r\n]+)")

# Column 4 of every row. A reader can tell the two kinds apart without parsing
# anything else, and a file written by the older requests-only version of this
# script has neither, which is how startup spots one.
REQUEST_MARK = ">"
RESPONSE_MARK = "<"


class DedupeWindow:
    """Drop packets we have already recorded.

    Listening on every interface at once means one arriving packet can be seen
    more than once: on this host the LAN card `eth0` is enslaved to `bond0`, so
    a request from a TV on the LAN is captured twice, while a request over
    Tailscale crosses one interface and is captured once. Counting raw captures
    would report the LAN client making exactly double the requests it really
    makes — an invented pattern that looks like a client bug.

    A packet's identity is its sender's address and port plus its TCP sequence
    number, and no two distinct messages on one connection ever share that. A
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

    def rotate(self):
        self.handle.close()
        try:
            os.replace(self.path, self.path + ".1")
        except OSError:
            pass
        self.handle = open(self.path, "a", buffering=1)

    def write(self, row):
        self.handle.write(row + "\n")
        if self.handle.tell() >= self.max_bytes:
            self.rotate()


def roll_old_format(path):
    """Move a log written by the requests-only version out of the way.

    Rows gained a client-port column and a direction arrow, so old and new rows
    do not line up. Mixing both shapes in one file means every reader has to
    guess which era a row came from, and a count grouped by column would quietly
    total two different things. The old rows are still worth keeping — they are
    the record of whatever freeze prompted this — so they go to the generation
    slot the rotation already uses.
    """
    try:
        with open(path) as handle:
            first = handle.readline()
    except OSError:
        return
    if not first.strip():
        return
    fields = first.rstrip("\n").split("\t")
    if len(fields) > 3 and fields[3] in (REQUEST_MARK, RESPONSE_MARK):
        return
    os.replace(path, path + ".1")
    print(f"rolled pre-reply rows to {path}.1", file=sys.stderr)


def tcpdump_filter():
    """Client requests, plus reply headers with the video stripped out.

    ``tcp dst port`` is every packet heading to the server, which is where the
    request lines are. Coming back the other way we want only the first packet
    of each reply, so the second test reads the four bytes at the start of the
    TCP payload and keeps the packet only if they spell "HTTP" — the status
    line. Segment data never begins that way, so the megabytes of video are
    discarded in the kernel.

    ``tcp[12:1] & 0xf0 >> 2`` is the TCP data offset field turned into a byte
    count, i.e. where the header stops and the payload starts.
    """
    payload_starts_http = "tcp[((tcp[12:1] & 0xf0) >> 2):4] = 0x48545450"
    return (
        f"tcp port {PORT} and "
        f"(tcp dst port {PORT} or {payload_starts_http})"
    )


def tcpdump_command():
    """
    -tttt   full date+time per packet, so rows stay meaningful across days
    -A      print payload as ASCII, which is where the request line lives
    -s 700  enough of each packet to carry the request line and User-Agent
    -l      line-buffered, so rows appear as they happen, not in blocks
    -p      no promiscuous mode

    -p is what lets this run inside the container as a non-root user. Going
    promiscuous needs CAP_NET_ADMIN, which Docker does not grant by default;
    plain capture needs only CAP_NET_RAW, which it does. Nothing is given up:
    promiscuous mode collects traffic addressed to other machines, and every
    packet this cares about is addressed to the server it is running beside.
    """
    return [
        "tcpdump", "-i", IFACE, "-p", "-nn", "-A", "-s", "700", "-l", "-tttt",
        tcpdump_filter(),
    ]


def main():
    for tool in ("tcpdump",):
        if shutil.which(tool) is None:
            print(f"fatal: required tool not found: {tool}", file=sys.stderr)
            return 2

    os.makedirs(os.path.dirname(LOG_FILE), exist_ok=True)
    roll_old_format(LOG_FILE)
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
    src = src_port = dst = dst_port = None
    header_line = ""
    packet = []

    def flush():
        """Emit one row if the packet just read carried an HTTP message. The
        request line and User-Agent almost always share a packet, as do a status
        line and its Content-Length, so no cross-packet reassembly is needed —
        and a message whose trailing header landed elsewhere is still worth
        recording without it."""
        if not packet or stamp is None:
            return
        body = "\n".join(packet)
        outbound = src_port == PORT

        if outbound:
            response = RESPONSE.search(body)
            if not response:
                return
            client, client_port = dst, dst_port
            mark, what = RESPONSE_MARK, response.group(1)
            length, ctype = LENGTH.search(body), CTYPE.search(body)
            detail = " ".join(
                part for part in (
                    f"len={length.group(1)}" if length else "",
                    f"type={ctype.group(1).strip()}" if ctype else "",
                ) if part
            ) or "-"
        else:
            request = REQUEST.search(body)
            if not request:
                return
            client, client_port = src, src_port
            mark = REQUEST_MARK
            what = f"{request.group(1)} {request.group(2)}"
            agent = AGENT.search(body)
            detail = agent.group(1).strip() if agent else "-"

        # Keyed on the *client* end plus the arrow, never the sender's. tcpdump
        # prints sequence numbers relative to the start of each connection, so
        # every connection's first packet is "seq 1". Keying replies on the
        # sender would make that (server, 8419, 1) for every viewer at once, and
        # the second viewer's reply would be thrown away as a duplicate of the
        # first. The client's address and port name one connection, and the two
        # directions of that connection number themselves independently, so the
        # arrow has to be in the key too.
        seq = SEQ.search(header_line)
        key = (client, client_port, mark, seq.group(1) if seq else what)
        if not dedupe.is_new(key):
            return
        log.write(
            "\t".join([
                stamp.replace(" ", "T") + "Z",
                client or "?",
                client_port or "?",
                mark,
                what,
                detail,
            ])
        )

    try:
        for line in proc.stdout:
            match = HEADER.match(line)
            if match:
                flush()
                stamp = match.group(1)
                src, src_port = match.group(2), match.group(3)
                dst, dst_port = match.group(4), match.group(5)
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
