#!/usr/bin/env bash
# Draw the content-rating badges rating-badge.rhai shows, one 8-bit RGBA PNG per
# rating, into deploy/appdata/shared/ratings/.
#
# The PNGs are committed, so the station and the container never run this; it
# exists so the art can be regenerated or restyled, and so nobody has to wonder
# where the files came from.
#
#   ./tools/make-rating-icons.sh
#   RATING_FONT=/path/to/Bold.ttf ./tools/make-rating-icons.sh
#
# Plain boxes in the shape of the on-screen US icons: a near-black plate, a white
# frame, the code in bold white. Not the MPA's or the TV Parental Guidelines
# Oversight Monitoring Board's artwork — the codes are public, the drawings are
# ours. TV ratings are the code alone; film ratings add a small "RATED" line, the
# way a film's rating card reads.
#
# Drawn at 240x112 and shown at 120x56 (`height: 56` on the image layer), so the
# renderer area-averages down and the edges stay clean.
#
# Order matters outside this file: rating-badge.rhai addresses its layers by
# index in exactly the order of RATINGS below.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/deploy/appdata/shared/ratings"
FONT="${RATING_FONT:-/System/Library/Fonts/Supplemental/Arial Bold.ttf}"

if [ ! -f "$FONT" ]; then
  echo "make-rating-icons: font not found: $FONT (set RATING_FONT to a bold .ttf)" >&2
  exit 1
fi
mkdir -p "$OUT"

# file-stem|code shown|small line above the code ("" for none)
RATINGS=(
  "tv-y|TV-Y|"
  "tv-y7|TV-Y7|"
  "tv-g|TV-G|"
  "tv-pg|TV-PG|"
  "tv-14|TV-14|"
  "tv-ma|TV-MA|"
  "g|G|RATED"
  "pg|PG|RATED"
  "pg-13|PG-13|RATED"
  "r|R|RATED"
  "nc-17|NC-17|RATED"
)

for entry in "${RATINGS[@]}"; do
  IFS='|' read -r stem code caption <<<"$entry"
  if [ -n "$caption" ]; then
    text="drawtext=fontfile='$FONT':text='$caption':fontsize=26:fontcolor=white@0.85:x=(w-text_w)/2:y=20,drawtext=fontfile='$FONT':text='$code':fontsize=58:fontcolor=white:x=(w-text_w)/2:y=48"
  else
    text="drawtext=fontfile='$FONT':text='$code':fontsize=62:fontcolor=white:x=(w-text_w)/2:y=(h-text_h)/2+2"
  fi
  ffmpeg -loglevel error -y -f lavfi -i "color=c=black@0.88:s=240x112,format=rgba" \
    -vf "drawbox=x=0:y=0:w=240:h=112:color=white:t=6,$text" \
    -frames:v 1 "$OUT/$stem.png"
  echo "wrote $OUT/$stem.png"
done
