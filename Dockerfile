# syntax=docker/dockerfile:1
#
# One image, both halves of the stack: the etv-station daemon (plus the
# etv-overlay renderer it supervises) and the ErsatzTV-next server that streams
# what the daemon writes. They are packaged together because the playout folder
# is the entire interface between them — two containers would need that folder
# shared, kept in sync, and started in the right order, whereas in one container
# it is just a directory both processes see. The container takes the station
# config (and whatever env it needs to reach Plex or the media roots) and serves
# HLS + XMLTV; nothing else has to be wired up.
#
# Dependency compilation is cached via cargo-chef. The `planner` stage emits a
# recipe describing the dependency graph; the `builder` stage cooks just those
# dependencies in a layer that survives source-only changes. See the cook step
# for how the etv-next path dependency is handled.

# ---- chef ----
# Shared base: the pinned toolchain plus cargo-chef. Edition 2024 requires
# Rust >= 1.85.
#
# The Rust stages run natively on the build machine's own architecture
# ($BUILDPLATFORM) and cross-compile to x86_64-unknown-linux-gnu, the runtime's,
# so their output is under target/x86_64-unknown-linux-gnu/release. On an amd64
# build machine the x86_64 toolchain is its native one.
#
# Every cargo run takes its job count from docker/cargo-jobs.sh, which sizes it
# to the build machine's memory and CPUs; the CARGO_BUILD_JOBS build arg
# overrides it.
FROM --platform=$BUILDPLATFORM rust:1.93-bookworm AS chef
ARG BUILDARCH

# cargo-chef's own release binary for the build machine, pinned by its sha256,
# rather than `cargo install`, which compiles ~200 crates on every cold build.
RUN case "$BUILDARCH" in \
        arm64) triple=aarch64-unknown-linux-gnu; sum=a47e13fba89c2895f5a5c3d0844acd2a5fd416eceb3a6f9dfb26e28155099f4e ;; \
        amd64) triple=x86_64-unknown-linux-gnu; sum=70ef940ef90d04d122f0176fdb8d6c39069191b484a1eaa29b327370c2e1c3c0 ;; \
        *) echo "no cargo-chef binary for $BUILDARCH" >&2; exit 1 ;; \
    esac \
    && curl -fsSLo /tmp/chef.tar.xz "https://github.com/LukeMathWalker/cargo-chef/releases/download/v0.1.78/cargo-chef-$triple.tar.xz" \
    && echo "$sum  /tmp/chef.tar.xz" | sha256sum -c - \
    && tar -xJf /tmp/chef.tar.xz -C /tmp \
    && install -m 755 "/tmp/cargo-chef-$triple/cargo-chef" /usr/local/cargo/bin/cargo-chef \
    && rm -rf /tmp/chef.tar.xz "/tmp/cargo-chef-$triple"
WORKDIR /build

# Build dependencies for the graphics stack pulled in transitively by
# etv-overlay (vello + wgpu + parley), plus the x86_64 C toolchain the cross
# build links with. fontconfig is the target's (amd64) library, because it is
# linked into the binaries; cmake and pkg-config run on the build machine.
# Shared by the `builder` stage, which compiles etv-station (a dependent of
# etv-overlay for overlay-spec validation), and the `etv-builder` stage below,
# which compiles ersatztv-channel (a dependent of etv-overlay for its
# diagnostic card).
RUN dpkg --add-architecture amd64 \
    && apt-get update && apt-get install -y --no-install-recommends \
        cmake \
        pkg-config \
        libfontconfig1-dev:amd64 \
        $( [ "$BUILDARCH" = amd64 ] || echo gcc-x86-64-linux-gnu g++-x86-64-linux-gnu libc6-dev-amd64-cross ) \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-gnu
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    CXX_x86_64_unknown_linux_gnu=x86_64-linux-gnu-g++ \
    AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar \
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_PATH_x86_64_unknown_linux_gnu=/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig
COPY --chmod=755 docker/cargo-jobs.sh /usr/local/bin/cargo-jobs

# ---- planner ----
# Reads the whole workspace (including the ersatztv-playout path dep under
# vendor/etv-next) and writes recipe.json — the dependency graph, no sources.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- builder ----
FROM chef AS builder
ARG CARGO_BUILD_JOBS

