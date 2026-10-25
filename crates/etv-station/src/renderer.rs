//! The chapter renderer as the station sees it: where its queue and cache are,
//! and what `/api/health/renders` and the command line need from it.
//!
//! The rendering itself is [`etv_render`]. The station core holds only paths,
//! a size cap and a read of the queue's state files; what a chapter is, and
//! how a plugin decides to ask for one, stay on the plugin side.

use std::path::PathBuf;

use etv_render::Settings;
use etv_render::queue::Queue;

use crate::config::station::RendererConfig;

/// Gigabytes the cache may take when `cache_cap_gb` is unset.
pub const DEFAULT_CACHE_CAP_GB: u64 = 20;

/// The renderer's resolved locations. Environment variables replace the
/// station config's `renderer:` section, key by key.
#[derive(Debug, Clone, PartialEq)]
pub struct RendererPaths {
    pub queue_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub cache_cap_bytes: u64,
}

impl RendererPaths {
    /// `Err` names every setting that is missing, so one message covers a
    /// half-configured renderer.
    pub fn resolve(
        config: &RendererConfig,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let var = |k: &str| lookup(k).filter(|v| !v.trim().is_empty());
        let queue = var("ETV_RENDER_QUEUE_DIR").or_else(|| config.queue_dir.clone());
        let cache = var("ETV_RENDER_CACHE_DIR").or_else(|| config.cache_dir.clone());
        let cap_gb = match var("ETV_RENDER_CACHE_CAP_GB") {
            Some(raw) => raw.trim().parse::<u64>().map_err(|_| {
                format!("ETV_RENDER_CACHE_CAP_GB must be a whole number of gigabytes, got {raw:?}")
            })?,
            None => config.cache_cap_gb.unwrap_or(DEFAULT_CACHE_CAP_GB),
        };
        let mut missing = Vec::new();
        if queue.is_none() {
            missing.push("queue_dir (ETV_RENDER_QUEUE_DIR)");
        }
        if cache.is_none() {
            missing.push("cache_dir (ETV_RENDER_CACHE_DIR)");
        }
        match (queue, cache) {
            (Some(queue), Some(cache)) => Ok(Self {
                queue_dir: PathBuf::from(queue),
                cache_dir: PathBuf::from(cache),
                cache_cap_bytes: cap_gb.saturating_mul(1_000_000_000),
            }),
            _ => Err(format!(
                "the chapter renderer is not configured: set renderer.{}",
                missing.join(" and renderer.")
            )),
        }
    }

    pub fn from_env(config: &RendererConfig) -> Result<Self, String> {
        Self::resolve(config, |k| std::env::var(k).ok())
    }

    pub fn queue(&self) -> Queue {
        Queue::new(&self.queue_dir)
    }

