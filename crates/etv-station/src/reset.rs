//! The two pieces `--refresh-channel --seed N` adds around the forced refresh:
//! rewriting a channel's `seed:` and reaching the running daemon so it reloads.
//!
//! The refresh itself (cut at the airing item's finish, wipe the playout JSON
//! from there, truncate the play history and rewind `.resume` to the surviving
//! frontier) is `daemon::refresh_forward`; nothing here repeats it.

use std::path::{Path, PathBuf};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// The daemon's pid file, under `output_base`. Written at startup so the CLI
/// can send SIGHUP; the daemon binds no port to ask instead.
pub fn pid_path(output_base: &Path) -> PathBuf {
    output_base.join(".etv-station.pid")
}

/// Removes the pid file when the daemon returns.
pub struct PidFile(PathBuf);

impl PidFile {
    pub fn write(output_base: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(output_base)?;
        let path = pid_path(output_base);
        std::fs::write(&path, format!("{}\n", std::process::id()))?;
        Ok(Self(path))
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        // Another daemon on the same folder may have overwritten the file; only
        // remove our own.
        let ours = format!("{}\n", std::process::id());
        if std::fs::read_to_string(&self.0).is_ok_and(|t| t == ours) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// Whether `pid` is a live etv-station process. A pid file outlives a daemon
/// killed without cleanup, and SIGHUP terminates a process that does not
/// handle it, so the pid is signalled only after its command name checks out.
fn is_station_process(pid: i32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .map(|o| {
            // `comm` can be a full path; only the file name identifies the program.
            let comm = String::from_utf8_lossy(&o.stdout);
            o.status.success() && comm.trim().rsplit('/').next() == Some("etv-station")
        })
        .unwrap_or(false)
}

/// What reaching the daemon came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Reload {
    Signalled(i32),
    /// No pid file, or the pid in it names no live process.
    NotRunning,
}

/// SIGHUP the daemon named by the pid file, so it re-reads config from disk.
pub fn signal_reload(output_base: &Path) -> Result<Reload, String> {
    let path = pid_path(output_base);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Reload::NotRunning);
    };
    let pid: i32 = text
        .trim()
        .parse()
        .map_err(|_| format!("{}: not a pid: {:?}", path.display(), text.trim()))?;
    if pid <= 1 || !is_station_process(pid) {
        return Ok(Reload::NotRunning);
    }
    match kill(Pid::from_raw(pid), Signal::SIGHUP) {
        Ok(()) => Ok(Reload::Signalled(pid)),
        Err(nix::errno::Errno::ESRCH) => Ok(Reload::NotRunning),
        Err(err) => Err(format!("SIGHUP to pid {pid}: {err}")),
    }
}

/// `text` with its top-level `seed:` set to `seed`, every other line untouched.
///
/// A `seed:` that is a rotation (`seed:` followed by an indented mapping, or an
/// inline `{ rotate: 2d }`) is replaced by the plain number: a pinned seed is
/// what `--seed` asks for. With no `seed:` yet, the line goes straight after
/// `name:`.
pub fn set_seed(text: &str, seed: u64) -> Result<String, String> {
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if !replaced && line.starts_with("seed:") {
            out.push(format!("seed: {seed}"));
            replaced = true;
            // Drop the indented body of a block-form value.
            while lines
                .peek()
                .is_some_and(|l| l.starts_with(' ') || l.starts_with('\t'))
            {
                lines.next();
            }
            continue;
        }
        out.push(line.to_string());
    }
    if !replaced {
        let at = out
            .iter()
            .position(|l| l.starts_with("name:"))
            .ok_or_else(|| "the channel config has no top-level `name:` line".to_string())?;
        out.insert(at + 1, format!("seed: {seed}"));
    }
    let mut result = out.join("\n");
    if text.ends_with('\n') {
        result.push('\n');
    }
    let parsed: serde_norway::Value = serde_norway::from_str(&result)
        .map_err(|e| format!("rewritten config does not parse: {e}"))?;
    match parsed.get("seed").and_then(|v| v.as_u64()) {
        Some(n) if n == seed => Ok(result),
        other => Err(format!("rewritten config reads seed {other:?}, not {seed}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_line_is_replaced_in_place_and_the_rest_is_untouched() {
        let before = "name: a\nseed: 21  # pinned\nwindow_days: 3\n";
        assert_eq!(
            set_seed(before, 99).unwrap(),
            "name: a\nseed: 99\nwindow_days: 3\n",
        );
    }

    #[test]
    fn a_rotation_is_replaced_by_the_pinned_number() {
        let before = "name: a\nseed:\n  rotate: 3d\n  cycle: [1, 2]\nnumber: 4\n";
        assert_eq!(
            set_seed(before, 7).unwrap(),
            "name: a\nseed: 7\nnumber: 4\n",
        );
        assert_eq!(
            set_seed("name: a\nseed: { rotate: 2d }\n", 7).unwrap(),
            "name: a\nseed: 7\n",
        );
    }

    #[test]
    fn a_missing_seed_goes_after_name() {
        assert_eq!(
            set_seed("# top\nname: a\nnumber: 4\n", 5).unwrap(),
            "# top\nname: a\nseed: 5\nnumber: 4\n",
        );
    }

    #[test]
    fn a_config_with_no_name_is_refused() {
        assert!(set_seed("number: 4\n", 5).is_err());
    }

    #[test]
    fn the_daemon_is_reached_only_through_a_live_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(signal_reload(dir.path()).unwrap(), Reload::NotRunning);
        // A pid no process holds: i32::MAX is above any pid_max.
        std::fs::write(pid_path(dir.path()), format!("{}\n", i32::MAX)).unwrap();
        assert_eq!(signal_reload(dir.path()).unwrap(), Reload::NotRunning);
        std::fs::write(pid_path(dir.path()), "garbage\n").unwrap();
        assert!(signal_reload(dir.path()).is_err());
        // A live process that is not a station is never signalled: this test
        // process itself (SIGHUP would end it), pid 0 and a negative group.
        for pid in [std::process::id().to_string(), "0".into(), "-1".into()] {
            std::fs::write(pid_path(dir.path()), format!("{pid}\n")).unwrap();
            assert_eq!(signal_reload(dir.path()).unwrap(), Reload::NotRunning);
        }
    }

    #[test]
    fn the_pid_file_names_this_process_and_goes_with_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let guard = PidFile::write(dir.path()).unwrap();
        let text = std::fs::read_to_string(pid_path(dir.path())).unwrap();
        assert_eq!(text.trim(), std::process::id().to_string());
        drop(guard);
        assert!(!pid_path(dir.path()).exists());
    }
}
