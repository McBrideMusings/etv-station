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
| ffmpeg | `data/diag/ffmpeg-progress.log` | `out_time` advancing 1:1 with wall clock, `fps` ≈ source fps |
| Overlay | `data/playout/<folder>/overlay.heartbeat` | `frames_written` climbing |

```sh
# ffmpeg clock (its own file — NOT the container log; see below)
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "docker exec etv-station sh -c 'grep \"channel <N>:\" /data/diag/ffmpeg-progress.log | tail -3'"

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

### The reburst signature — a restart that is not a stall

```
lead down to 18s while still playing; restarting the item to rebuild the buffer
resuming the same item from 2026-08-21 20:31:18.460403686 +00:00:00
```

This is `reburst_decision` in
`vendor/etv-next/crates/ersatztv-channel/src/channel_session.rs` deliberately
killing a *healthy* ffmpeg because its lead drained below 20s, then resuming the
same item from the segment frontier. Exit 75 is absent — do not read it as a
stall.

Pair each one with the `-t` the replacement was handed:

```sh
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "grep -A1 -x -- '-t' /mnt/user/appdata/etv-station/data/diag/ffmpeg-argv-ch<N>.log | grep -E '^[0-9]+ms$' | tr -d 'ms' | sort -n | head -3"
```

Anything under 4000ms is shorter than one HLS segment and should no longer
happen — #339 gated the reburst on `remaining > REBURST_AT_LEAD`. A sliver
reappearing means that gate regressed.

## Step 3 — rule on which side stopped

**There is nothing to arm.** The capture runs inside the container, started by
`docker/entrypoint.sh`, watching every channel that has a running transcode. It
comes back with the container and it is not behind `ETV_DIAG_CAPTURE` — a freeze
is unannounced, so a capture you have to remember to switch on is off when it
matters. Just read what it already recorded:

```sh
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "docker exec etv-station sh -c 'grep -A6 \"channel=<N>\" /data/diag/two-clock.log | tail -40'"
```

Confirm it is alive with `docker logs etv-station 2>&1 | grep two-clock`, which
should show `two-clock freeze capture started`. Verdicts:

| Verdict | Means | Go look at |
|---|---|---|
| `ffmpeg-stopped-first` | overlay frames frozen while blocked in `pipe_write` | ETV-next's consumer side |
| `overlay-stopped-first` | overlay frames frozen off-pipe | the heartbeat `phase` names which phase hung |
| `overlay-alive` | overlay still feeding; freeze is downstream | ffmpeg's input read — the array, disk spin-up, `/mnt/user` pressure |
| `mutual-pipe-block` | both wedged on the fifo | genuine deadlock, the most interesting case |
| `no-overlay-ffmpeg-futex` | no overlay in the graph; ffmpeg parked on a futex | contention or an internal lock — read the `cpu_ticks` delta on the same capture |
| `no-overlay-but-on-a-pipe` | no overlay, yet blocked on a pipe | some other pipe in the graph |
| `no-overlay-ffmpeg-elsewhere` | no overlay; blocked outside a pipe or futex | the input read or the output write |
| `no-overlay-ffmpeg-gone` | session already torn down | nothing to attribute |

The `no-overlay-*` classes are not rare, but **do not assume them**. That was true
on 2026-08-21 morning (#295) and had reversed by that evening: all six live
transcodes carried `[1:0]hwupload[v_s0];[v_m0][v_s0]overlay_vaapi` in their
filter_complex. Read the current graph out of `ffmpeg-argv-ch<N>.log` rather than
trusting either state. `cpu_ticks x -> y (advanced=yes)` on a futex means contention or
a spin; `advanced=no` means a genuine wedge. Same wchan, opposite meanings.

`--self-test` exercises the verdict logic with no host and no freeze. Run it
after editing the script — locally, or in the container with
`docker exec etv-station two-clock-capture.sh --self-test`.

A channel with no ffmpeg is skipped rather than reported: its HLS folder keeps
the last session's segments forever, so an idle channel would otherwise look
permanently frozen.

## Step 3b — attribute it in one command (#327)

`tools/attribute-stalls.sh` answers "which side stopped first" for every exit-75
kill in the container log, without ssh'ing around by hand:

```sh
tools/attribute-stalls.sh              # every stall
tools/attribute-stalls.sh --channel 4  # one channel
```

It cross-checks two independent clocks. The **overrun** comes from the argv log:
an invocation handed `-t X` at time `T` should exit at `T+X`, so `kill - (T+X)`
says how long ffmpeg outlived its own assignment. The **flatline** comes from the
progress stream: how long `frame=` sat unchanged before the kill. When those two
agree, the verdict is solid.

### What it found on 2026-08-22, and why it settles #327

**54 of 59 stalls over 13 hours were `COMPLETED-BUT-WOULD-NOT-EXIT`** — across
channels 1, 2, 4, 5, 9 and 30. 2 were `ENCODER-STOPPED-FIRST` (a genuine
mid-item wedge), 0 were `CONSUMER-STOPPED-FIRST`, and 3 had no progress data.

So **neither** of the two hypotheses in the issue is what usually happens.
ffmpeg does not stop producing partway through an item, and it does not keep
producing while its output fails to land. It encodes **exactly** the duration it
was assigned, reaches the last frame, and then does not terminate. 60s later the
stall detector kills it with exit 75, which is `STALL_THRESHOLD` doing its job.

The two clocks agree to the second, which is what makes it airtight — e.g.
channel 4 killed at 02:49:49Z hit its `-t` of 1262480ms at 02:48:50Z: 59s of
overrun against 59s of frame flatline. Channel 30's `-t 11008ms` case (frame=264
= the whole assignment, killed 68s later) is not the outlier it looked like; it
is the general case at small scale.

Consequences for anyone triaging this:

- **`exit status: 75` is mostly an item-boundary event, not a mid-item freeze.**
  Expect roughly one per item per channel, clustering at item ends.
- **A viewer's freeze is the respawn, not the encode.** The encode already
  delivered every frame of the item before the kill.
- **Do not go looking for a starved pipe or a slow array read.** Channels 4 and 5
  have no overlay and no pipe in the media path at all — file in, file out — and
  they stall at the same rate as the channels that do.
- **The two-clock `no-overlay-ffmpeg-futex` verdicts are consistent with this.**
  All 21-22 threads parked on futexes with `cpu_ticks advanced=no` is what a
  process with no work left and a shutdown that never completes looks like.

### Reading the verdicts

| Verdict | Means |
|---|---|
| `COMPLETED-BUT-WOULD-NOT-EXIT` | reached its `-t`, then hung. The #327 majority |
| `ENCODER-STOPPED-FIRST` | frames stopped with the item's end still far off |
| `CONSUMER-STOPPED-FIRST` | ffmpeg still emitting frames at the kill |
| `NO-PROGRESS-DATA` | instrumentation gap, not an unattributable stall |

`[per-session]` / `[rotated]` / `[argv-only]` names which source backed the
flatline. Images older than `b6f5ac8` have per-session
`ffmpeg-progress-ch<N>-*.log` files and an empty rotated log, because that
probe's own `-progress` replaced ETV-next's; newer images have the reverse. The
script reads whichever is present, so it works either side of that deploy.

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

## Where the progress history lives

`ffmpeg_progress` no longer goes to the Docker log driver. It logs once per
second per channel — ~21,600 lines/hour at six channels — and used to hold the
buffer down to ~2.8 hours of history even after days of uptime, so the record
leading up to a freeze was routinely gone before anyone looked. It now has its
own rotated file:

```sh
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "docker exec etv-station sh -c 'grep \"channel <N>:\" /data/diag/ffmpeg-progress.log | tail -20'"
```

Rolled files are `ffmpeg-progress.log.1`, `.2`. Size and count are
`ETV_PROGRESS_LOG_MAX_BYTES` (64MB) and `ETV_PROGRESS_LOG_KEEP` (2).

The Docker log still holds everything else, but check its span before claiming
anything about rates or onset — that mistake has been made in this repo:

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
# segment fetches only — this is the client's real clock
ssh "$UNRAID_USER@$UNRAID_HOST" \
  "grep '/session/<N>/live0' /mnt/user/appdata/etv-station/data/diag/access.log \
   | grep '<viewer-ip>' | grep '>' | tail -20"
```

