use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use etv_station::catalog::Catalog;
use etv_station::catalog::reconcile_plexdb::{self, ReconcileReport};
use etv_station::{config, daemon, etv_next};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "etv-station",
    about = "Playout JSON generator daemon for ErsatzTV-next"
)]
struct Cli {
    /// Path to the top-level station config (TOML or YAML, by extension).
    #[arg(short, long)]
    config: PathBuf,

    /// Log output format.
    #[arg(long, value_enum, default_value_t = LogFormat::Pretty)]
    log_format: LogFormat,

    /// Print each channel's resolved output_folder (one per line) and exit,
    /// instead of running the daemon. tools/dev-run.sh uses this to discover
    /// the folders to poll through the daemon's own config loader, so it can
    /// never disagree with where the daemon actually writes.
    #[arg(long)]
    list_folders: bool,

    /// Render ETV-next's lineup.json + channelN.json into this directory from
    /// the station config, then exit. The `ffmpeg`/`normalization` playback
    /// profile every channel body is built from comes from the station config
    /// itself now, not a file in this directory. Display names come from each
    /// channel's own `display_name` (#158) — there is no `presentation.json`.
    /// The container entrypoint runs this before starting ETV-next, so the
    /// playout folders it reads are always the ones the daemon writes.
    #[arg(long, value_name = "DIR")]
    render_etv_next: Option<PathBuf>,

    /// Generate the named channel twice from identical inputs (same catalog
    /// snapshot, seed, and resume state) and report whether the two
    /// schedules match, then exit — a debug check for a plugin that breaks
    /// reproducible generation silently (#168). Not part of normal
    /// generation. Exit code is 0 for identical, 1 for a differing schedule
    /// or a load/resolve failure.
    #[arg(long, value_name = "CHANNEL")]
    check_determinism: Option<String>,

    /// Compare this station's catalog (`--config`'s `catalog_path`)
    /// `entry_id` against a `plex-db-ex` snapshot's `item_id`, joined on
    /// Plex rating key, then exit — a report, never a fix (#269). Both
    /// databases are opened read-only; every mismatch prints in full, and
    /// "present in only one store" titles are counted and sampled. Exit
    /// code is non-zero when anything mismatches.
    #[arg(long, value_name = "PLEXDB_PATH")]
    reconcile_plexdb: Option<PathBuf>,

    /// Print the named channel's fully-resolved overlay spec as YAML on stdout
    /// and exit, with every path absolute — what the channel's overlay process
    /// would actually be spawned with.
    ///
    /// The channel is named the way its directory or file is
    /// (`036-the-academy`), not by number. Exits non-zero if no such channel
    /// exists or the channel resolves to no overlay at all.
    ///
    /// This exists so `tools/overlay-extract.py` (and so `admin overlay-watch`)
    /// can preview the real cascade instead of reimplementing it. Since ADR
    /// 0008 a channel may declare only `overlay: {extend: …}`, which carries no
    /// geometry and means nothing without the station-level spec above it — so
    /// a preview that reads the channel file alone can no longer resolve one.
    /// Reusing `config::overlay` here is what keeps the preview and the daemon
    /// from disagreeing about what is on screen.
    #[arg(long, value_name = "CHANNEL")]
    dump_overlay: Option<String>,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum LogFormat {
    Pretty,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Query mode: print resolved folders and exit before any tracing/runtime
    // setup, so stdout carries only the folder list for a caller to capture.
    if cli.list_folders {
        return list_folders(&cli.config);
    }

    // Same reasoning as `--list-folders`: stdout carries only the YAML, so a
    // caller can redirect it straight into a spec file.
    if let Some(channel) = cli.dump_overlay.as_deref() {
        return dump_overlay(&cli.config, channel);
    }

    // Tracing comes up before this one, unlike `--list-folders` above: the
    // container entrypoint runs `--render-etv-next` on every start, and config
    // loading now *warns* about a key it does not recognise rather than
    // refusing the file. Without a subscriber that warning goes nowhere, so the
    // one path a rollback actually takes would drop a key in silence. Rendering
    // writes files rather than a machine-parsed list, so nothing here is
    // reading its stdout.
    if let Some(dir) = cli.render_etv_next.as_deref() {
        init_tracing(cli.log_format);
        return render_etv_next(&cli.config, dir);
    }

    if let Some(channel) = cli.check_determinism.as_deref() {
        return check_determinism(&cli.config, channel);
    }

    if let Some(plexdb_path) = cli.reconcile_plexdb.as_deref() {
        return reconcile_plexdb_cmd(&cli.config, plexdb_path);
    }

    init_tracing(cli.log_format);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            tracing::error!(event = "runtime.error", error = %err, "failed to start tokio runtime");
            return ExitCode::from(1);
        }
    };

    runtime.block_on(async move {
        let station = match config::load(&cli.config) {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(event = "config.error", error = %err, "failed to load configuration");
                return ExitCode::from(1);
            }
        };

        tracing::info!(
            event = "station.load",
            station_config = %station.config_path.display(),
            tz = %station.station.tz,
            channels = station.channels.len(),
            "loaded station config",
        );
        for ch in &station.channels {
            tracing::info!(
                event = "channel.load",
                channel = %ch.name,
                config = %ch.config_path.display(),
                blocks = ch.config.rule.blocks.len(),
                output_folder = %ch.output_folder.display(),
                window_days = ch.config.window_days,
                chunk_hours = ch.config.chunk_hours,
                roll_interval_secs = ch.config.roll_interval.as_secs(),
                "loaded channel",
            );
        }

        match daemon::run(station).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                tracing::error!(event = "daemon.error", error = %err, "daemon failed");
                ExitCode::from(1)
            }
        }
    })
}

