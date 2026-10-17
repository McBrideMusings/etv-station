use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One entry of a `hooks:` list in `station.yaml` or `channel.yaml` (ADR
/// 0021): code the station runs when something happens, which cannot change
/// the schedule.
///
/// `run` names either a Rhai script (a path ending in `.rhai`, run on a worker
/// thread) or any other executable (run as a child process with the event as
/// JSON on stdin). A relative path resolves against the directory of the
/// config file that declares the hook.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct HookDecl {
    pub on: HookTrigger,

    /// Interval between runs. Required for `on: cron`, and measured from the
    /// daemon's start (or the last config reload): the first run is one
    /// interval away.
    #[serde(
        default,
        with = "humantime_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub every: Option<Duration>,

    pub run: String,

    /// What a Rhai script may reach. Empty by default: a script with no grant
    /// can compute and log, nothing else. Executables run with the daemon's
    /// own permissions and take no grants.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<HookGrant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookTrigger {
    Cron,
}

impl HookTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            HookTrigger::Cron => "cron",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookGrant {
    /// `exec(program, args)` — run a program and read its exit code and output.
    Exec,
    /// `http(url, method, body)` — one blocking HTTP request.
    Http,
}

impl HookGrant {
    pub fn as_str(self) -> &'static str {
        match self {
            HookGrant::Exec => "exec",
            HookGrant::Http => "http",
        }
    }
}

impl HookDecl {
    /// Whether `run` is a Rhai script rather than an executable.
    pub fn is_rhai(&self) -> bool {
        Path::new(&self.run)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("rhai"))
    }

    /// `run` resolved against the declaring config's directory.
    pub fn resolved_run(&self, base_dir: &Path) -> PathBuf {
        let p = Path::new(&self.run);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base_dir.join(p)
        }
    }

    /// The refusal message for a malformed declaration, or `None`. `scope`
    /// names where it was declared (`station` or `channel "x"`).
    pub fn problem(&self, scope: &str, index: usize) -> Option<String> {
        let at = format!("{scope} hooks[{index}]");
        if self.run.trim().is_empty() {
            return Some(format!("{at} has an empty run"));
        }
        match self.on {
            HookTrigger::Cron => match self.every {
                None => return Some(format!("{at} is on: cron and needs `every:`")),
                Some(d) if d < Duration::from_secs(1) => {
                    return Some(format!("{at} every must be at least 1s"));
                }
                Some(_) => {}
            },
        }
        if !self.grants.is_empty() && !self.is_rhai() {
            return Some(format!(
                "{at} grants {:?} but run {:?} is an executable — grants apply to .rhai scripts only",
                self.grants.iter().map(|g| g.as_str()).collect::<Vec<_>>(),
                self.run
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> HookDecl {
        serde_norway::from_str(yaml).expect("parses")
    }

    #[test]
    fn a_cron_hook_parses_with_a_humantime_interval() {
        let h = parse("on: cron\nevery: 90s\nrun: hooks/ping.sh\n");
        assert_eq!(h.on, HookTrigger::Cron);
        assert_eq!(h.every, Some(Duration::from_secs(90)));
        assert!(!h.is_rhai());
        assert_eq!(h.problem("station", 0), None);
    }

    #[test]
    fn a_rhai_hook_takes_grants() {
        let h = parse("on: cron\nevery: 1m\nrun: hooks/ping.rhai\ngrants: [http, exec]\n");
        assert!(h.is_rhai());
        assert_eq!(h.grants, vec![HookGrant::Http, HookGrant::Exec]);
        assert_eq!(h.problem("station", 0), None);
    }

    #[test]
    fn cron_without_every_is_refused() {
        let h = parse("on: cron\nrun: a.sh\n");
        assert!(h.problem("station", 2).unwrap().contains("hooks[2]"));
    }

    #[test]
    fn grants_on_an_executable_are_refused() {
        let h = parse("on: cron\nevery: 1m\nrun: a.sh\ngrants: [exec]\n");
        assert!(h.problem("station", 0).unwrap().contains("executable"));
    }

    #[test]
    fn a_relative_run_resolves_against_the_config_dir() {
        let h = parse("on: cron\nevery: 1m\nrun: hooks/a.sh\n");
        assert_eq!(
            h.resolved_run(Path::new("/cfg")),
            PathBuf::from("/cfg/hooks/a.sh")
        );
    }
}