> **VLC uses two sockets, and reading the wrong one inverts the answer.**
> One connection polls `live.m3u8`, a *different* one fetches `.ts` segments
> (observed: `52889` polling, `52888` fetching). A plain `tail` of the access log
> can show nothing but playlist requests and look exactly like a client that has
> stopped asking for media when it is streaming fine. **Filter by request path,
> never by recency.** This produced a confidently wrong diagnosis on 2026-08-21,
> including a retracted claim about sequence regression.

Read it for:

- **playlist polls continue but segment fetches stop** → the client froze while
  the server stayed healthy. Confirm by checking the encoder over the same window
  in `ffmpeg-progress.log` and that `two-clock.log` has no capture for that
  channel. Proven case: 2026-08-21 channel 10, 92s client gap with `out_time`
  advancing 1:1 and `fps` steady at 24.03 throughout (#265). Detecting this
  automatically is #328.
- **requests stop entirely, playlist included** → the client gave up; it will not
  come back without a re-tune.
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
./tools/progress-split-check.sh
```

The first proves a silent writer wedges ffmpeg (`stall` arm), a closing writer
does not (`close`), and a resuming one recovers (`resume`). The second proves
the overlay's own clock reports correctly when the *reader* stops. The third
covers the progress-log split and its rotation. All three exit non-zero on
failure, so they work as pass/fail loops.

## Escalation checklist

Work down; stop when one answers it.

1. Is it broken now, read through `docker exec`? (Step 1)
2. Does the container log show exit 75, and what is `consecutive failures`?
   Run `tools/attribute-stalls.sh` before theorising — most exit-75 kills are
   ffmpeg refusing to exit at the end of its item (Step 3b), not a freeze.
3. Did the client stop requesting, or keep retrying? (access log)
4. What did `two-clock-capture` rule, if armed?
5. Is `/mnt/user` under pressure, or are there disk errors in `dmesg`?
6. How many channels are transcoding at once, and is any below realtime `fps`?

## Related

- `verify-project` — booting the daemon locally and the freeze-tool table.
- `crates/etv-overlay/src/phase_watchdog.rs` — the overlay clock's implementation.
- Global `diagnose` — the method this skill plugs into.