/// Load the station config and print each channel's `output_folder` verbatim,
/// one per line. Prints the value the daemon writes to (used as-is, relative to
/// the process CWD), so a caller polling these paths watches exactly the daemon's
/// output. Config-load failure goes to stderr with a non-zero exit.
fn list_folders(config_path: &Path) -> ExitCode {
    match config::load(config_path) {
        Ok(station) => {
            for ch in &station.channels {
                println!("{}", ch.output_folder.display());
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("failed to load configuration: {err}");
            ExitCode::from(1)
        }
    }
}

/// Print one channel's resolved overlay spec as YAML and exit.
///
/// Deliberately the channel's `base` — the station → channel resolution, which
/// is the spawn config its overlay process actually runs — and not a per-block
/// resolution: a preview has no schedule, so there is no block to be inside of.
fn dump_overlay(config_path: &Path, channel_name: &str) -> ExitCode {
    match resolve_overlay_for(config_path, channel_name) {
        Ok(yaml) => {
            print!("{yaml}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("dump-overlay: {err}");
            ExitCode::from(1)
        }
    }
}

/// Just enough of a config file to find its `overlay:` and, for the station,
/// where its channels live.
///
/// A narrow read rather than `config::load`, on purpose. A full load resolves
/// pools, which opens every datastore a channel names — so asking "what does
/// this channel draw" would fail on a deploy host's `${PLEXDB_SNAPSHOT_PATH}`
/// being absent from the machine running the preview. Nothing about the
/// cascade needs any of that. The cascade itself is NOT reimplemented here:
/// `OverlayDecl` is the real type and `resolve_decl`/`load_chain` are the real
/// functions the daemon uses.
#[derive(serde::Deserialize)]
struct OverlayProbe {
    #[serde(default)]
    overlay: Option<config::OverlayDecl>,
    #[serde(default)]
    channels: Vec<String>,
}

fn read_probe(path: &Path) -> Result<OverlayProbe, String> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    serde_norway::from_str(&raw).map_err(|e| format!("parsing {}: {e}", path.display()))
}

fn resolve_overlay_for(config_path: &Path, channel_name: &str) -> Result<String, String> {
    // Absolute from here down, so the emitted spec's `script:` and logo paths
    // work from whatever directory the caller later loads it in — a preview
    // writes the spec to a temp dir and renders it from there.
    let absolute = config_path
        .canonicalize()
        .map_err(|e| format!("resolving {}: {e}", config_path.display()))?;
    let station_dir = absolute.parent().unwrap_or(Path::new("."));
    let station = read_probe(&absolute)?;

    // Same globs the loader walks, matched on the name a channel is known by:
    // its directory (deploy/appdata's layout) or its file stem (examples').
    let mut channel_path = None;
    let mut known = Vec::new();
    for pattern in &station.channels {
        let full = station_dir.join(pattern);
        let entries = glob::glob(&full.to_string_lossy())
            .map_err(|e| format!("bad channels pattern {pattern:?}: {e}"))?;
        for entry in entries.flatten() {
            let name = if entry.file_name().and_then(|n| n.to_str()) == Some("channel.yaml") {
                entry.parent().and_then(|p| p.file_name())
            } else {
                entry.file_stem()
            }
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
            if name == channel_name {
                channel_path = Some(entry.clone());
            }
            known.push(name);
        }
    }
    let Some(channel_path) = channel_path else {
        known.sort();
        return Err(format!(
            "no channel named {channel_name:?}. Known: {}",
            known.join(", ")
        ));
    };

    let channel = read_probe(&channel_path)?;
    let channel_dir = channel_path.parent().unwrap_or(Path::new("."));

    let station_level = station.overlay.as_ref().map(|d| (d, station_dir));
    let channel_level = channel.overlay.as_ref().map(|d| (d, channel_dir));
    let chain = config::resolve_decl(station_level, channel_level, None);
    let Some(spec) = config::load_chain(&chain)? else {
        return Err(format!(
            "channel {channel_name:?} resolves to no overlay (nothing declared, or `clear`)"
        ));
    };
    serde_norway::to_string(&spec).map_err(|e| format!("serializing the resolved overlay: {e}"))
}

/// Render ETV-next's config from the station config and exit. Progress goes to
/// stdout so the container entrypoint's log shows what the lineup ended up as;
/// a failure is fatal, since ETV-next would otherwise boot on stale config.
fn render_etv_next(config_path: &Path, out_dir: &Path) -> ExitCode {
    let opts = match etv_next::RenderOptions::from_env(out_dir.to_path_buf()) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("render-etv-next: {err}");
            return ExitCode::from(1);
        }
    };
    match etv_next::render(config_path, &opts) {
        Ok(rendered) => {
            println!(
                "render-etv-next: {} + {} channel file(s) (bind={} port={} device_id={})",
                rendered.lineup_path.display(),
                rendered.channels,
                opts.bind_address,
                opts.port,
                rendered.device_id,
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("render-etv-next: {err}");
            ExitCode::from(1)
        }
    }
}

