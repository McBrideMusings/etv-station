# Subtitles

How a channel gets subtitles onto a viewer's screen, and what the two ways of
doing it cost.

## The two modes

A channel picks one, in its config:

```json
"normalization": {
  "subtitle": {
    "mode": "convert",
    "language": { "tag": "en", "name": "English" }
  }
}
```

**`burn`** paints the subtitle words into the video picture before the frames
leave the server. Every viewer on that channel sees them and no player setting
can hide them, because by the time the video arrives the words are part of the
image. Works with any subtitle format, including the picture-based ones on DVDs
and Blu-rays.

**`convert`** pulls the subtitles out as text, turns them into WebVTT, and
serves them as a separate track the viewer can switch on and off. The video
picture is left alone. Only works on text subtitle formats — SRT, ASS, and the
like.

`burn` is the default when the config says nothing, which keeps the behaviour of
a config written before `convert` existed.

## What convert mode actually produces

For each `liveNNNNNN.ts` video segment written to the channel's output folder,
a matching `liveNNNNNN.vtt` is written beside it holding the subtitle lines that
fall in that segment's time range. A second playlist, `live_sub.m3u8`, lists
those `.vtt` files the same way `live.m3u8` lists the `.ts` files.

The channel's top-level playlist then ties them together:

```
#EXTM3U
#EXT-X-VERSION:6
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",DEFAULT=NO,AUTOSELECT=YES,FORCED=NO,LANGUAGE="en",URI="http://host/session/1/live_sub.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=4611200,SUBTITLES="subs"
http://host/session/1/live.m3u8
```

`DEFAULT=NO` means the player does not switch subtitles on by itself. The
viewer asks for them, or they stay off.

A channel in `burn` mode gets none of that — no `.vtt` files, no
`live_sub.m3u8`, and a plain `#EXT-X-STREAM-INF` line with no `SUBTITLES`
attribute. There is nothing to announce, so nothing is announced.

## Which subtitle track gets used

A playout item can name one, in its `tracks` block:

```json
"tracks": {
  "subtitle": { "stream_index": 3 }
}
```

If it does, that stream is used. If it does not — which is the ordinary case,
since a playout generator has to probe every file to fill that in — the first
subtitle stream in the file is used, preferring a text one over a picture one.
Text is preferred because text can go either way: it can be burned in *or*
converted. A picture subtitle can only be burned.

## Language

HLS gives a live stream one subtitle language for the whole session, so the
language is a property of the channel, not of whatever is playing right now. A
channel whose schedule runs an English film after a Spanish one still has to
pick one label. That is what `language.tag` and `language.name` are: `tag` goes
into `LANGUAGE=` and `name` into `NAME=`, which is the text a player shows in
its subtitle menu. Both default to English.

The picker does not yet match the source subtitle stream against that language —
it takes the first suitable one regardless. See issue #31.

## Known gap: disc rips lose their subtitles under convert

Blu-ray (PGS) and DVD (VobSub) subtitles are pictures, and a picture cannot be
written into a WebVTT file. Under `convert` those items currently play with no
subtitles at all rather than falling back to burning them in for that item. A
channel mixing disc rips with other sources therefore shows subtitles for part
of its schedule and none for the rest. See issue #30.
