#!/bin/sh
# Prints how many rustc one of the Dockerfile's Rust stages may run at once.
#
# The two Rust stages (builder, etv-builder) build at the same time, and one
# rustc peaks at up to ~900 MB (ash, the Vulkan bindings under wgpu). Each stage
# gets half of the build machine's memory at RUSTC_MB per job, and never more
# jobs than CPUs. Inside a build, /proc/meminfo and nproc describe the build
# machine (Docker Desktop's VM on a Mac).
#
# The CARGO_BUILD_JOBS build arg, when set, wins; it must be a positive integer.
RUSTC_MB=1000
if [ -n "${CARGO_BUILD_JOBS:-}" ]; then
  case "$CARGO_BUILD_JOBS" in
    *[!0-9]* | 0*) echo "cargo-jobs: CARGO_BUILD_JOBS must be a positive integer, got '$CARGO_BUILD_JOBS'" >&2; exit 1 ;;
  esac
  echo "$CARGO_BUILD_JOBS"
  exit 0
fi
cpus=$(nproc) || exit 1
jobs=$(awk -v per="$RUSTC_MB" '/^MemTotal:/ { print int($2 / 1024 / 2 / per) }' /proc/meminfo)
case "$jobs" in
  '' | *[!0-9]*) echo "cargo-jobs: could not read MemTotal from /proc/meminfo" >&2; exit 1 ;;
esac
[ "$jobs" -le "$cpus" ] || jobs=$cpus
[ "$jobs" -ge 1 ] || jobs=1
echo "cargo-jobs: $jobs per stage ($cpus CPUs, $(awk '/^MemTotal:/ { print int($2 / 1024) }' /proc/meminfo) MB)" >&2
echo "$jobs"
