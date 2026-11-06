#!/bin/sh
# Stand-in for the render broker. Usage: stub_broker.sh <delay seconds> <render path> <job json>
# Returns at once; after the delay a 2-second clip lands at <render path> (tmp +
# rename) carrying the tags ComfyUI and local-genai write: `prompt`, `workflow`
# and `lg_params`, the job's params as JSON.
delay=$1
out=$2
job=$3
params=$(printf %s "$job" | python3 -I -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["params"], sort_keys=True, separators=(",", ":")))')
(
  sleep "$delay"
  tmp="$out.part.mp4"
  ffmpeg -hide_banner -loglevel error -y -f lavfi -i "color=c=0x203040:s=320x180:r=25" \
    -t 2 -c:v libx264 -pix_fmt yuv420p -movflags +use_metadata_tags \
    -metadata "prompt={\"3\":{\"class_type\":\"KSampler\"}}" \
    -metadata "workflow={\"nodes\":[]}" \
    -metadata "lg_params=$params" "$tmp" && mv "$tmp" "$out"
) >/dev/null 2>&1 &
