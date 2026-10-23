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

## Production: the Unraid container

Never ask pierce to paste a prod log; read it over SSH. `UNRAID_HOST` and `UNRAID_USER` (default `root`) are in `.env`. Every read below is read-only and goes ahead unasked.

| Log | Holds | Fetch |
|---|---|---|
| Loki `{container="etv-station"}` | The container stdout below, shipped by Alloy on the host; 90 days retained, queryable by time without ssh | `logcli query --since=12h '{container="etv-station"} \|= "reburst: lead exhausted"'` (`LOKI_ADDR` from `~/.claude/.env`); count rebursts with `logcli query --since=12h --limit=5000 '…' \| wc -l`. Lines exist from 2026-10-23 17:06 UTC (1:06pm EDT) on; earlier ones are in `docker logs` only |
| Container stdout | Both programs' tracing, one JSON object per line with a UTC `timestamp`, `level` and `fields.event` (`chunk.write`, `calendar.select`, `bumper.fill`, `mem.sample`, …) | `ssh "$UNRAID_USER@$UNRAID_HOST" docker logs --since <t> --until <t> etv-station`; `admin logs` follows it live |
| `data/diag/access.log` | ETV-next's per-HTTP-message access log: client address, path, status | `ssh … tail -n 200 /mnt/user/appdata/etv-station/data/diag/access.log` |
| `data/diag/stream-events.log` | The stream event watcher: segment and playlist events per channel | as above; `admin diag` follows both together |
| `data/diag/ffmpeg-argv-ch<N>.log` | The exact ffmpeg argv per transcode, one argument per line | as above |
| `data/diag/ffmpeg-progress-ch<N>-<stamp>-<pid>.log` | Per-second `frame`, `fps`, `out_time`, `speed` for one transcode | as above |

`--since`/`--until` take an RFC 3339 time or a duration (`10m`). Times in the container are UTC; the station's own `tz` (America/Chicago) only shapes chunk boundaries.

Filter one channel or event with `jq` on this machine: `… docker logs --since 1h etv-station 2>&1 | jq -c 'select(.fields.channel=="003-for-madi")'`.
