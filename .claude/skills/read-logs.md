---
name: read-logs
description: Read runtime logs from the last dev run or other command. Use when the user says they ran the app and something didn't work, or when you need to check what happened during the last run.
---

# Read Logs

Dev and tooling runs write output to timestamped log files in `tmp/`. The file name pattern is:

```
tmp/<command>.<YYYY-MM-DD>_<HHMMSS>.log
```

Examples:
- `./tools/dev-run.sh` → `tmp/dev.2026-05-01_133442.log`
- a build → `tmp/build.2026-05-01_133442.log`
- `cargo test --workspace` → `tmp/test.2026-05-01_133442.log`

Each run produces a new file. Old files are pruned by count (most recent N kept; defaults configured by the generator).

## Strategy

Determine which command was last run, then find its **most recent** log file. Determine whether this is a **build problem** or a **runtime/logging problem**, then read accordingly.

To find the most recent log for a command, sort by mtime:

```
ls -t tmp/<command>.*.log | head -1
```

### Build problem (didn't launch, panic on start)
Read from the **top** of the log file (first 80 lines). Look for:
- `error[E...]:` or `warning:` lines from rustc/clippy
- `error: could not compile` or `BUILD FAILED`
- panic output immediately after launch

### Runtime / behavior bug (launched but something went wrong)
Read from the **bottom** of the log file (last 80 lines). The user typically quits after observing the bug.

### If you need more context
- Read the full file only if the targeted read didn't give enough info
- Look at the previous run (second-most-recent timestamp) if the latest log is empty or unrelated
- Search for specific error patterns

### What NOT to do
- Don't read the entire log file upfront if it's large
- Don't ask the user to paste logs — just read the file
- Don't shell out to tail/view logs — use the Read tool directly on the file
