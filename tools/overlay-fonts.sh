# Sourced by overlay-still.sh, overlay-test.sh and overlay-watch.sh: builds
# FONT_ARGS, the `--font <path>` list they hand to etv-overlay, so a preview
# renders with the same fonts the deployed station registers from `fonts:` in
# station.yaml.
#
# OVERLAY_FONTS is a colon-separated list of font files or directories. Unset,
# it is deploy/appdata/shared/fonts when that directory exists, else no fonts
# (the OS sans-serif renders every family).
FONT_ARGS=()
OVERLAY_FONTS="${OVERLAY_FONTS-}"
if [ -z "$OVERLAY_FONTS" ] && [ -d deploy/appdata/shared/fonts ]; then
  OVERLAY_FONTS="deploy/appdata/shared/fonts"
fi
if [ -n "$OVERLAY_FONTS" ]; then
  IFS=':' read -r -a FONT_PATHS <<< "$OVERLAY_FONTS"
  for FONT_PATH in "${FONT_PATHS[@]}"; do
    [ -n "$FONT_PATH" ] && FONT_ARGS+=(--font "$FONT_PATH")
  done
fi