    pub fn settings(&self) -> Settings {
        Settings::from_env(self.cache_dir.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(queue: Option<&str>, cache: Option<&str>, cap: Option<u64>) -> RendererConfig {
        RendererConfig {
            queue_dir: queue.map(String::from),
            cache_dir: cache.map(String::from),
            cache_cap_gb: cap,
        }
    }

    #[test]
    fn the_environment_replaces_the_config_key_by_key() {
        let env = |k: &str| match k {
            "ETV_RENDER_QUEUE_DIR" => Some("/env/queue".to_string()),
            "ETV_RENDER_CACHE_CAP_GB" => Some("3".to_string()),
            _ => None,
        };
        let paths = RendererPaths::resolve(
            &cfg(Some("/file/queue"), Some("/file/cache"), Some(50)),
            env,
        )
        .unwrap();
        assert_eq!(paths.queue_dir, PathBuf::from("/env/queue"));
        assert_eq!(paths.cache_dir, PathBuf::from("/file/cache"));
        assert_eq!(paths.cache_cap_bytes, 3_000_000_000);
    }

    #[test]
    fn an_unconfigured_renderer_names_what_is_missing() {
        let err = RendererPaths::resolve(&cfg(None, Some("/c"), None), |_| None).unwrap_err();
        assert!(err.contains("queue_dir (ETV_RENDER_QUEUE_DIR)"), "{err}");
        assert!(!err.contains("cache_dir"), "{err}");
    }

    #[test]
    fn the_cap_defaults_to_twenty_gigabytes() {
        let paths = RendererPaths::resolve(&cfg(Some("/q"), Some("/c"), None), |_| None).unwrap();
        assert_eq!(paths.cache_cap_bytes, 20_000_000_000);
    }
}

/// The command-line side: `render`, `enqueue` and `render-sidecar`. Each reads
/// only the station config's `renderer:` section, so none of them loads
/// channels or opens the catalog.
pub mod cli {
    use std::path::Path;
    use std::process::ExitCode;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use etv_render::Plan;
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    use super::RendererPaths;
    use crate::config::{self, RendererConfig};

    /// How often the sidecar looks for a new plan when the queue is empty.
    const POLL: Duration = Duration::from_secs(2);

    fn paths(config_path: Option<&Path>) -> Result<RendererPaths, String> {
        let section = match config_path {
            Some(path) => {
                config::read_station(path)
                    .map_err(|e| format!("cannot read the station config: {e}"))?
                    .renderer
            }
            None => RendererConfig::default(),
        };
        RendererPaths::from_env(&section)
    }

    fn read_plan(path: &Path) -> Result<Plan, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Plan::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// `render <plan.json>`: render one plan now, in this process.
    pub fn render(config_path: Option<&Path>, plan_path: &Path) -> ExitCode {
        let run = || -> Result<(), String> {
            let paths = paths(config_path)?;
            let plan = read_plan(plan_path)?;
            let rendered =
                etv_render::render::render(&plan, &paths.settings()).map_err(|e| e.to_string())?;
            println!("{}", plan.output.display());
            eprintln!("rendered in {:.1}s", rendered.timings.total_s);
            Ok(())
        };
        match run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("render failed: {message}");
                ExitCode::FAILURE
            }
        }
    }

    /// `enqueue <plan.json>`: put the plan on the queue and print its job id.
    pub fn enqueue(config_path: Option<&Path>, plan_path: &Path, air_at: Option<&str>) -> ExitCode {
        let run = || -> Result<String, String> {
            let paths = paths(config_path)?;
            let plan = read_plan(plan_path)?;
            let air_at = air_at
                .map(|raw| {
                    OffsetDateTime::parse(raw, &Rfc3339)
                        .map_err(|e| format!("--air-at must be an RFC 3339 time like 2026-10-23T19:00:00Z, got {raw:?}: {e}"))
                })
                .transpose()?;
            let job = paths
                .queue()
                .enqueue(&plan, air_at, OffsetDateTime::now_utc())
                .map_err(|e| e.to_string())?;
            Ok(job.id)
        };
        match run() {
            Ok(id) => {
                println!("{id}");
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("enqueue failed: {message}");
                ExitCode::FAILURE
            }
        }
    }

    /// `render-sidecar`: render queued plans one at a time until killed.
    /// Exit status `render-sidecar` returns when no queue or cache is
    /// configured, so the container entrypoint can tell it from a crash.
    pub const NOT_CONFIGURED: u8 = 78;

    pub fn sidecar(config_path: Option<&Path>) -> ExitCode {
        let paths = match paths(config_path) {
            Ok(p) => p,
            Err(message) => {
                eprintln!("render-sidecar: {message}");
                return ExitCode::from(NOT_CONFIGURED);
            }
        };
        if let Err(e) = std::fs::create_dir_all(&paths.queue_dir)
            .and_then(|()| std::fs::create_dir_all(&paths.cache_dir))
        {
            eprintln!("render-sidecar: cannot create the queue and cache directories: {e}");
            return ExitCode::FAILURE;
        }
        let stop = AtomicBool::new(false);
        etv_render::queue::run_sidecar(
            &paths.queue(),
            &paths.settings(),
            paths.cache_cap_bytes,
            &stop,
            POLL,
        );
        ExitCode::SUCCESS
    }
}
