---
name: check-channels
description: "Check if the ETV-next HLS channels are up and serving. Use when the user asks if channels are working, if the stream is live, or to verify the dev integration is healthy."
---

# Check Channels

## Dev server endpoints

ETV-next runs on `http://127.0.0.1:8409` when `./tools/dev-run.sh` is active.

| Channel | Number | HLS URL |
|---------|--------|---------|
| etv-station test | 1 | `http://127.0.0.1:8409/channel/1.m3u8` |
| Die Hard 24/7 | 2 | `http://127.0.0.1:8409/channel/2.m3u8` |
| Lineup (M3U) | — | `http://127.0.0.1:8409/channels.m3u` |

## Procedure

Do these steps. Run each Bash call as a single simple command (no `&&`, no `||`, no `$()`).

### 1. Check the lineup endpoint

```
curl -s -o /tmp/etv-lineup.m3u -w "%{http_code}" http://127.0.0.1:8409/channels.m3u
```

If the exit status is non-zero or the HTTP code is not 200, the dev server is not running. Tell the user to start it with `./tools/dev-run.sh` and stop here.

### 2. Check each HLS master playlist

For each channel number (1, 2):

```
curl -s -o /tmp/etv-ch1.m3u8 -w "%{http_code}" http://127.0.0.1:8409/channel/1.m3u8
curl -s -o /tmp/etv-ch2.m3u8 -w "%{http_code}" http://127.0.0.1:8409/channel/2.m3u8
```

A 200 response with an `.m3u8` body means the channel is responding. A 404 or timeout means the channel session failed to start.

### 3. Read the playlist files to confirm content

```
cat /tmp/etv-ch1.m3u8
cat /tmp/etv-ch2.m3u8
```

A valid master playlist starts with `#EXTM3U` and contains at least one `#EXT-X-STREAM-INF` line followed by a variant URL. Report what you see.

### 4. Follow one variant URL

From the master playlist, extract the first variant URL (it will be relative, like `1/index.m3u8`). Fetch it:

```
curl -s -o /tmp/etv-ch1-variant.m3u8 -w "%{http_code}" http://127.0.0.1:8409/1/index.m3u8
```

A valid variant playlist contains `#EXT-X-TARGETDURATION` and at least one `.ts` segment URL. This confirms ffmpeg is actually producing segments.

## Reporting

For each channel, report:
- **Up / Down** — did it respond with 200?
- **Playlist valid** — does the master playlist look well-formed?
- **Segments present** — did the variant playlist contain `.ts` segments?

If something is down, check the most recent `tmp/dev.*.log` (use the `read-logs` skill) to see why.
