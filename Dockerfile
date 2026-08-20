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
FROM rust:1.93-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /build

# ---- planner ----
# Reads the whole workspace (including the ersatztv-playout path dep under
# vendor/etv-next) and writes recipe.json — the dependency graph, no sources.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- builder ----
FROM chef AS builder

# Native build dependencies for the graphics stack pulled in transitively by
# etv-overlay (vello + wgpu + parley) — etv-station depends on the etv-overlay
# crate for overlay-spec validation, so this toolchain is required even though
# the daemon itself renders nothing. Needed by the cook step below, which
# compiles those dependencies.
RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake \
        pkg-config \
        libfontconfig1-dev \
    && rm -rf /var/lib/apt/lists/*

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
RUN cargo chef cook --release --locked --recipe-path recipe.json -p etv-station -p etv-overlay

# Now the real build. The whole workspace is needed: ersatztv-playout is a path
# dependency under vendor/etv-next, and etv-overlay is a sibling crate.
# .dockerignore keeps the context small (no target/, .git/, docs build output).
# The dependency layer cooked above is reused, so this recompiles only the
# workspace crates. etv-query-test (the Phase A CEL harness) is a dev tool and
# intentionally left out of the deployed image.
COPY . .
RUN cargo build --release --locked -p etv-station -p etv-overlay

# ---- etv-builder ----
# ErsatzTV-next is its own cargo workspace under vendor/, so it builds on its own
# here rather than as part of the station workspace above. The layer is keyed on
# the vendored tree, so it only recompiles when that tree moves.
# `ersatztv-channel` is built alongside the server because the server looks for
# it as a sibling executable when it spawns a channel session.
FROM chef AS etv-builder
COPY vendor/etv-next /build/etv-next
WORKDIR /build/etv-next
RUN cargo build --release --locked --bin ersatztv --bin ersatztv-channel

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
        tcpdump \
        tini \
    && rm -rf /var/lib/apt/lists/*

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
COPY --from=builder /build/target/release/etv-station /usr/local/bin/etv-station
COPY --from=builder /build/target/release/etv-overlay /usr/local/bin/etv-overlay
COPY --from=etv-builder /build/etv-next/target/release/ersatztv /usr/local/bin/ersatztv
COPY --from=etv-builder /build/etv-next/target/release/ersatztv-channel /usr/local/bin/ersatztv-channel
COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY --chmod=755 tools/stream-access-log.py tools/stream-watch.py /usr/local/bin/
COPY --chmod=755 tools/probe-checks.sh tools/soak-probe.sh /usr/local/bin/
COPY --chmod=755 tools/two-clock-capture.sh /usr/local/bin/

# Config lives on a bind mount; the playout folders and the HLS working set are
# written under /data so a restart resumes from what the daemon already emitted.
RUN mkdir -p /config /data \
    && chown etv:etv /config /data

ENV ETV_STATION_CONFIG=/config/station.yaml \
    ETV_NEXT_DIR=/config/etv-next \
    ETV_STATION_OUTPUT_BASE=/data/playout \
    ETV_STATION_CATALOG=/data/catalog.db \
    ETV_STATION_ARTWORK_CACHE=/data/artwork \
    ETV_HLS_OUTPUT=/data/hls \
    ETV_BIND_ADDRESS=0.0.0.0 \
    ETV_PORT=8409

EXPOSE 8409

# Drop to the non-root user. The bind-mounted /config and /data must be
# readable/writable by uid 1000; if the host volume is owned by a different uid,
# override at run time with `docker run --user <uid>:<gid>` to match it.
USER etv

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/entrypoint.sh"]
