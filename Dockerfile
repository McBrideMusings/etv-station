# syntax=docker/dockerfile:1
#
# Multi-stage build for the etv-station daemon (and the etv-overlay renderer it
# supervises). The builder compiles a release binary with the full Rust
# toolchain; the runtime stage is a slim Debian image carrying just the two
# binaries plus the runtime libraries they need.
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
# Reads the whole workspace (including the ersatztv-playout path dep under the
# etv-next submodule) and writes recipe.json — the dependency graph, no sources.
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
# ersatztv-playout is a path dependency under the etv-next submodule, which the
# top-level Cargo.toml excludes from this workspace. cargo-chef only reconstructs
# skeleton crates for workspace members, so it does NOT recreate the etv-next
# path dep — cook would fail to resolve it. Copy the real submodule in first so
# cargo can resolve and build it. The submodule is pinned by SHA and changes
# rarely, so it belongs in this cached dependency layer; a submodule bump is the
# only thing besides a Cargo.lock change that busts the cache.
COPY --from=planner /build/recipe.json recipe.json
COPY etv-next etv-next
RUN cargo chef cook --release --locked --recipe-path recipe.json -p etv-station -p etv-overlay

# Now the real build. The whole workspace is needed: ersatztv-playout is a path
# dependency under the etv-next submodule, and etv-overlay is a sibling crate.
# .dockerignore keeps the context small (no target/, .git/, docs build output).
# The dependency layer cooked above is reused, so this recompiles only the
# workspace crates. etv-query-test (the Phase A CEL harness) is a dev tool and
# intentionally left out of the deployed image.
COPY . .
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
