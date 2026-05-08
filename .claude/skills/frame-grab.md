---
name: frame-grab
description: "Capture a single video frame from a live ETV-next HLS stream and display it. Use when the user wants to see what a channel is currently showing, verify video is rendering, or confirm a specific program is on screen."
---

# Frame Grab

Capture a frame from a live HLS stream using `ffmpeg`, then display it inline using the Read tool (multimodal image support).

## Prerequisites

- `./tools/dev-run.sh` must be running
- `ffmpeg` must be installed (`which ffmpeg` to verify)

## Channel map

| Channel | Number | HLS URL |
|---------|--------|---------|
| etv-station test | 1 | `http://127.0.0.1:8409/channel/1.m3u8` |
| Die Hard 24/7 | 2 | `http://127.0.0.1:8409/channel/2.m3u8` |

## Procedure

### 1. Determine the channel

If the user specified a channel number or name, use it. If not, default to channel 1.

### 2. Capture the frame

The quickest way is to run the helper script, which handles timeouts and opens the image automatically:

```
CHANNEL=1 ./tools/frame-grab.sh
CHANNEL=2 ./tools/frame-grab.sh
```

To run ffmpeg directly, use `-rw_timeout 15000000` (15 seconds in microseconds) so it fails fast instead of hanging:

```
ffmpeg -y -rw_timeout 15000000 -i http://127.0.0.1:8409/channel/1.m3u8 -frames:v 1 -q:v 2 /tmp/etv-frame-ch1.jpg
ffmpeg -y -rw_timeout 15000000 -i http://127.0.0.1:8409/channel/2.m3u8 -frames:v 1 -q:v 2 /tmp/etv-frame-ch2.jpg
```

**If ffmpeg times out or errors:** The HLS stream may not be producing segments yet. Wait 10–15s after channel start and retry. Check dev logs with `read-logs` if it keeps failing.

**Note:** ffmpeg typically outputs several lines of version/codec info to stderr before the frame. This is normal. A successful grab exits 0 and produces the image file.

### 3. Display the frame

Use the Read tool on the output path:
- Channel 1: `/tmp/etv-frame-ch1.jpg`
- Channel 2: `/tmp/etv-frame-ch2.jpg`

The Read tool supports images — Claude will see the frame and can describe or verify its contents.

### 4. Report what's on screen

After viewing the image:
- Describe what is visible (content, any test patterns, color bars, black screen, actual video)
- Note if it looks like the right program based on what the playout JSON says should be airing
- Flag any obvious problems: solid black (ffmpeg ingested but no picture), green screen, corrupted blocks

## Multiple channels

If the user wants to check all channels, run frame grabs for each and report side by side.

## Troubleshooting

| Symptom | Likely cause |
|---------|-------------|
| `Connection refused` | Dev server not running — `./tools/dev-run.sh` |
| `Invalid data found` | Stream just started; wait 10–15s and retry |
| Black frame | ffmpeg got a segment but video is silent/dark — check lavfi test source config |
| `No such file or directory` on the output | ffmpeg crashed before writing — check its stderr output |
