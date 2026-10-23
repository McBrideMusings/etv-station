# Swarm workers — etv-station

`H` below means `"$HOME/.claude/skills/swarm/swarm" heavy --`.

## Heavy commands

Every one of these runs through `H`:

- `./tools/verify-all.sh` and each of its modes (`test`, `lint`, `fmt-check`, `fmt`) — the
  full gate, both cargo workspaces (the root and `vendor/etv-next`, which is excluded from the
  root workspace).
- Any `cargo build`, `cargo test`, `cargo clippy`, `cargo check`, `cargo run`, in either
  workspace (`cargo --manifest-path vendor/etv-next/Cargo.toml ...` for the vendored one).
- `./tools/dev-run.sh`, `./tools/verify-integration.sh`, `./tools/verify-reburst.sh` — they build both etv-next binaries and
  the daemon, and run ffmpeg.
- `./tools/dev-station.sh` — builds and runs only the station daemon.
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

- **The dev stack is one per machine, whatever the port.** `dev-run.sh`'s preflight
  (`tools/dev-run.sh` `preflight_stale_procs`) and `verify-integration.sh`'s orphan check match
  `etv-station`, `ersatztv`, `etv-overlay` and dev ffmpeg processes machine-wide with `pgrep`.
  A run beside another worker's stack, or the main checkout's, refuses at preflight or fails
  its orphan check. So `dev-run.sh`, `verify-integration.sh` and `dev-station.sh` run only
  inside `swarm heavy`, which also keeps two workers' runs apart. If the preflight still
  refuses, the main checkout's stack is running: report that in your pass's result and skip
  the integration run. Never kill it.
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
  worker already has its own.
- **Cleanup.** `dev-run.sh` and `verify-integration.sh` kill their own process groups from a
  `cleanup` trap when they exit or are interrupted, so ending the script ends its stack. A
  process you started directly (a `cargo run`, an `etv-overlay`) you stop by the PID you
  recorded when you started it. Never run `./tools/kill-dev.sh`, `pkill` or `killall` in a
  worker: they match process names machine-wide and kill the other workers' and the main
  checkout's stacks too.
- **`deploy/appdata/*`** is gitignored and absent from worktrees. No worker reads or edits it.

## Slot cap

None beyond the lead's default; the heavy lock serializes builds.