# Cook only the dependencies from the recipe. This layer is cached across source
# changes — only a Cargo.toml/Cargo.lock change busts it, so iterating on the
# crates no longer recompiles the heavy vello / wgpu / parley / cranelift graph.
# --locked cooks against the committed Cargo.lock; the -p filters match the real
# build below so only the deployed crates' dependencies are compiled.
#
# ersatztv-playout is a path dependency under vendor/etv-next, which the
# top-level Cargo.toml excludes from this workspace. cargo-chef only reconstructs
# skeleton crates for workspace members, so it does NOT recreate the vendored
# path dep — cook would fail to resolve it. Copy the real tree in first so cargo
# can resolve and build it. The vendored tree only moves when upstream is
# absorbed, so it belongs in this cached dependency layer; an upstream absorb is
# the only thing besides a Cargo.lock change that busts the cache.
COPY --from=planner /build/recipe.json recipe.json
COPY vendor/etv-next vendor/etv-next
RUN jobs=$(cargo-jobs) && CARGO_BUILD_JOBS=$jobs cargo chef cook --release --locked --target x86_64-unknown-linux-gnu --recipe-path recipe.json -p etv-station -p etv-overlay

# Now the real build. The whole workspace is needed: ersatztv-playout is a path
# dependency under vendor/etv-next, and etv-overlay is a sibling crate.
# .dockerignore keeps the context small (no target/, .git/, docs build output).
# The dependency layer cooked above is reused, so this recompiles only the
# workspace crates. etv-query-test (the Phase A CEL harness) is a dev tool and
# intentionally left out of the deployed image.
COPY . .
RUN jobs=$(cargo-jobs) && CARGO_BUILD_JOBS=$jobs cargo build --release --locked --target x86_64-unknown-linux-gnu -p etv-station -p etv-overlay

# ---- etv-builder ----
# ErsatzTV-next is its own cargo workspace under vendor/, so it builds on its own
# here rather than as part of the station workspace above. `ersatztv-channel` is
# built alongside the server because the server looks for it as a sibling
# executable when it spawns a channel session.
#
# vendor/etv-next/Cargo.toml declares etv-overlay as a path dependency
# (`../../crates/etv-overlay`, for the diagnostic card ersatztv-channel renders
# in place of black/silence) — relative to vendor/etv-next/, so it must land at
# the SAME nesting depth it has in the real repo, two directories up from
# vendor/etv-next itself. Copying vendor/etv-next straight to /build/etv-next
# (one level shallower) resolves that path to a location that does not exist.
#
# etv-overlay's own Cargo.toml uses `{ workspace = true }` dependencies, which
# cargo resolves against the nearest ancestor [workspace] — the *root*
# Cargo.toml, since crates/etv-overlay is a member of that workspace, not of
# vendor/etv-next's. So the root manifest has to be present too. Cargo does not
# require the root workspace's other members (crates/etv-station,
# crates/etv-query-test) to exist for this — only the
# path actually walked, crates/etv-overlay.
#
# This layer is keyed on vendor/etv-next AND crates/etv-overlay: either one is
# a real build input now (etv-overlay is a dependency of ersatztv-channel), so
# both are what should bust the cache — ordinary station-side changes outside
# those two trees still don't.
FROM chef AS etv-builder
ARG CARGO_BUILD_JOBS
COPY Cargo.toml Cargo.toml
COPY crates/etv-overlay crates/etv-overlay
COPY vendor/etv-next vendor/etv-next
WORKDIR /build/vendor/etv-next
RUN jobs=$(cargo-jobs) && CARGO_BUILD_JOBS=$jobs cargo build --release --locked --target x86_64-unknown-linux-gnu --bin ersatztv --bin ersatztv-channel

