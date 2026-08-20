---
name: stream-triage
description: "Diagnose a channel that froze, buffered, or stuttered for a real viewer — reading the server's three clocks (viewer, ffmpeg, overlay) and the tvOS client's logs. Use for 'the stream froze', 'it's buffering', 'channel N is stuck', or any playback complaint from a device."
user_invocable: true
---

# Triaging a frozen or buffering channel

The global `diagnose` skill owns the method — feedback loop, reproduce,
hypothesise, instrument, fix. This skill is the etv-station-specific half: which
clock to read, which command actually returns the truth, and which readings mean
nothing.

**Never hardcode host, device, or IP.** They are personal infrastructure and this
file is committed. `$UNRAID_HOST` / `$UNRAID_USER` come from `.env`; the Apple TV
is discovered with `xcrun devicectl list devices`; the viewer's IP is read out of
the access log. Every command below follows that rule.

## The one thing to internalise

**The outside symptom is symmetric.** When a channel freezes, ffmpeg's `out_time`
and frame counter stop advancing, its stderr stays empty, and ~60s later
ETV-next kills the session with exit 75. That looks *identical* whether:

- the overlay stopped producing frames, starving ffmpeg; or
- ffmpeg stopped consuming them, blocking the overlay; or
- ffmpeg's input file read stalled and neither pipe is at fault.

Do not guess between these. There is instrumentation that rules; use it.

## Step 0 — resolve the channel id

ETV-next channel ids are **not** the station folder numbers. `032-action` is
channel 10, `033-kung-fu` is 11. Everything below — HLS dirs, log lines, capture
— uses the ETV-next id.

```sh
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "docker exec etv-station curl -s http://127.0.0.1:8409/channels.m3u" \
  | grep -i '<folder-fragment>' \
  | sed -E 's/.*tvg-id="ersatztv\.([0-9]+)".*tvg-name="([^"]+)".*/\1 \2/'
```

Prints `id name`, e.g. `11 033-kung-fu`. Drop the `grep` to list every channel.

## Step 1 — is it broken *now*?

```sh
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "docker exec etv-station sh -c 'ls -t /config/data/hls/<N>/ | head -3; date -u +%FT%TZ'"
```

Run it twice, ~6s apart. New `.ts` files means the transcode is alive and the
problem is client-side or already over.

> **Read live state through `docker exec`, never host-side `ls` on `/mnt/user`.**
> `/mnt/user` is shfs (FUSE) and its attribute cache serves mtimes hours stale.
> A healthy channel read host-side once looked 4 hours dead. This single mistake
> costs the most time of anything in this file.

## Step 2 — the three clocks

| Clock | Where | Healthy looks like |
|---|---|---|
| Viewer | `data/diag/access.log` | steady `GET /session/<N>/liveNNNNNN.ts 200/206` |
| ffmpeg | container log, `ffmpeg_progress` | `out_time` advancing 1:1 with wall clock, `fps` ≈ source fps |
| Overlay | `data/playout/<folder>/overlay.heartbeat` | `frames_written` climbing |

```sh
# ffmpeg clock
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "docker logs --since 5m etv-station 2>&1 | grep 'channel <N>' | grep ffmpeg_progress | tail -3"

# overlay clock (sample twice; a single sample proves nothing — see below)
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "docker exec etv-station cat /data/playout/<folder>/overlay.heartbeat"
```

### The stall signature

```
channel <N> terminated after ffmpeg stall
channel <N> exited with status exit status: 75
channel <N> exited while a viewer was watching; consecutive failures now K
```

Exit 75 is the stall detector, not a crash. Watch `K`: after enough consecutive
failures ETV-next gives up and logs `channel <N> is failed; serving ended
playlist to viewer`. **That** is the hard freeze a viewer never recovers from —
an ordinary single stall self-heals in ~4s via respawn, and the viewer usually
only sees a hiccup.

A respawn resets `EXT-X-MEDIA-SEQUENCE` and bumps `EXT-X-DISCONTINUITY-SEQUENCE`.
Many clients do not survive that even when the server is healthy again, so
"server recovered, screen still frozen" is expected — tell the user to re-tune.

## Step 3 — rule on which side stopped

```sh
# already installed at /mnt/user/appdata/etv-station/two-clock-capture.sh
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "bash /mnt/user/appdata/etv-station/two-clock-capture.sh <N> --gap 12"
```

Arm it *before* the freeze; it polls the segment index and captures on a gap.
Results land in `data/diag/two-clock-<N>.log`. Verdicts:

| Verdict | Means | Go look at |
|---|---|---|
| `ffmpeg-stopped-first` | overlay frames frozen while blocked in `pipe_write` | ETV-next's consumer side |
| `overlay-stopped-first` | overlay frames frozen off-pipe | the heartbeat `phase` names which phase hung |
| `overlay-alive` | overlay still feeding; freeze is downstream | ffmpeg's input read — the array, disk spin-up, `/mnt/user` pressure |
| `mutual-pipe-block` | both wedged on the fifo | genuine deadlock, the most interesting case |

`--self-test` exercises the verdict logic with no host and no freeze. Run it
after editing the script.

