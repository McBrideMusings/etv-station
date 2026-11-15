#!/bin/sh
# The chapter renderer's ffmpeg (ETV_RENDER_FFMPEG); the Dockerfile's
# render-ffmpeg stage says why it lives apart. Its own libraries first, then the
# image's LD_LIBRARY_PATH, which supplies the base's libva and libdrm.
LD_LIBRARY_PATH=/opt/render-ffmpeg/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
export LD_LIBRARY_PATH
exec /opt/render-ffmpeg/libexec/ffmpeg "$@"