# ---- render-ffmpeg ----
# The chapter renderer's ffmpeg. Tempo changes need the rubberband filter, and
# the base image's ffmpeg is built without librubberband. Ubuntu's ffmpeg
# package has it (and h264_vaapi), and the runtime is Ubuntu 24.04 too, so this
# stage is the same release. It is NOT installed into the runtime with apt: the
# base image sets LD_LIBRARY_PATH to /usr/local/lib/x86_64-linux-gnu and then
# /usr/lib/x86_64-linux-gnu, and Ubuntu's libx264, libass, libdav1d, libzimg and
# others landing in /usr/lib would replace the base's own builds of them under
# the playback ffmpeg. Instead the binary and its libraries go to
# /opt/render-ffmpeg, which only docker/render-ffmpeg.sh puts on the library
# path. Left out: glibc, which the runtime has at the same version, and libva,
# libva-drm and libdrm, which must be the base's builds because its VAAPI
# drivers are built against them. Ubuntu's libva-x11, which the base lacks, is
# copied and loads against the base's libva. The renderer's ffmpeg is therefore
# 6.1, not the base's 8.1; the Health tab shows its version line.
FROM ubuntu:24.04@sha256:2198e4c65076e9cb0b3d695219df667c0e3b0072da64ca3c2648aca17dcecaf5 AS render-ffmpeg
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg \
    && rm -rf /var/lib/apt/lists/*
RUN ! ldd /usr/bin/ffmpeg | grep -q 'not found' \
    && mkdir -p /opt/render-ffmpeg/lib /opt/render-ffmpeg/libexec \
    && cp /usr/bin/ffmpeg /opt/render-ffmpeg/libexec/ffmpeg \
    && ldd /usr/bin/ffmpeg | awk '$3 ~ /^\// { print $3 }' \
        | grep -vE '/(libc|libm|libmvec|libpthread|libdl|librt|libresolv|ld-linux-x86-64|libva|libva-drm|libdrm)\.so\.[0-9]+$' \
        | xargs -I{} cp -L {} /opt/render-ffmpeg/lib/

# ---- runtime ----
# ErsatzTV's own ffmpeg image: it carries the ffmpeg/ffprobe build ETV-next
# expects for streaming, and the same ffprobe covers the station daemon's
# duration probing.
FROM ghcr.io/ersatztv/ersatztv-ffmpeg:8.1.2 AS runtime

# tini reaps zombies and forwards signals — the entrypoint runs two long-lived
# children, so PID 1 has real supervision work to do. libvulkan1 +
# mesa-vulkan-drivers give etv-overlay a software Vulkan (lavapipe), so overlay
# rendering works on a headless host with no GPU; channels without an overlay
# never spawn it and don't exercise these. iproute2 is here for `ss`: asking
# whether a frozen player still holds a connection open is a question only the
# server's own network namespace can answer, and without it the shell answers
# "command not found" on stderr and exits 0, which reads as "no connections".
#
# tcpdump + python3 are the same argument taken one step further, and they are
# why the diagnostics now live in here rather than as loose scripts copied onto
# the Unraid host. Off unless ETV_DIAG_CAPTURE says otherwise, so an ordinary
# run pays only the image size.
# curl + libxml2-utils (xmllint) are here for tools/soak-probe.sh (#297): it
# probes localhost the same way tools/verify-integration.sh does on a dev
# host, and neither is otherwise part of this runtime image.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        iproute2 \
        libcap2-bin \
        libvulkan1 \
        libxml2-utils \
        mesa-vulkan-drivers \
        python3 \
        python3-venv \
        tcpdump \
        tini \
    && rm -rf /var/lib/apt/lists/*

# The chapter renderer's stem separation: Demucs on CPU PyTorch, in its own
# virtualenv so the system python3 the diagnostics use is untouched.
# ETV_RENDER_PYTHON points the renderer at it; the Health tab reports Demucs and
# PyTorch as detected or missing.
RUN python3 -m venv /opt/render-venv \
    && /opt/render-venv/bin/pip install --no-cache-dir \
        --index-url https://download.pytorch.org/whl/cpu torch torchaudio \
    && /opt/render-venv/bin/pip install --no-cache-dir demucs soundfile
ENV ETV_RENDER_PYTHON=/opt/render-venv/bin/python

# The renderer's own ffmpeg (see the render-ffmpeg stage); the playback
# channels keep /usr/local/bin/ffmpeg. The build fails here when the copied ffmpeg cannot load a library, lacks rubberband, or
# cannot run the renderer's software path: a rubberband stretch whose tempo an
# asendcmd changes, encoded with libx264 and aac. The VAAPI encode needs the
# host's GPU, so only the deployed container can run it. The Health tab reports
# the same detection at run time.
COPY --from=render-ffmpeg /opt/render-ffmpeg /opt/render-ffmpeg
COPY --chmod=755 docker/render-ffmpeg.sh /opt/render-ffmpeg/bin/ffmpeg
RUN /opt/render-ffmpeg/bin/ffmpeg -hide_banner -filters | grep -qE '^ [.A-Z]{3} rubberband ' \
    && /opt/render-ffmpeg/bin/ffmpeg -hide_banner -v error \
        -f lavfi -i sine=frequency=440:duration=2 -f lavfi -i testsrc2=size=320x240:rate=30:duration=2 \
        -filter_complex "[0:a]asendcmd=c='1.0 rubberband@r tempo 1.2',rubberband@r=tempo=1.1:pitch=1.0[a]" \
        -map "[a]" -map 1:v -c:v libx264 -c:a aac -f mp4 /dev/null
ENV ETV_RENDER_FFMPEG=/opt/render-ffmpeg/bin/ffmpeg

# The container runs as uid 1000, and a plain non-root process cannot open the
# packet socket tcpdump needs. A file capability grants that one binary the one
# capability, which is narrower than either running the whole container as root
# or handing it --cap-add on the command line.
#
# cap_net_raw only — deliberately not cap_net_admin. NET_RAW is in Docker's
# default capability set, so this works on an unmodified `docker run`; NET_ADMIN
# is not, and needing it would put a flag in the Unraid run config that a future
# container recreate could quietly drop. The cost is that tcpdump cannot go
# promiscuous, which is why the capture passes -p. Nothing is lost: every packet
# this cares about is addressed to this container anyway.
RUN setcap cap_net_raw+eip "$(command -v tcpdump)"

# ETV-next burns subtitles with libass, which resolves fonts through fontconfig.
ENV FONTCONFIG_PATH=/etc/fonts
RUN fc-cache -f

# One fixed non-root uid/gid for both processes. They share the playout folder,
# so a single owner is the only arrangement where neither can write files the
# other cannot read. Neither needs root: they read config and write playout
# JSON, sidecars, the overlay fifo, and the HLS working set.
RUN groupadd --system --gid 1000 etv \
    && useradd --system --uid 1000 --gid 1000 --no-create-home etv

# The daemon resolves the overlay binary next to its own executable
# (overlay_supervisor::overlay_binary_path) and ETV-next resolves
# ersatztv-channel next to its own, so all four sit in one directory.
COPY --from=builder /build/target/x86_64-unknown-linux-gnu/release/etv-station /usr/local/bin/etv-station
COPY --from=builder /build/target/x86_64-unknown-linux-gnu/release/etv-overlay /usr/local/bin/etv-overlay
COPY --from=etv-builder /build/vendor/etv-next/target/x86_64-unknown-linux-gnu/release/ersatztv /usr/local/bin/ersatztv
COPY --from=etv-builder /build/vendor/etv-next/target/x86_64-unknown-linux-gnu/release/ersatztv-channel /usr/local/bin/ersatztv-channel
COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY --chmod=755 tools/stream-access-log.py tools/stream-watch.py tools/client-stall.py /usr/local/bin/
COPY --chmod=755 tools/probe-checks.sh tools/soak-probe.sh /usr/local/bin/
COPY --chmod=755 tools/two-clock-capture.sh /usr/local/bin/
# The ffmpeg probe ships IN THE IMAGE, not scp'd onto the host. It used to be
# host-only state that every `just deploy files` destroyed, so the one thing
# that can tell "ffmpeg stopped encoding" from "ffmpeg is encoding and the
# output is not landing" — and the only proof a channel is on the GPU rather
# than silently on the CPU (#258) — kept disappearing and had to be re-applied
# by hand. In the image it cannot be lost: it survives every deploy, every
# restart, every container recreate.
COPY --chmod=755 tools/ffmpeg-probe.sh /usr/local/bin/ffmpeg-probe.sh

# Config lives on a bind mount; the playout folders and the HLS working set are
# written under /data so a restart resumes from what the daemon already emitted.
# /data/cache is the Vulkan and libplacebo shader cache. HDR items tone map
# through libplacebo, which compiles its shaders on first use and caches them
# under $XDG_CACHE_HOME; unset, that resolves to the home directory of whichever
# uid the container was told to run as, which on this deploy is not writable
# ("Failed to create /root/.cache for shader cache"). Every ffmpeg spawn then
# recompiles. It is on /data so it survives a container recreate.
RUN mkdir -p /config /data /data/cache /data/render \
    && chown etv:etv /config /data /data/cache /data/render

ENV XDG_CACHE_HOME=/data/cache \
    ETV_STATION_CONFIG=/config/station.yaml \
    ETV_NEXT_DIR=/config/etv-next \
    ETV_STATION_OUTPUT_BASE=/data/playout \
    ETV_STATION_CATALOG=/data/catalog.db \
    ETV_RENDER_QUEUE_DIR=/data/render/queue \
    ETV_RENDER_CACHE_DIR=/data/render/cache \
    ETV_STATION_ARTWORK_CACHE=/data/artwork \
    ETV_HLS_OUTPUT=/data/hls \
    ETV_BIND_ADDRESS=0.0.0.0 \
    ETV_PORT=8409 \
    ETV_UI_PORT=4747

# Which etv-station commit this image was built from. The base image's own
# revision/source labels describe the ErsatzTV-ffmpeg image, so these replace
# them. The values come from `tools/deploy.sh build` (--build-arg); an image
# built without them reads `unknown` and `tools/image-age.sh` reports it
# unstamped. The ENV copies are what ersatztv serves at /build.json.
ARG ETV_STATION_REVISION=unknown
ARG ETV_STATION_CREATED=unknown
LABEL org.opencontainers.image.revision=$ETV_STATION_REVISION \
      org.opencontainers.image.created=$ETV_STATION_CREATED \
      org.opencontainers.image.source=https://github.com/McBrideMusings/etv-station
ENV ETV_STATION_REVISION=$ETV_STATION_REVISION \
    ETV_STATION_CREATED=$ETV_STATION_CREATED

# 8409: ETV-next (playlist, guide, HLS). 4747: the station's web UI
# (`etv-station --serve-ui`, started by the entrypoint), no auth of its own —
# front it with your own proxy.
EXPOSE 8409 4747

# Drop to the non-root user. The bind-mounted /config and /data must be
# readable/writable by uid 1000; if the host volume is owned by a different uid,
# override at run time with `docker run --user <uid>:<gid>` to match it.
USER etv

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/entrypoint.sh"]