## Readings that mean nothing (do not re-derive these)

- **`pipe_write` alone is not evidence.** One 1280x720 rgba frame is 3.5MB
  against a 64KB pipe buffer, so a *healthy* overlay sits inside `write_all`
  almost all the time. Measured healthy `phase_age_ms` is 16-241ms. The
  discriminator is `frames_written` standing still across two samples.
- **Missing `eof_action` on `overlay_vaapi` is a dead end.** Software `overlay`
  sets `eof_action=pass` and the hardware variants don't, which looks damning.
  But framesync's default is `repeat`, and a writer that *closes* the fifo lets
  ffmpeg finish cleanly — proven by the `close` arm of the repro. Only a writer
  that goes quiet *while holding the fifo open* wedges anything.
- **A short overlay hiccup is survivable.** The `resume` arm shows ffmpeg catches
  up fully after a pause. A freeze needs **≥60s of continuous silence** to trip
  the detector, so "it paused briefly at an item boundary" does not explain a
  stall.

## The container log window is short

`ffmpeg_progress` logs once per second per channel. With six channels that is
~21,600 lines/hour, and the docker buffer holds roughly **2.8 hours** even
though the container may have been up for days. Check before concluding anything
about rates or onset:

```sh
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "docker inspect etv-station --format '{{.State.StartedAt}}'; docker logs etv-station 2>&1 | head -1 | cut -c1-40"
```

If the oldest line is much newer than `StartedAt`, you cannot speak to anything
earlier. `data/diag/access.log` and `access.log.1` go back further.

## Step 4 — the client side

### Without touching the device (do this first)

The access log is the viewer's clock and needs no device access. It records IP,
User-Agent, path, status and latency.

```sh
# who is watching, and with what
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "docker logs etv-station 2>&1 | grep -oE 'access: [0-9.]+:[0-9]+ .*\"[^\"]+\"$' \
   | sed -E 's/access: ([0-9.]+):[0-9]+ .*\"([^\"]+)\"$/\1 \2/' | sort | uniq -c | sort -rn | head"
```

Internal `Lavf/...` from the docker bridge is the soak probe, not a person. Real
tvOS clients show as `iPlayTV/...` or `VLC/... LibVLC/...`.

Then follow that IP through the log around the freeze:

```sh
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "grep '<viewer-ip>' /mnt/user/appdata/etv-station/data/diag/access.log | tail -40"
```

Read it for:

- **requests stop entirely** → the client gave up; it will not come back without
  a re-tune.
- **same segment requested repeatedly** → the client is retrying; the playlist
  moved under it (discontinuity) or the segment 404s.
- **requests continue, latency climbs** → buffering, not a freeze. Look at
  segment size and whether `fps` fell below realtime.
- **`404`** on a segment the client wants → it fell behind the sliding window.

### On the Apple TV

Discover it rather than assuming:

```sh
xcrun devicectl list devices
xcrun devicectl device info details --device <UDID> | grep -iE 'osVersionNumber|productType|developerModeStatus'
```

**Verified working** on this setup: the office Apple TV 4K (3rd gen) shows as
`available (paired)` with developer mode enabled, and `info details` returns its
tvOS version.

**Verified NOT working — do not reach for these:**

- `log stream --device-name <name>` — the `log` CLI on this macOS rejects
  `--device-name` outright. There is no live unified-log stream from tvOS here.
- `devicectl device process list` — no such subcommand.

What is available:

```sh
xcrun devicectl device sysdiagnose --device <UDID> --output <dir>
```

A sysdiagnose is the honest path to on-device logs; it is slow and large, so
take one only when the access log has already ruled the server out. Console.app
also shows a paired device, but prefer the programmatic route — driving the GUI
takes the user's focus.

Neither iPlayTV nor VLC for tvOS exposes a documented log subsystem worth
grepping for, so treat the device as a last resort. **In practice the access log
answers the client question faster and with less ceremony.**

## Local reproduction

Neither of these needs the host.

```sh
./tools/overlay-stall-repro.sh --dur 15 --write-secs 5 --pause-secs 8
./tools/overlay-heartbeat-check.sh
```

The first proves a silent writer wedges ffmpeg (`stall` arm), a closing writer
does not (`close`), and a resuming one recovers (`resume`). The second proves
the overlay's own clock reports correctly when the *reader* stops. Both exit
non-zero on failure, so they work as pass/fail loops.

## Escalation checklist

Work down; stop when one answers it.

1. Is it broken now, read through `docker exec`? (Step 1)
2. Does the container log show exit 75, and what is `consecutive failures`?
3. Did the client stop requesting, or keep retrying? (access log)
4. What did `two-clock-capture` rule, if armed?
5. Is `/mnt/user` under pressure, or are there disk errors in `dmesg`?
6. How many channels are transcoding at once, and is any below realtime `fps`?

## Related

- `verify-project` — booting the daemon locally and the freeze-tool table.
- `crates/etv-overlay/src/phase_watchdog.rs` — the overlay clock's implementation.
- Global `diagnose` — the method this skill plugs into.
