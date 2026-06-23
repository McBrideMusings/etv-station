# syntax=docker/dockerfile:1
#
# Multi-stage build for the etv-station daemon (and the etv-overlay renderer it
# supervises). The builder compiles a release binary with the full Rust
# toolchain; the runtime stage is a slim Debian image carrying just the two
# binaries plus the runtime libraries they need.

# ---- builder ----
# Pinned to the toolchain the workspace is developed against. Edition 2024
# requires Rust >= 1.85.
FROM rust:1.93-bookworm AS builder

# Native build dependencies for the graphics stack pulled in transitively by
# etv-overlay (vello + wgpu + parley) — etv-station depends on the etv-overlay
# crate for overlay-spec validation, so this toolchain is required even though
# the daemon itself renders nothing.
RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake \
        pkg-config \
        libfontconfig1-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# The whole workspace is needed: ersatztv-playout is a path dependency under the
# etv-next submodule, and etv-overlay is a sibling crate. .dockerignore keeps the
# context small (no target/, .git/, docs build output).
COPY . .

# Build the daemon and the overlay renderer. etv-query-test (the Phase A CEL
# harness) is a dev tool and intentionally left out of the deployed image.
# --locked builds against the committed Cargo.lock.
RUN cargo build --release --locked -p etv-station -p etv-overlay

# ---- runtime ----
FROM debian:bookworm-slim AS runtime

# ffmpeg provides ffprobe, which the daemon shells out to for media duration
# (the duration cache). libvulkan1 + mesa-vulkan-drivers give etv-overlay a
# software Vulkan (lavapipe), so overlay rendering works on a headless host with
# no GPU; channels without an overlay never spawn it and don't exercise these.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        ffmpeg \
        libvulkan1 \
        mesa-vulkan-drivers \
    && rm -rf /var/lib/apt/lists/*

# The daemon resolves the overlay binary next to its own executable
# (overlay_supervisor::overlay_binary_path), so the two sit side by side.
COPY --from=builder /build/target/release/etv-station /usr/local/bin/etv-station
COPY --from=builder /build/target/release/etv-overlay /usr/local/bin/etv-overlay

# Config and the shared playout volume are bind-mounted at run time. JSON logging
# is the default so the container runtime captures structured stdout lines.
ENTRYPOINT ["etv-station"]
CMD ["--config", "/config/station.toml", "--log-format", "json"]