/// Generate `channel_name` twice from identical inputs and print whether the
/// two schedules match — see `etv_station::determinism::check` (#168) for
/// what "identical inputs" means and what this can and cannot catch. Exit
/// code carries the verdict as well as failure, so a script driving this in
/// CI can rely on the exit code alone: 0 only for a proven-identical pair of
/// passes.
fn check_determinism(config_path: &Path, channel_name: &str) -> ExitCode {
    let mut station = match config::load(config_path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("check-determinism: failed to load configuration: {err}");
            return ExitCode::from(1);
        }
    };
    match etv_station::determinism::check(&mut station, channel_name) {
        Ok(report) if report.is_identical() => {
            println!(
                "check-determinism: {} — identical ({} items)",
                report.channel, report.pass_a_len,
            );
            ExitCode::SUCCESS
        }
        Ok(report) => {
            let diff = report
                .difference
                .as_ref()
                .expect("checked above: identical branch already handled");
            println!(
                "check-determinism: {} — DIFFERS at position {}: pass A = {}, pass B = {} \
                 (lengths {} vs {})",
                report.channel,
                diff.position,
                diff.entry_a.as_deref().unwrap_or("<none — pass A ended>"),
                diff.entry_b.as_deref().unwrap_or("<none — pass B ended>"),
                report.pass_a_len,
                report.pass_b_len,
            );
            ExitCode::from(1)
        }
        Err(err) => {
            eprintln!("check-determinism: {err}");
            ExitCode::from(1)
        }
    }
}

/// Load the station config, open its catalog and the named `plex-db-ex`
/// snapshot both read-only, compare `entry_id` against `item_id`, and print
/// the report. See `catalog::reconcile_plexdb` (#269) for what the
/// comparison does and why it lives here rather than in `plex-db-ex`. Exit
/// code is non-zero when anything mismatches, so this can run as a check
/// rather than be read by eye.
fn reconcile_plexdb_cmd(config_path: &Path, plexdb_path: &Path) -> ExitCode {
    let station = match config::load(config_path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("reconcile-plexdb: failed to load configuration: {err}");
            return ExitCode::from(1);
        }
    };
    let Some(catalog_path) = station.station.catalog_path.clone() else {
        eprintln!(
            "reconcile-plexdb: station config at {} has no catalog_path set — nothing to compare",
            config_path.display()
        );
        return ExitCode::from(1);
    };
    let catalog = match Catalog::open_readonly(&catalog_path) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("reconcile-plexdb: failed to open catalog at {catalog_path}: {err}");
            return ExitCode::from(1);
        }
    };
    let plexdb_conn = match reconcile_plexdb::open_plexdb_readonly(plexdb_path) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("reconcile-plexdb: {err}");
            return ExitCode::from(1);
        }
    };
    let report = match reconcile_plexdb::reconcile(&catalog, &plexdb_conn) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("reconcile-plexdb: {err}");
            return ExitCode::from(1);
        }
    };

    print_reconcile_report(&report);

    if report.mismatched() > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn print_reconcile_report(report: &ReconcileReport) {
    println!(
        "compared {} title(s) present in both stores: {} agree, {} differ",
        report.compared,
        report.agree,
        report.mismatched(),
    );
    for mismatch in &report.mismatches {
        println!(
            "  rating_key={} title={:?} entry_id={} item_id={}",
            mismatch.rating_key, mismatch.title, mismatch.entry_id, mismatch.item_id,
        );
        println!("    reason: {}", mismatch.reason);
    }
    print_one_sided(
        "in etv-station only (plex-db-ex never ingested this rating key from Plex)",
        &report.only_in_etv,
    );
    print_one_sided(
        "in plex-db-ex only (this repository's walk never saw this rating key)",
        &report.only_in_plexdb,
    );
}

fn print_one_sided(label: &str, rows: &[(String, String)]) {
    println!("{} title(s) {}", rows.len(), label);
    for (rating_key, title) in rows.iter().take(reconcile_plexdb::ONE_SIDED_SAMPLE_LIMIT) {
        println!("  rating_key={rating_key} title={title:?}");
    }
    if rows.len() > reconcile_plexdb::ONE_SIDED_SAMPLE_LIMIT {
        println!(
            "  … and {} more",
            rows.len() - reconcile_plexdb::ONE_SIDED_SAMPLE_LIMIT
        );
    }
}

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Pretty => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}
