#!/usr/bin/env python3
"""Record when a channel's stream starts, stalls, restarts, or goes away.

Why this exists: the access log (tools/stream-access-log.py) shows what the
player asked for. This shows what the server had to give it. Read together they
separate "the player gave up" from "the player kept asking and the server had
nothing new" — which is the whole question when a picture freezes while the
daemon reports itself healthy.

It reads the generated playlists straight off the host filesystem rather than
over HTTP, and that is deliberate. Fetching ``/session/<N>/live.m3u8`` is not a
passive act: etv-next treats any request under ``/session`` as a viewer
arriving, refreshes the channel's heartbeat, and respawns the worker if it had
idled out (``crates/ersatztv/src/main.rs:382``). A watcher polling every
channel over HTTP would pin every channel to a live transcode forever and
quietly become the largest load on the box. Reading the files sees the same
bytes and changes nothing.

One tab-separated row per event, never per poll::

    2026-08-07T16:23:01Z<TAB>1<TAB>session-start<TAB>media_seq=0 disc_seq=0
    2026-08-07T16:23:44Z<TAB>1<TAB>stall<TAB>media_seq=10 stalled_for=21.4s
    2026-08-07T16:24:05Z<TAB>1<TAB>recovered<TAB>media_seq=13 stalled_for=42.1s

Events:

    session-start   a channel's playlist appeared (a viewer tuned in)
    session-end     the playlist vanished (worker idled out or died)
    restart         media sequence went backwards — a fresh session replaced a
                    running one underneath whoever was watching
    stall           media sequence stopped advancing past the threshold
    recovered       it advanced again
    discontinuity   the discontinuity counter rose (an encoder handoff)

Runs inside the container, started by docker/entrypoint.sh when
ETV_DIAG_CAPTURE is set. Reads the playlist files off disk rather than over
HTTP, because a request under /session makes the server spawn a worker.

Usage::

    tools/stream-watch.py
    HLS_ROOT=/data/hls INTERVAL=2 STALL_SECS=20 \
        LOG_FILE=/data/diag/stream-events.log \
        tools/stream-watch.py
"""

import os
import re
import sys
import time

HLS_ROOT = os.environ.get("HLS_ROOT", "/mnt/user/appdata/etv-station/data/hls")
LOG_FILE = os.environ.get(
    "LOG_FILE", "/mnt/user/appdata/etv-station/data/diag/stream-events.log"
)
INTERVAL = float(os.environ.get("INTERVAL", 2))
# A segment is 4 seconds, so a healthy channel advances well inside 20. Set
# this below 8 and ordinary scheduling jitter starts reading as a stall.
STALL_SECS = float(os.environ.get("STALL_SECS", 20))
# A worker that exits leaves its last live.m3u8 sitting on disk. Nothing ever
# deletes it, so a channel nobody has watched for a week still looks live if
# you only read the file's contents. Past this age the playlist is treated as a
# leftover rather than a running channel — otherwise every idle channel reports
# a session starting each time this watcher is restarted, and then stalls
# forever.
IDLE_SECS = float(os.environ.get("IDLE_SECS", 60))
MAX_BYTES = int(os.environ.get("MAX_BYTES", 10 * 1024 * 1024))

MEDIA_SEQ = re.compile(r"^#EXT-X-MEDIA-SEQUENCE:(\d+)", re.M)
DISC_SEQ = re.compile(r"^#EXT-X-DISCONTINUITY-SEQUENCE:(\d+)", re.M)


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


def read_playlist(channel):
    """Return (media_sequence, discontinuity_sequence) for a channel, or None
    if it is not live right now — no playlist, an unreadable one, or one whose
    last write is old enough that it is leftover rather than current. A channel
    nobody is watching is not an error, so all three read the same way."""
    path = os.path.join(HLS_ROOT, channel, "live.m3u8")
    try:
        if time.time() - os.path.getmtime(path) > IDLE_SECS:
            return None
        with open(path) as handle:
            body = handle.read()
    except OSError:
        return None
    match = MEDIA_SEQ.search(body)
    if not match:
        return None
    disc = DISC_SEQ.search(body)
    return int(match.group(1)), int(disc.group(1)) if disc else 0


def main():
    os.makedirs(os.path.dirname(LOG_FILE), exist_ok=True)
    log = RotatingLog(LOG_FILE, MAX_BYTES)

    def emit(channel, event, detail):
        stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        log.write(f"{stamp}\t{channel}\t{event}\t{detail}")

    # channel -> {seq, disc, since (monotonic clock when seq last moved), stalled}
    state = {}

    while True:
        try:
            channels = sorted(
                name for name in os.listdir(HLS_ROOT)
                if os.path.isdir(os.path.join(HLS_ROOT, name))
            )
        except OSError:
            channels = []

        now = time.monotonic()
        seen = set()

        for channel in channels:
            reading = read_playlist(channel)
            if reading is None:
                continue
            seq, disc = reading
            seen.add(channel)

            prior = state.get(channel)
            if prior is None:
                emit(channel, "session-start", f"media_seq={seq} disc_seq={disc}")
                state[channel] = {
                    "seq": seq, "disc": disc, "since": now, "stalled": False
                }
                continue

            if disc > prior["disc"]:
                emit(
                    channel,
                    "discontinuity",
                    f"disc_seq={prior['disc']}->{disc} media_seq={seq}",
                )
                prior["disc"] = disc

            if seq < prior["seq"]:
                # Media sequence only ever counts up within one session. Going
                # backwards means a new worker started writing over the same
                # folder while somebody was mid-playlist — the player is now
                # holding segment numbers that mean different video than they
                # did a second ago.
                emit(
                    channel,
                    "restart",
                    f"media_seq={prior['seq']}->{seq} disc_seq={disc}",
                )
                prior.update(seq=seq, disc=disc, since=now, stalled=False)
                continue

            if seq > prior["seq"]:
                if prior["stalled"]:
                    stalled_for = now - prior["since"]
                    emit(
                        channel,
                        "recovered",
                        f"media_seq={seq} stalled_for={stalled_for:.1f}s",
                    )
                prior.update(seq=seq, since=now, stalled=False)
            elif not prior["stalled"] and now - prior["since"] > STALL_SECS:
                stalled_for = now - prior["since"]
                emit(
                    channel,
                    "stall",
                    f"media_seq={seq} stalled_for={stalled_for:.1f}s",
                )
                prior["stalled"] = True

        for channel in list(state):
            if channel not in seen:
                emit(channel, "session-end", f"media_seq={state[channel]['seq']}")
                del state[channel]

        time.sleep(INTERVAL)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(0)
