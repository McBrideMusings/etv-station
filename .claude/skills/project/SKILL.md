---
name: project
description: "etv-station: how to verify it works (the daemon booted locally against examples/, the web dashboard, channel review), where its logs live (dev runs under tmp/, the prod container's stdout and data/diag/) and how to fetch them, and how swarm workers build it. Read before declaring a change verified, before diagnosing anything that happened while it ran, and before the first build in a swarm worktree."
---

# etv-station

- **Verify** — read [verify.md](verify.md) before reporting any change works.
- **Logs** — read [logs.md](logs.md) when something happened while it ran: in a dev run or in
  the prod container, at a time. It fetches the lines itself; never ask the user to paste them.
- **Swarm** — read [swarm.md](swarm.md) before the first build or test in a swarm worktree.

The scripts these sections call live in `tools/`.
