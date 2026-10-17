---
name: swarm-worker
description: "How a swarm worker builds and tests etv-station beside other workers: heavy commands, the two cargo workspaces, per-worker ETV_PORT and docs port. Read before the first build or test in a swarm worktree."
---

# swarm-worker — etv-station

`H` below means `"$HOME/.claude/skills/swarm/swarm" heavy --`.

## Heavy commands

Every one of these runs through `H`:

- `./tools/verify-all.sh` and each of its modes (`test`, `lint`, `fmt-check`, `fmt`) — the
  full gate, both cargo workspaces (the root and `vendor/etv-next`, which is excluded from the
  root workspace).
- Any `cargo build`, `cargo test`, `cargo clippy`, `cargo check`, `cargo run`, in either
  workspace (`cargo --manifest-path vendor/etv-next/Cargo.toml ...` for the vendored one).
- `./tools/dev-run.sh`, `./tools/verify-integration.sh`, `./tools/dev-station.sh` — they build
  both etv-next binaries and the daemon, and run ffmpeg.
- `./tools/overlay-test.sh`, `./tools/overlay-still.sh` — Vello render plus ffmpeg encode.
- `bun install`, `bun run docs:build`.

Run the full `./tools/verify-all.sh` once per round of edits, not after each file.

## The cheap check

`H cargo test -p etv-station --lib <filter>` for a unit you touched, or
`H cargo check -p etv-station` for a compile check. A change under `vendor/etv-next/` checks
with `H cargo check --manifest-path vendor/etv-next/Cargo.toml --workspace`. Both still take
the lock — a cargo check of this workspace uses every core.

## Shared caches

`swarm launch` sets `CARGO_TARGET_DIR=<worktree>/target`, cloned from the main checkout's
`target/`, and that environment variable wins over the `build.target-dir` in
`~/.worktrees/etv-station/.cargo/config.toml`. Keep it: a target dir shared across worktrees
lets one worker's `cargo test` run another branch's binary. The vendored workspace builds into
the same `CARGO_TARGET_DIR`.

## Per-worker resources

- **HTTP port.** `dev-run.sh` and `verify-integration.sh` bind `ETV_PORT` (default 8409, the
  main checkout's). Give each run your own port, derived from the bead id:

  ```sh
  ETV_PORT=$((20000 + $(printf %s "$SWARM_WORKER" | cksum | cut -d' ' -f1) % 10000)) \
    "$HOME/.claude/skills/swarm/swarm" heavy -- ./tools/verify-integration.sh
  ```

  Never run either script with the default port in a worker.
- **Docs port.** `bun run docs:dev` binds 5193. Don't run it in a worker; `bun run docs:build`
  checks a docs change without a server.
- **Output and temp paths.** `examples/output/` and `tmp/` are relative to the worktree, so each
  worker already has its own. Stop every daemon and ETV-next process you started, by its PID,
  before the pass ends. Never run `./tools/kill-dev.sh` in a worker: it matches process names
  machine-wide (`pgrep`) and kills the other workers' and the main checkout's dev stacks too.
- **`deploy/appdata/*`** is gitignored and absent from worktrees. No worker reads or edits it.

## Slot cap

None beyond the lead's default; the heavy lock serializes builds.
