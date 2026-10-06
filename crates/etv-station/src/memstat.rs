//! Once-a-minute memory sample of the daemon's own process, written to the log.
//!
//! The question it answers: when the daemon's resident set (RSS) grows, is the
//! allocator holding the memory *in use*, or is it holding freed memory it has
//! not handed back to the kernel? `rss_kb` is what the host's `oomwatch` reads;
//! `in_use_bytes` is what the program still owns. A gap that widens means
//! fragmentation; an `in_use_bytes` that climbs with `rss_kb` means something
//! holds the data. Correlate `mem.sample` lines against the `catalog.refresh.*`
//! and roll-tick events by timestamp to see which activity moves the number.
//!
//! Linux with glibc only (`/proc/self` and `mallinfo2`); elsewhere [`run`]
//! returns at once and the daemon logs nothing.

use std::time::Duration;

/// One reading. Sizes from `/proc` are kilobytes, from `mallinfo2` bytes —
/// the field names carry the unit so the log line needs no legend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemSample {
    /// Resident set size: what the host sees and `oomwatch` kills on.
    pub rss_kb: u64,
    /// The part of RSS that is anonymous memory (heap and thread stacks, not
    /// the binary or mapped files).
    pub anon_kb: u64,
    /// Bytes the program has allocated and not freed: `uordblks` (heap
    /// arenas) plus `hblkhd` (large allocations served by `mmap`).
    pub in_use_bytes: u64,
    /// Bytes freed inside the heap arenas but still held by the allocator
    /// (`fordblks`). RSS minus `in_use_bytes` is mostly this.
    pub free_held_bytes: u64,
    /// Bytes of large `mmap`ed allocations (`hblkhd`), a subset of
    /// `in_use_bytes`.
    pub mmapped_bytes: u64,
    /// OS threads in the process (`Threads:` in `/proc/self/status`).
    pub threads: u64,
}

/// `Rss` and `Anonymous` (both kB) from the text of `/proc/<pid>/smaps_rollup`.
#[cfg_attr(not(all(target_os = "linux", target_env = "gnu")), allow(dead_code))]
fn parse_smaps_rollup(text: &str) -> Option<(u64, u64)> {
    Some((
        number_after(text, "Rss:")?,
        number_after(text, "Anonymous:")?,
    ))
}

/// The `Threads:` count from the text of `/proc/<pid>/status`.
#[cfg_attr(not(all(target_os = "linux", target_env = "gnu")), allow(dead_code))]
fn parse_threads(text: &str) -> Option<u64> {
    number_after(text, "Threads:")
}

/// The first whole number on the first line that starts with `key`.
#[cfg_attr(not(all(target_os = "linux", target_env = "gnu")), allow(dead_code))]
fn number_after(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn sample() -> Option<MemSample> {
    let (rss_kb, anon_kb) =
        parse_smaps_rollup(&std::fs::read_to_string("/proc/self/smaps_rollup").ok()?)?;
    let threads = parse_threads(&std::fs::read_to_string("/proc/self/status").ok()?)?;
    // SAFETY: `mallinfo2` takes no arguments and returns a plain struct by
    // value; it only reads the allocator's own bookkeeping.
    let mi = unsafe { libc::mallinfo2() };
    Some(MemSample {
        rss_kb,
        anon_kb,
        in_use_bytes: (mi.uordblks + mi.hblkhd) as u64,
        free_held_bytes: mi.fordblks as u64,
        mmapped_bytes: mi.hblkhd as u64,
        threads,
    })
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn sample() -> Option<MemSample> {
    None
}

/// Log a `mem.sample` every `interval`, starting now, until the runtime drops
/// the task. Returns at once on a platform where [`sample`] has nothing to read.
pub async fn run(interval: Duration) {
    if sample().is_none() {
        return;
    }
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(s) = sample() else {
            tracing::warn!(event = "mem.sample_failed", "could not read process memory");
            continue;
        };
        tracing::info!(
            event = "mem.sample",
            rss_kb = s.rss_kb,
            anon_kb = s.anon_kb,
            in_use_bytes = s.in_use_bytes,
            free_held_bytes = s.free_held_bytes,
            mmapped_bytes = s.mmapped_bytes,
            threads = s.threads,
            "process memory",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smaps_rollup_yields_rss_and_anonymous() {
        let text = "Rss:             2206096 kB\n\
                    Pss:             2205127 kB\nAnonymous:       2201784 kB\n";
        assert_eq!(parse_smaps_rollup(text), Some((2_206_096, 2_201_784)));
    }

    #[test]
    fn status_yields_thread_count() {
        assert_eq!(
            parse_threads("Name:\tetv-station\nThreads:\t17\n"),
            Some(17)
        );
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn live_sample_reads_this_process() {
        let s = sample().expect("linux glibc sample");
        assert!(s.rss_kb > 0 && s.in_use_bytes > 0 && s.threads >= 1);
    }
}
