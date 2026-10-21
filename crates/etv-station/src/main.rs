use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use etv_station::catalog::Catalog;
use etv_station::catalog::reconcile_plexdb::{self, ReconcileReport};
use etv_station::{audit_report, config, daemon, etv_next};
use time::OffsetDateTime;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "etv-station",
    about = "Playout JSON generator daemon for ErsatzTV-next"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the top-level station config (TOML or YAML, by extension).
    ///
    /// Required by every mode except `--check-plugins`, which reads a
    /// directory of scripts and nothing else, and `bumper-preview`, which
    /// needs it only to look up a catalog `entry_id`.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Log output format.
    #[arg(long, value_enum, default_value_t = LogFormat::Pretty)]
    log_format: LogFormat,

    /// Print each channel's resolved output_folder (one per line) and exit,
    /// instead of running the daemon. tools/dev-run.sh uses this to discover
    /// the folders to poll through the daemon's own config loader, so it can
    /// never disagree with where the daemon actually writes.
    #[arg(long)]
    list_folders: bool,

    /// Print every configured event hook (station and channel `hooks:`), one
    /// per line, and exit instead of running the daemon. Backs `admin hooks`.
    #[arg(long)]
    list_hooks: bool,

    /// Render ETV-next's lineup.json + channelN.json into this directory from
    /// the station config, then exit. The `ffmpeg`/`normalization` playback
    /// profile every channel body is built from comes from the station config
    /// itself now, not a file in this directory. Display names come from each
    /// channel's own `display_name` (#158) — there is no `presentation.json`.
    /// The container entrypoint runs this before starting ETV-next, so the
    /// playout folders it reads are always the ones the daemon writes.
    #[arg(long, value_name = "DIR")]
    render_etv_next: Option<PathBuf>,

    /// Do everything the container's start does to the config — load it with
    /// this process's environment and render ETV-next's config from it, into
    /// a throwaway directory — then exit non-zero if that would fail OR would
    /// leave any channel config out of the lineup, naming each one and why.
    ///
    /// `--render-etv-next` deliberately serves the channels that load and
    /// drops the rest, which is right for a running station and wrong for a
    /// deploy: `admin deploy`'s preflight runs this from the new image,
    /// before the running container is replaced, so a config the new image
    /// cannot read refuses the deploy instead of taking channels off the air.
    #[arg(long)]
    check_config: bool,

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
    /// This exists so a preview can show the real cascade instead of reimplementing it. Since ADR
    /// 0008 a channel may declare only `overlay: {extend: …}`, which carries no
    /// geometry and means nothing without the station-level spec above it — so
    /// a preview that reads the channel file alone can no longer resolve one.
    /// Reusing `config::overlay` here is what keeps the preview and the daemon
    /// from disagreeing about what is on screen.
    #[arg(long, value_name = "CHANNEL")]
    dump_overlay: Option<String>,

    /// Repair the play-history ledger from the playout files on disk, then
    /// exit. A one-shot: it inserts only the airings the ledger is missing and
    /// writes nothing else, so re-running it on a healthy station is a no-op.
    ///
    /// The ledger is written at generation time alongside the chunk JSON, so
    /// the two normally agree. A coverage heal used to truncate the ledger from
    /// the instant its wipe *started* while regeneration resumed at the
    /// surviving frontier, dropping rows for schedule that was still on disk
    /// and still aired. That is fixed at its source; this repairs the damage it
    /// already did — a channel whose ledger has a hole reads as having no past,
    /// so its resume cursor restarts every series and its adjacency seam stops
    /// seeing what just aired.
    ///
    /// Only what is still on disk can be recovered: `retention_days` prunes
    /// elapsed chunk files, and a hole older than that window is gone.
    #[arg(long)]
    backfill_history: bool,

    /// With `--backfill-history`, report what would be written and write
    /// nothing.
    #[arg(long, requires = "backfill_history")]
    dry_run: bool,

    /// Force a channel to throw away its schedule from the next item boundary
    /// forward and regenerate it, then exit. Names a channel the way its
    /// directory is (`002-for-pierce`), by bare name (`for-pierce`), or by
    /// channel number (`2`) — the `number:` the channel declares, the same
    /// number ETV-next and the guide use. `all` marks every channel.
    ///
    /// A refresh normally happens on its own when a channel's configuration
    /// changes. This is the override for when you want one anyway — after
    /// editing a plugin script, or when the catalog has moved and you would
    /// rather not wait for the window to roll forward.
    ///
    /// What is already airing is never touched: the regeneration starts at the
    /// finish of the item currently on screen, so nothing cuts mid-programme.
    ///
    /// This leaves a marker the running daemon consumes on its next pass rather
    /// than rewriting playout here — the daemon owns those files, and a second
    /// process writing them would race its generation loop.
    #[arg(long, value_name = "CHANNEL")]
    refresh_channel: Option<String>,

    /// With `--refresh-channel`, pin the channel's `seed:` to N first and
    /// reload the running daemon, so the regenerated future is a new shuffle.
    /// Rewrites the `seed:` line in the channel's own YAML (the file that
    /// declares the seed stays the record of it). Omit it to keep the seed and
    /// rebuild the same schedule. Names one channel, never `all`.
    #[arg(long, value_name = "N", requires = "refresh_channel")]
    seed: Option<u64>,

    /// Read the named channel's chunk files, take the next `--next` items
    /// from now forward, and write a report naming each item, its audit
    /// trail, and the overlay config in effect for it, then exit. Named the
    /// same way `--refresh-channel` accepts a channel (exact name, bare
    /// name, channel number, or `all`).
    ///
    /// With `--list` and no channel name, lists every channel instead of
    /// reporting one — see `--list`'s own doc comment.
    ///
    /// Reads only the chunk files already on disk: no generation runs, no
    /// catalog is opened, and no plugin is evaluated (ADR 0011) — this
    /// reports what will actually air, not what a re-simulation believes
    /// would air.
    #[arg(long, value_name = "CHANNEL", num_args = 0..=1, default_missing_value = "")]
    audit: Option<String>,

    /// With `--audit`, how many upcoming items to report.
    #[arg(long, value_name = "N", requires = "audit", default_value_t = 10)]
    next: usize,

    /// With `--audit` and no channel name, list every channel in the station
    /// config instead of reporting one: its declared name, its declared dial
    /// number (the same one ETV-next writes into `channelN.json` and the
    /// XMLTV guide shows as `ersatztv.<N>`), and its display name. Lets a
    /// caller map an XMLTV channel id back onto a channel.
    #[arg(long, requires = "audit")]
    list: bool,

    /// With `--audit`, write the report to this path instead of a generated
    /// `std::env::temp_dir()` path, then print nothing about it — the caller
    /// named the destination, so echoing it back adds nothing. `-` writes the
    /// report itself to stdout instead of a file, which is what
    /// `tools/audit-report.sh` uses to fetch a report over ssh in one remote
    /// call rather than a second read of the path the station printed
    /// (etv-station-400).
    ///
    /// With no `--audit-out`, behaviour is unchanged from before this flag
    /// existed: the report is written to `std::env::temp_dir()` and that path
    /// is printed.
    #[arg(long, value_name = "PATH", requires = "audit")]
    audit_out: Option<PathBuf>,

    /// Check every `*.rhai` in this directory against the plugin hook
    /// contract, print one line per script, and exit. Non-zero if any script
    /// fails to compile, declares no hooks, or declares a hook whose required
    /// functions it does not implement.
    ///
    /// Takes no `--config` and opens no catalog, deliberately: this is the
    /// only check that sees a script no channel references. Contract
    /// enforcement otherwise happens at config load, so it reaches exactly the
    /// scripts some pool names — and the host spent weeks carrying a
    /// `pool_provider` with no `audit()` whose sole reference was inside a
    /// YAML comment (#396). Point it at the plugin directory a host actually
    /// loads, before shipping a binary that requires a new contract function:
    /// `admin plugin-check` does exactly that, over ssh, in this image.
    #[arg(long, value_name = "DIR")]
    check_plugins: Option<PathBuf>,

    /// Serve the station's web dashboard (`/`) instead of
    /// running the daemon. It reads `--config`'s lineup and `catalog_path`
    /// (after `ETV_STATION_CATALOG`) and writes nothing to either. The
    /// container entrypoint runs this beside the daemon and ETV-next.
    #[arg(
        long,
        conflicts_with_all = [
            "list_folders", "list_hooks", "dump_overlay", "render_etv_next",
            "check_config", "check_determinism", "reconcile_plexdb",
            "backfill_history", "refresh_channel", "audit", "check_plugins",
        ]
    )]
    serve_ui: bool,

    /// Address `--serve-ui` listens on. The container sets `0.0.0.0`.
    #[arg(long, requires = "serve_ui", default_value = "127.0.0.1")]
    ui_bind: std::net::IpAddr,

    /// Port `--serve-ui` listens on.
    #[arg(long, requires = "serve_ui", default_value_t = 4747)]
    ui_port: u16,

    /// Score each pool with its namesake script in this directory instead of
    /// the deployed one its `plugin:` names — `tools/station-ui.sh` points it
    /// at this checkout's `examples/plugins/` to tune an in-development script.
    #[arg(long, requires = "serve_ui", value_name = "DIR")]
    ui_plugins_dir: Option<PathBuf>,

    /// Serve a private, migrated copy of the catalog (`<catalog>.ui-copy`)
    /// instead of the file itself, so a catalog fetched from a host running an
    /// older release still answers. `tools/station-ui.sh` sets it; the
    /// container, whose daemon owns and migrates its catalog, does not.
    #[arg(long, requires = "serve_ui")]
    ui_private_catalog: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Resolve one bumper script on one gap without a station, and print the
    /// playout items the scheduler would write for it (`--otio` prints the
    /// OpenTimelineIO document instead). Channels › Graphics renders the same
    /// result.
    ///
    /// A script that fails is an error here, not the black card the generator
    /// substitutes: a preview exists to show the author what went wrong.
    BumperPreview(BumperPreviewArgs),

    /// Rank the catalog tags a set of example titles share and print them as a
    /// category file (#175), for a person to read, cut and freeze.
    ///
    /// A suggestion, never a finished category: the same output, built into a
    /// query unreviewed, matched most of a curated set at 14-15% precision.
    /// One seed set per run — infer each distinct group separately.
    SuggestCategory(SuggestCategoryArgs),

    /// Write a show's arc ranges (#163) from anibridge/anibridge-mappings (#171).
    ///
    /// The source has season boundaries and no names, so each arc is named
    /// `Season N` as a placeholder for a person to replace before committing.
    /// Overlaps and gaps in the source are written as published and reported.
    ImportArcs(ImportArcsArgs),
}

#[derive(clap::Args, Debug)]
struct ImportArcsArgs {
    /// The show's exact Plex title, written as the file's `show:`.
    show: String,

    /// The upstream entry to read, as keyed in the source: `anilist:97940`.
    #[arg(long, value_name = "KEY")]
    key: String,

    /// A local `mappings.min.json`, instead of downloading the published one.
    #[arg(long, value_name = "PATH")]
    mappings: Option<PathBuf>,

    /// Write the arcs file here, instead of printing it.
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct SuggestCategoryArgs {
    /// Example titles, each `Title` or `Title (Year)`. At least two.
    #[arg(required = true, num_args = 1..)]
    seeds: Vec<String>,

    /// Name written into the emitted category.
    #[arg(long, default_value = "suggested")]
    name: String,

    /// Candidates kept per tag namespace.
    #[arg(long, value_name = "N", default_value_t = 15)]
    top: usize,

    /// Catalog database to read, instead of the station config's `catalog_path`.
    #[arg(long, value_name = "PATH")]
    catalog: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct BumperPreviewArgs {
    /// The bumper plugin (`.rhai`) to run.
    script: PathBuf,

    /// Length of the gap to fill: `45s`, `1500ms`, `2m`.
    #[arg(long)]
    gap: String,

    /// The item after the gap: a media file, or a catalog `entry_id` (which
    /// needs `--config`). Omit it to preview a gap at the generation frontier.
    #[arg(long, value_name = "PATH|ENTRY_ID")]
    next: Option<String>,

    /// A programme after `--next`, as `ctx.upcoming` lists it: a media file
    /// or a catalog `entry_id`. Repeat it, nearest first.
    #[arg(long, value_name = "PATH|ENTRY_ID")]
    upcoming: Vec<String>,

    /// The `config:` a fill entry would give the script, as JSON
    /// (`'{"mode":"upcoming"}'`); the script reads it as `ctx.config`.
    #[arg(long, value_name = "JSON")]
    script_config: Option<String>,

    /// The channel number `ctx.channel.number` reports; its name is
    /// `channel-<N>`.
    #[arg(long, value_name = "N", default_value_t = 1)]
    channel: i64,

    /// The channel framerate the solver rounds every boundary to.
    #[arg(long, default_value_t = 30.0)]
    fps: f64,

    /// Directory a relative overlay script path in the timeline is resolved
    /// against, so the printed `overlay.script` is absolute.
    #[arg(long, value_name = "DIR")]
    overlay_root: Option<PathBuf>,

    /// Print the resolved timeline as OpenTimelineIO JSON instead of items.
    #[arg(long, conflicts_with = "ndjson")]
    otio: bool,

    /// Print one playout item per line instead of one JSON array.
    #[arg(long)]
    ndjson: bool,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum LogFormat {
    Pretty,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(Command::BumperPreview(args)) = &cli.command {
        return bumper_preview(cli.config.as_deref(), args);
    }

    if let Some(Command::SuggestCategory(args)) = &cli.command {
        return suggest_category(cli.config.as_deref(), args);
    }

    if let Some(Command::ImportArcs(args)) = &cli.command {
        return import_arcs(args);
    }

    // First, and before `--config` is even resolved: this is the one mode that
    // has no station config to resolve. It reads a directory of scripts, which
    // is what lets it run against a host's plugin folder on a machine that
    // holds no channel configs at all.
    if let Some(dir) = cli.check_plugins.as_deref() {
        return check_plugins(dir);
    }

    let config: &Path = match cli.config.as_deref() {
        Some(path) => path,
        None => {
            eprintln!("--config is required (only --check-plugins runs without one)");
            return ExitCode::FAILURE;
        }
    };

    // Query mode: print resolved folders and exit before the daemon's tracing
    // and runtime setup. `load_for` logs to stderr only, so stdout carries just
    // the folder list for a caller to capture.
    if cli.list_folders {
        return list_folders(config);
    }

    if cli.list_hooks {
        return list_hooks(config);
    }

    // Same reasoning as `--list-folders`: stdout carries only the YAML, so a
    // caller can redirect it straight into a spec file.
    if let Some(channel) = cli.dump_overlay.as_deref() {
        return dump_overlay(config, channel);
    }

    // Tracing comes up on stdout before this one, unlike `--list-folders` above: the
    // container entrypoint runs `--render-etv-next` on every start, and config
    // loading now *warns* about a key it does not recognise rather than
    // refusing the file. Without a subscriber that warning goes nowhere, so the
    // one path a rollback actually takes would drop a key in silence. Rendering
    // writes files rather than a machine-parsed list, so nothing here is
    // reading its stdout.
    if let Some(dir) = cli.render_etv_next.as_deref() {
        init_tracing(cli.log_format);
        return render_etv_next(config, dir);
    }

    if cli.check_config {
        init_tracing(cli.log_format);
        return check_config(config);
    }

    if cli.serve_ui {
        init_tracing(cli.log_format);
        return serve_ui(
            config,
            cli.ui_bind,
            cli.ui_port,
            cli.ui_plugins_dir,
            cli.ui_private_catalog,
        );
    }

    if let Some(channel) = cli.check_determinism.as_deref() {
        return check_determinism(config, channel);
    }

    if let Some(plexdb_path) = cli.reconcile_plexdb.as_deref() {
        return reconcile_plexdb_cmd(config, plexdb_path);
    }

    if cli.backfill_history {
        return backfill_history(config, cli.dry_run);
    }

    if let Some(target) = cli.refresh_channel.as_deref() {
        return refresh_channel(config, target, cli.seed);
    }

    if let Some(target) = cli.audit.as_deref() {
        if cli.list {
            return list_channels(config);
        }
        return audit(config, target, cli.next, cli.audit_out.as_deref());
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

    let code = runtime.block_on(async move {
        let station = match config::load(config) {
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

        tokio::spawn(etv_station::memstat::run(std::time::Duration::from_secs(60)));

        match daemon::run(station).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                tracing::error!(event = "daemon.error", error = %err, "daemon failed");
                ExitCode::from(1)
            }
        }
    });
    // Dropping the runtime would join its blocking threads, and a Plex ingest
    // abandoned on shutdown can sit in a socket read for the whole HTTP
    // timeout. Every task that matters was joined by `daemon::run`.
    runtime.shutdown_background();
    code
}

/// Load the station config and print each channel's `output_folder` verbatim,
/// one per line. Prints the value the daemon writes to (used as-is, relative to
/// the process CWD), so a caller polling these paths watches exactly the daemon's
/// output. Config-load failure goes to stderr with a non-zero exit.
fn list_folders(config_path: &Path) -> ExitCode {
    let station = match load_for("list-folders", config_path) {
        Ok(s) => s,
        Err(code) => return code,
    };
    for ch in &station.channels {
        println!("{}", ch.output_folder.display());
    }
    ExitCode::SUCCESS
}

/// Load the station config and print one line per configured event hook (ADR
/// 0021): label, trigger, interval, kind, grants, resolved `run` path.
fn list_hooks(config_path: &Path) -> ExitCode {
    let station = match load_for("list-hooks", config_path) {
        Ok(s) => s,
        Err(code) => return code,
    };
    print!(
        "{}",
        etv_station::hooks::describe(&etv_station::hooks::collect(&station))
    );
    ExitCode::SUCCESS
}

/// Walk a directory of plugin scripts, print one line per script, and exit
/// non-zero if any of them would refuse to load.
///
/// The output is one `OK`/`FAIL` line per script plus a count, on stdout, with
/// no tracing subscriber in the way — this runs over ssh inside the container
/// image (`admin plugin-check`), where the whole answer needs to survive being
/// piped through two shells.
///
/// An empty directory is a pass with a count of zero, not an error: a station
/// with no plugin scripts is a legitimate configuration, and inventing a
/// failure there would make the check something people learn to skip.
fn check_plugins(dir: &Path) -> ExitCode {
    let checks = match etv_station::score::check_plugin_dir(dir) {
        Ok(checks) => checks,
        Err(err) => {
            eprintln!("check-plugins: {err}");
            return ExitCode::from(1);
        }
    };

    let mut failed = 0usize;
    for check in &checks {
        let name = check
            .path
            .file_name()
            .unwrap_or(check.path.as_os_str())
            .to_string_lossy();
        match &check.result {
            Ok((hooks, missing)) if missing.is_empty() => {
                println!("OK   {name}  hooks=[{}]", hooks.join(", "));
            }
            Ok((hooks, missing)) => {
                failed += 1;
                for req in missing {
                    println!(
                        "FAIL {name}  hooks=[{}]  missing {} — {}",
                        hooks.join(", "),
                        req.signature,
                        req.why
                    );
                }
            }
            Err(err) => {
                failed += 1;
                println!("FAIL {name}  {err}");
            }
        }
    }

    if failed == 0 {
        println!(
            "check-plugins: {} script(s) in {} satisfy the hook contract",
            checks.len(),
            dir.display()
        );
        ExitCode::SUCCESS
    } else {
        // Named as the failure it is on the host: these scripts load at
        // startup, so shipping a binary against them is a crash loop with
        // every channel dark, not one bad channel (#396).
        eprintln!(
            "check-plugins: {failed} of {} script(s) in {} would refuse to load — \
             a station running this binary against them fails to start",
            checks.len(),
            dir.display()
        );
        ExitCode::from(1)
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
    /// A channel file's declared identity (#414); absent on the station file.
    #[serde(default)]
    name: Option<String>,
}

fn read_probe(path: &Path) -> Result<OverlayProbe, String> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    serde_norway::from_str(&raw).map_err(|e| format!("parsing {}: {e}", path.display()))
}

fn resolve_overlay_for(config_path: &Path, channel_name: &str) -> Result<String, String> {
    // Absolute from here down, so the emitted spec's `script:` and image paths
    // work from whatever directory the caller later loads it in — a preview
    // writes the spec to a temp dir and renders it from there.
    let absolute = config_path
        .canonicalize()
        .map_err(|e| format!("resolving {}: {e}", config_path.display()))?;
    let station_dir = absolute.parent().unwrap_or(Path::new("."));
    let station = read_probe(&absolute)?;

    // Same globs the loader walks, matched on the name a channel is known by:
    // its declared `name:` (#414), never its file or folder name.
    let mut found: Option<(PathBuf, OverlayProbe)> = None;
    let mut known = Vec::new();
    for pattern in &station.channels {
        let full = station_dir.join(pattern);
        let entries = glob::glob(&full.to_string_lossy())
            .map_err(|e| format!("bad channels pattern {pattern:?}: {e}"))?;
        for entry in entries.flatten() {
            // One unreadable or unnamed channel file costs that channel, not
            // the preview of every other — the loader drops it the same way.
            // Named on stderr so the skipped channel isn't simply absent from
            // the "Known:" list; stdout carries only the YAML (#420).
            let probe = match read_probe(&entry) {
                Ok(probe) => probe,
                Err(err) => {
                    eprintln!("dump-overlay: skipping {err}");
                    continue;
                }
            };
            let Some(name) = probe.name.as_deref().map(str::trim).map(str::to_string) else {
                eprintln!(
                    "dump-overlay: skipping {}: declares no `name`",
                    entry.display()
                );
                continue;
            };
            if name == channel_name {
                // The loader drops every channel sharing a name, so a preview
                // must not pick one of them as if it served.
                if let Some((first, _)) = &found {
                    return Err(format!(
                        "channels {} and {} both declare name {channel_name:?}; \
                         neither loads",
                        first.display(),
                        entry.display()
                    ));
                }
                found = Some((entry.clone(), probe));
            }
            known.push(name);
        }
    }
    let Some((channel_path, channel)) = found else {
        known.sort();
        return Err(format!(
            "no channel named {channel_name:?}. Known: {}",
            known.join(", ")
        ));
    };
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

/// `--serve-ui`: resolve the catalog from the station config, then serve
/// until killed. Exits non-zero only when it cannot start.
fn serve_ui(
    config_path: &Path,
    bind: std::net::IpAddr,
    port: u16,
    plugins_dir: Option<PathBuf>,
    private_catalog: bool,
) -> ExitCode {
    let station = match config::load_for_inspection(config_path) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(event = "ui.config_error", error = %err, "failed to load configuration");
            return ExitCode::FAILURE;
        }
    };
    let Some(catalog) = station.station.catalog_path else {
        tracing::error!(
            event = "ui.config_error",
            config = %config_path.display(),
            "the station config sets no catalog_path (or ETV_STATION_CATALOG); the web UI needs one"
        );
        return ExitCode::FAILURE;
    };
    let mut catalog = PathBuf::from(catalog);
    if private_catalog {
        match etv_station::catalog::Catalog::migrated_copy(&catalog) {
            Ok(copy) => {
                tracing::info!(
                    event = "ui.private_catalog",
                    source = %catalog.display(),
                    copy = %copy.display(),
                    "serving a migrated private copy of the catalog"
                );
                catalog = copy;
            }
            Err(err) => {
                tracing::error!(event = "ui.config_error", error = %err, "failed to copy and migrate the catalog");
                return ExitCode::FAILURE;
            }
        }
    }
    let options = etv_station::web_ui::Options {
        station: config_path.to_path_buf(),
        catalog,
        plugins_dir,
        bind,
        port,
        health: etv_station::web_ui::HealthSources::from_env(),
    };
    match etv_station::web_ui::serve(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(event = "ui.start_failed", error = %err, "web UI failed to start");
            ExitCode::FAILURE
        }
    }
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

/// The strict twin of [`render_etv_next`]: the same load and render, into a
/// throwaway directory, but any dropped channel is a failure. See the
/// `--check-config` flag for why the two differ. It writes nothing outside
/// that directory — not even `.device_id` — so it runs against a read-only
/// config mount.
fn check_config(config_path: &Path) -> ExitCode {
    let out_dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("check-config: cannot create a scratch directory: {err}");
            return ExitCode::from(1);
        }
    };
    let opts = match etv_next::RenderOptions::from_env(out_dir.path().to_path_buf()) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("check-config: {err}");
            return ExitCode::from(1);
        }
    };
    let device_id = etv_next::DeviceIdSource::Fixed("check-config");
    let rendered = match etv_next::render_with(config_path, &opts, device_id) {
        Ok(rendered) => rendered,
        Err(etv_next::RenderError::NoChannels { path, dropped }) => {
            eprintln!(
                "check-config: no channel loads from {} ({} channel config(s) dropped):",
                path.display(),
                dropped.len()
            );
            print_dropped(&dropped);
            return ExitCode::from(1);
        }
        Err(err) => {
            eprintln!("check-config: {err}");
            return ExitCode::from(1);
        }
    };
    if rendered.dropped.is_empty() {
        println!(
            "check-config: {} channel(s) load from {}",
            rendered.channels,
            config_path.display()
        );
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "check-config: {} channel config(s) would be left out of the lineup ({} load):",
        rendered.dropped.len(),
        rendered.channels
    );
    print_dropped(&rendered.dropped);
    ExitCode::from(1)
}

fn print_dropped(dropped: &[etv_station::config::DroppedChannel]) {
    for d in dropped {
        eprintln!("{}", dropped_line(d));
    }
}

fn dropped_line(d: &etv_station::config::DroppedChannel) -> String {
    format!("  {}: {}", d.config_path.display(), d.reason)
}

/// Load the station config for a one-shot mode, saying on stderr which channel
/// configs the loader left out.
///
/// The loader drops a bad channel rather than failing the station (#263), and
/// its `channel.load_failed` event goes through tracing, which
/// [`init_stderr_tracing`] installs for these modes on stderr (a stdout
/// subscriber would corrupt `--list-folders`' output, which the container
/// entrypoint parses). Without this notice a dropped
/// channel simply isn't there, and "the station has 58" reads as the station's
/// real size (#420).
fn load_for(prefix: &str, config_path: &Path) -> Result<config::Station, ExitCode> {
    init_stderr_tracing();
    match config::load(config_path) {
        Ok(station) => {
            if let Some(notice) = dropped_notice(prefix, &station) {
                eprintln!("{notice}");
            }
            Ok(station)
        }
        Err(err) => {
            eprintln!("{prefix}: failed to load configuration: {err}");
            Err(ExitCode::from(1))
        }
    }
}

/// The stderr notice [`load_for`] prints, or `None` when every channel loaded.
fn dropped_notice(prefix: &str, station: &config::Station) -> Option<String> {
    if station.dropped.is_empty() {
        return None;
    }
    let mut notice = format!(
        "{prefix}: {} channel config(s) failed to load and are left out ({} load):",
        station.dropped.len(),
        station.channels.len(),
    );
    for d in &station.dropped {
        notice.push('\n');
        notice.push_str(&dropped_line(d));
    }
    Some(notice)
}

/// Generate `channel_name` twice from identical inputs and print whether the
/// two schedules match — see `etv_station::determinism::check` (#168) for
/// what "identical inputs" means and what this can and cannot catch. Exit
/// code carries the verdict as well as failure, so a script driving this in
/// CI can rely on the exit code alone: 0 only for a proven-identical pair of
/// passes.
/// Repair every channel's play-history ledger from its playout files.
///
/// Prints one line per channel that was missing rows and a total, so a healthy
/// station prints only the total. Exits non-zero when the pass itself fails —
/// finding nothing to repair is success, not failure.
fn backfill_history(config_path: &Path, dry_run: bool) -> ExitCode {
    let station = match load_for("backfill-history", config_path) {
        Ok(s) => s,
        Err(code) => return code,
    };

    // Read-only: the repair reads `show_id` per entry and writes nothing here.
    // A missing catalog is not fatal — the rows still go in, but episodes land
    // without their show, which leaves those series unable to resume. Say so
    // rather than silently writing a half-useful ledger.
    let catalog = match station.station.catalog_path.as_deref() {
        Some(path) => match Catalog::open_readonly(path) {
            Ok(c) => Some(c),
            Err(err) => {
                eprintln!(
                    "backfill-history: WARNING: cannot open catalog at {path} ({err}); episodes \
                     will be recorded without their show_id and those series will not resume",
                );
                None
            }
        },
        None => {
            eprintln!(
                "backfill-history: WARNING: station config sets no catalog_path; episodes will \
                 be recorded without their show_id and those series will not resume",
            );
            None
        }
    };

    let history_db =
        match etv_station::history::HistoryDb::open(station.station.output_base.join("history.db"))
        {
            Ok(db) => db,
            Err(err) => {
                eprintln!("backfill-history: failed to open history db: {err}");
                return ExitCode::from(1);
            }
        };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("backfill-history: failed to start tokio runtime: {err}");
            return ExitCode::from(1);
        }
    };

    let report = match runtime.block_on(etv_station::backfill::run(
        &station,
        &history_db,
        catalog.as_ref(),
        dry_run,
    )) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("backfill-history: {err}");
            return ExitCode::from(1);
        }
    };

    let verb = if dry_run { "would insert" } else { "inserted" };
    for repair in report.repaired() {
        let span = match (repair.filled_from, repair.filled_to) {
            (Some(from), Some(to)) => format!(" covering {from} .. {to}"),
            _ => String::new(),
        };
        println!(
            "backfill-history: {} — {} {} of {} airings on disk{span}",
            repair.channel, verb, repair.inserted, repair.on_disk,
        );
    }
    println!(
        "backfill-history: {} channels, {} airings on disk, {verb} {}",
        report.channels.len(),
        report.on_disk(),
        report.inserted(),
    );
    ExitCode::SUCCESS
}

/// Mark one channel — or every channel — for a forced forward refresh.
///
/// Resolution is deliberately forgiving, because the three names for a channel
/// all show up in different places: the declared `name` is what the logs and
/// the playout folder use, the bare name is what a person says, and the number is
/// what the guide and the TV show. A prefix that matches more than one channel
/// is refused rather than guessed at.
fn refresh_channel(config_path: &Path, target: &str, seed: Option<u64>) -> ExitCode {
    let station = match load_for("refresh-channel", config_path) {
        Ok(s) => s,
        Err(code) => return code,
    };

    let matched = match resolve_channels(&station, target) {
        Ok(m) => m,
        Err(msg) => {
            eprintln!("refresh-channel: {msg}");
            return ExitCode::from(1);
        }
    };

    if matched.is_empty() {
        eprintln!("refresh-channel: no channel matching {target:?}");
        return ExitCode::from(1);
    }

    if let Some(seed) = seed {
        let [ch] = matched.as_slice() else {
            eprintln!(
                "refresh-channel: --seed names one channel; {target:?} matched {}",
                matched.len()
            );
            return ExitCode::from(1);
        };
        let rewritten = std::fs::read_to_string(&ch.config_path)
            .map_err(|e| format!("{}: {e}", ch.config_path.display()))
            .and_then(|text| etv_station::reset::set_seed(&text, seed))
            .and_then(|text| {
                let tmp = ch.config_path.with_extension("yaml.seed-tmp");
                std::fs::write(&tmp, text)
                    .and_then(|()| std::fs::rename(&tmp, &ch.config_path))
                    .map_err(|e| format!("{}: {e}", ch.config_path.display()))
            });
        if let Err(msg) = rewritten {
            eprintln!("refresh-channel: {msg}");
            return ExitCode::from(1);
        }
        println!(
            "refresh-channel: {}: seed: {seed} in {}",
            ch.name,
            ch.config_path.display()
        );
    }

    for ch in &matched {
        let path = ch.output_folder.join(".refresh-request");
        if let Some(parent) = path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            eprintln!("refresh-channel: {}: {err}", parent.display());
            return ExitCode::from(1);
        }
        if let Err(err) = std::fs::write(&path, b"") {
            eprintln!("refresh-channel: {}: {err}", path.display());
            return ExitCode::from(1);
        }
        println!("refresh-channel: marked {}", ch.name);
    }
    println!(
        "refresh-channel: {} channel(s) marked; the daemon regenerates each from the next item \
         boundary on its following pass",
        matched.len(),
    );
    if seed.is_some() {
        match etv_station::reset::signal_reload(&station.station.output_base) {
            Ok(etv_station::reset::Reload::Signalled(pid)) => {
                println!(
                    "refresh-channel: sent SIGHUP to the daemon (pid {pid}); it reloads the new seed now"
                );
            }
            Ok(etv_station::reset::Reload::NotRunning) => {
                println!(
                    "refresh-channel: no running daemon found; the new seed applies at its next start"
                );
            }
            Err(msg) => {
                eprintln!(
                    "refresh-channel: {msg}; the new seed applies at the daemon's next reload"
                );
                return ExitCode::from(1);
            }
        }
    }
    ExitCode::SUCCESS
}

/// Resolve `target` to the channels it names, the same forgiving way for
/// every one-shot mode that accepts a channel argument: exact name, bare name
/// after the numeric directory prefix (`for-pierce` finds `002-for-pierce`),
/// substring, channel number (the `number:` the channel declares — the same
/// number the guide and ETV-next show; gaps are legal), or `all`.
///
/// Messages carry no command prefix — each caller prepends its own, so
/// `refresh-channel`'s and `audit`'s errors read as their own tool's output.
fn resolve_channels<'a>(
    station: &'a config::Station,
    target: &str,
) -> Result<Vec<&'a config::LoadedChannel>, String> {
    if target == "all" {
        return Ok(station.channels.iter().collect());
    }
    if let Ok(number) = target.parse::<i64>() {
        return match station.channels.iter().find(|c| c.config.number == number) {
            Some(ch) => Ok(vec![ch]),
            None => {
                let mut numbers: Vec<i64> =
                    station.channels.iter().map(|c| c.config.number).collect();
                numbers.sort_unstable();
                Err(format!(
                    "no channel number {number}; the station has numbers {}",
                    numbers
                        .iter()
                        .map(i64::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                ))
            }
        };
    }

    let exact: Vec<_> = station
        .channels
        .iter()
        .filter(|c| c.name == target)
        .collect();
    if !exact.is_empty() {
        return Ok(exact);
    }

    // `for-pierce` should find `002-for-pierce`, since the numeric prefix is
    // a sort key rather than part of the name anyone says.
    let loose: Vec<_> = station
        .channels
        .iter()
        .filter(|c| {
            let bare = c.name.split_once('-').map(|(_, r)| r).unwrap_or(&c.name);
            bare == target || c.name.contains(target)
        })
        .collect();
    if loose.len() > 1 {
        return Err(format!(
            "{target:?} matches {} channels: {}",
            loose.len(),
            loose
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    if loose.is_empty() {
        return Err(format!("no channel matching {target:?}"));
    }
    Ok(loose)
}

/// Read `target`'s chunk files, take the next `next` items from now forward,
/// and write an audit report (text or JSON, per `format`) to `audit_out` —
/// or, with `audit_out` absent, to a generated temp file whose path is then
/// printed.
///
/// Reads only the chunk files already on disk: no catalog is opened and no
/// plugin runs on this path, which is the whole point (ADR 0011) — the
/// report describes what will actually air, not what a re-simulation of a
/// plugin's `pick()` believes would air.
fn audit(config_path: &Path, target: &str, next: usize, audit_out: Option<&Path>) -> ExitCode {
    let station = match load_for("audit", config_path) {
        Ok(s) => s,
        Err(code) => return code,
    };

    let matched = match resolve_channels(&station, target) {
        Ok(m) => m,
        Err(msg) => {
            eprintln!("audit: {msg}");
            eprintln!(
                "audit: known channels: {}",
                station
                    .channels
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            return ExitCode::from(1);
        }
    };
    let channel = match matched.as_slice() {
        [ch] => *ch,
        other => {
            eprintln!(
                "audit: {target:?} must name exactly one channel, matched {}: {}",
                other.len(),
                other
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            eprintln!(
                "audit: known channels: {}",
                station
                    .channels
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            return ExitCode::from(1);
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("audit: failed to start tokio runtime: {err}");
            return ExitCode::from(1);
        }
    };

    let now = OffsetDateTime::now_utc();
    let items = match runtime.block_on(audit_report::upcoming(&channel.output_folder, now, next)) {
        Ok(items) => items,
        Err(err) => {
            eprintln!("audit: {err}");
            return ExitCode::from(1);
        }
    };

    let rotations = match etv_station::tz::parse(&station.station.tz) {
        Ok(tz) => etv_station::rotation::preview_channel(
            &channel.config,
            tz,
            &etv_station::rotation::read_rotations(&channel.output_folder),
            now,
            next,
        ),
        Err(_) => Vec::new(),
    };
    let report = audit_report::render(&channel.name, now, &items)
        + etv_station::rotation::render_text(&rotations).as_str();
    match audit_out {
        Some(path) if path == Path::new("-") => {
            print!("{report}");
        }
        Some(path) => {
            if let Err(err) = std::fs::write(path, report) {
                eprintln!("audit: writing {}: {err}", path.display());
                return ExitCode::from(1);
            }
        }
        None => {
            let path = std::env::temp_dir().join(format!("etv-station-audit-{}.txt", channel.name));
            if let Err(err) = std::fs::write(&path, report) {
                eprintln!("audit: writing {}: {err}", path.display());
                return ExitCode::from(1);
            }
            println!("{}", path.display());
        }
    }
    ExitCode::SUCCESS
}

/// One line of `--audit --list`'s output: a channel's declared name, its
/// declared dial number, and its display name.
struct ChannelListing {
    name: String,
    number: i64,
    display_name: String,
}

/// Every channel in the station config, keyed by the same dial number
/// `etv_next.rs`'s `ChannelRender::number` writes into `channelN.json` —
/// read from the identical field (`ch.config.number`), never recomputed, so
/// the two cannot drift. `display_name` falls back to the identity, mirroring
/// `ChannelRender`'s and `etv_next.rs`'s own fallback (#158).
fn channel_listing(station: &config::Station) -> Vec<ChannelListing> {
    station
        .channels
        .iter()
        .map(|ch| ChannelListing {
            name: ch.name.clone(),
            number: ch.config.number,
            display_name: ch
                .config
                .display_name
                .clone()
                .unwrap_or_else(|| ch.name.clone()),
        })
        .collect()
}

/// Load the station config and print `--audit --list`'s channel listing, one
/// `number<TAB>name<TAB>display name` line per channel.
fn list_channels(config_path: &Path) -> ExitCode {
    let station = match load_for("audit", config_path) {
        Ok(s) => s,
        Err(code) => return code,
    };

    for c in &channel_listing(&station) {
        println!("{}\t{}\t{}", c.number, c.name, c.display_name);
    }
    ExitCode::SUCCESS
}

/// Extract divergence messages from a determinism report.
/// Returns the lines to print when the two passes disagree.
/// When entries differ, only the entry message prints. When entries match but
/// audits differ, only the audit message prints. Never both — audit divergence
/// is trivial when entries already differ.
fn divergence_messages(report: &etv_station::determinism::DeterminismReport) -> Vec<String> {
    let diff = match report.difference.as_ref() {
        Some(d) => d,
        None => return Vec::new(), // identical, no messages
    };

    if diff.entry_a != diff.entry_b {
        vec![format!(
            "check-determinism: {} — DIFFERS at position {}: pass A = {}, pass B = {} \
             (lengths {} vs {})",
            report.channel,
            diff.position,
            diff.entry_a.as_deref().unwrap_or("<none — pass A ended>"),
            diff.entry_b.as_deref().unwrap_or("<none — pass B ended>"),
            report.pass_a_len,
            report.pass_b_len,
        )]
    } else if diff.audit_a != diff.audit_b {
        vec![format!(
            "check-determinism: {} — audit trail at position {} differs: \
             pass A = {}, pass B = {}",
            report.channel,
            diff.position,
            diff.audit_a.as_deref().unwrap_or("<no audit trail>"),
            diff.audit_b.as_deref().unwrap_or("<no audit trail>"),
        )]
    } else {
        Vec::new()
    }
}

fn check_determinism(config_path: &Path, channel_name: &str) -> ExitCode {
    let mut station = match load_for("check-determinism", config_path) {
        Ok(s) => s,
        Err(code) => return code,
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
            for line in divergence_messages(&report) {
                println!("{line}");
            }
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
    let station = match load_for("reconcile-plexdb", config_path) {
        Ok(s) => s,
        Err(code) => return code,
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

/// A subscriber for the one-shot modes that writes to stderr, so the loader's
/// warnings (an unrecognised config key, a dropped channel) reach the operator
/// without touching stdout, which `--list-folders` and `--audit` callers parse.
/// Defaults to `warn` rather than the daemon's `info`; `RUST_LOG` overrides it.
/// `try_init` leaves an already-installed subscriber alone.
fn init_stderr_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .try_init();
}

/// `etv-station bumper-preview`. Stdout carries only the items (or OTIO), so
/// a caller can pipe it; the log goes to stderr.
fn bumper_preview(config_path: Option<&Path>, args: &BumperPreviewArgs) -> ExitCode {
    use etv_station::bumper::hook::ChannelInfo;
    use etv_station::bumper::preview::{self, PreviewRequest};

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let fail = |msg: String| {
        eprintln!("bumper-preview: {msg}");
        ExitCode::from(1)
    };

    let gap_ms = match preview::parse_gap(&args.gap) {
        Ok(ms) => ms,
        Err(e) => return fail(e),
    };
    let item_for = |spec: &str| -> Result<rhai::Map, String> {
        let catalog = match config_path {
            Some(path) if !std::path::Path::new(spec).is_file() => {
                Some(open_preview_catalog(path)?)
            }
            _ => None,
        };
        preview::next_item(spec, catalog.as_ref())
    };
    let next = match args.next.as_deref() {
        None => None,
        Some(spec) => match item_for(spec) {
            Ok(m) => Some(m),
            Err(e) => return fail(e),
        },
    };
    let upcoming = match args
        .upcoming
        .iter()
        .map(|spec| item_for(spec))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(items) => items,
        Err(e) => return fail(e),
    };
    let script_config = match args.script_config.as_deref() {
        None => None,
        Some(raw) => match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(v) => Some(v),
            Err(e) => return fail(format!("--script-config is not JSON: {e}")),
        },
    };
    let overlay_root = match args
        .overlay_root
        .clone()
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
    {
        Ok(p) => p,
        Err(e) => return fail(format!("cannot read the working directory: {e}")),
    };
    let request = PreviewRequest {
        script: &args.script,
        gap_ms,
        fps: args.fps,
        next,
        upcoming,
        config: script_config,
        channel: ChannelInfo {
            name: format!("channel-{}", args.channel),
            number: args.channel,
            ..ChannelInfo::default()
        },
        overlay_root: &overlay_root,
        start: OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .unwrap_or_else(|_| OffsetDateTime::now_utc()),
    };
    match preview::preview(&request) {
        Ok(p) => {
            if args.otio {
                println!("{}", p.otio());
            } else if args.ndjson {
                println!("{}", p.items_ndjson());
            } else {
                println!("{}", p.items_json());
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

/// The catalog `--config` names, opened read-only, for `--next <entry_id>`.
fn open_preview_catalog(config_path: &Path) -> Result<Catalog, String> {
    let station = config::load_for_inspection(config_path).map_err(|e| e.to_string())?;
    let path = station.station.catalog_path.ok_or_else(|| {
        format!(
            "{} sets no catalog_path (or ETV_STATION_CATALOG); cannot look up an entry_id",
            config_path.display()
        )
    })?;
    Catalog::open_readonly(&path).map_err(|e| format!("opening catalog {path}: {e}"))
}

fn suggest_category(config_path: Option<&Path>, args: &SuggestCategoryArgs) -> ExitCode {
    let fail = |msg: String| {
        eprintln!("suggest-category: {msg}");
        ExitCode::from(1)
    };
    let catalog = match (&args.catalog, config_path) {
        (Some(path), _) => Catalog::open_readonly(path)
            .map_err(|e| format!("opening catalog {}: {e}", path.display())),
        (None, Some(config)) => open_preview_catalog(config),
        (None, None) => Err("pass --catalog or --config".to_string()),
    };
    let catalog = match catalog {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    match etv_station::suggest::suggest(&catalog, &args.seeds, args.top) {
        Ok(s) => {
            print!("{}", etv_station::suggest::render(&s, &args.name));
            ExitCode::SUCCESS
        }
        Err(e) => fail(e.to_string()),
    }
}

fn import_arcs(args: &ImportArcsArgs) -> ExitCode {
    let fail = |msg: String| {
        eprintln!("import-arcs: {msg}");
        ExitCode::from(1)
    };
    let (source, bytes) = match &args.mappings {
        Some(path) => match std::fs::read(path) {
            Ok(b) => (path.display().to_string(), b),
            Err(e) => return fail(format!("reading {}: {e}", path.display())),
        },
        None => {
            let url = etv_station::arcs_import::DEFAULT_SOURCE_URL;
            let mut bytes = Vec::new();
            let got = ureq::get(url)
                .call()
                .map_err(|e| e.to_string())
                .and_then(|r| {
                    r.into_reader()
                        .take(256 << 20)
                        .read_to_end(&mut bytes)
                        .map_err(|e| e.to_string())
                });
            if let Err(e) = got {
                return fail(format!("downloading {url}: {e}"));
            }
            (url.to_string(), bytes)
        }
    };
    let mappings: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return fail(format!("{source} is not JSON: {e}")),
    };
    if let Some(path) = &args.out {
        let yaml = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"));
        if !yaml {
            return fail(format!(
                "--out {} must end in .yaml or .yml",
                path.display()
            ));
        }
    }
    let imp = match etv_station::arcs_import::import(&args.show, &args.key, &source, &mappings) {
        Ok(i) => i,
        Err(e) => return fail(e),
    };
    if let Some(err) = &imp.load_error {
        eprintln!(
            "import-arcs: wrote the source's ranges as published; the arcs loader rejects them: {err}"
        );
    }
    match &args.out {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &imp.text) {
                return fail(format!("writing {}: {e}", path.display()));
            }
            eprintln!("import-arcs: wrote {}", path.display());
        }
        None => print!("{}", imp.text),
    }
    ExitCode::SUCCESS
}

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Pretty => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but real channel config: one entries-block with one lavfi
    /// item, which is all `validate::validate_channel` requires to accept it
    /// — the same shape `config/load.rs`'s own `resolves_path_referenced_block`
    /// test writes.
    fn write_channel_toml(dir: &Path, filename: &str, number: i64, display_name: Option<&str>) {
        let name = filename.trim_end_matches(".toml");
        let display_line = display_name
            .map(|d| format!("display_name = {d:?}\n"))
            .unwrap_or_default();
        std::fs::write(
            dir.join(filename),
            format!(
                "number = {number}\n\
                 name = {name:?}\n\
                 {display_line}\
                 [[rule.blocks]]\n\
                 order = \"manual\"\n\
                 [[rule.blocks.entries]]\n\
                 kind = \"item\"\n\
                 [rule.blocks.entries.source]\n\
                 kind = \"lavfi\"\n\
                 params = \"testsrc\"\n"
            ),
        )
        .unwrap();
    }

    fn write_station_toml(dir: &Path, channels: &[&str]) {
        let channel_list = channels
            .iter()
            .map(|c| format!("{c:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            dir.join("station.toml"),
            format!(
                "output_base = {:?}\n\
                 channels = [{channel_list}]\n\
                 [normalization.audio]\n\
                 [normalization.video]\n",
                dir.join("out").to_string_lossy(),
            ),
        )
        .unwrap();
    }

    /// `--list`'s `number` for each channel must be exactly the
    /// declared `number:` — the same field `etv_next.rs`'s `ChannelRender`
    /// reads (`ch.config.number`) to write `channelN.json`. Two channels with
    /// non-sequential numbers proves this isn't secretly positional.
    #[test]
    fn channel_listing_number_matches_the_declared_config_field() {
        let dir = tempfile::tempdir().unwrap();
        write_channel_toml(dir.path(), "a.toml", 7, Some("Channel A"));
        write_channel_toml(dir.path(), "b.toml", 3, None);
        write_station_toml(dir.path(), &["a.toml", "b.toml"]);

        let station = config::load(&dir.path().join("station.toml")).unwrap();
        let listing = channel_listing(&station);

        assert_eq!(listing.len(), 2);
        let a = listing.iter().find(|c| c.name == "a").unwrap();
        assert_eq!(a.number, 7, "must be the declared number, not a position");
        assert_eq!(a.display_name, "Channel A");

        let b = listing.iter().find(|c| c.name == "b").unwrap();
        assert_eq!(b.number, 3);
        // No `display_name:` authored — falls back to identity, matching
        // `etv_next.rs`'s `ChannelRender`/render fallback (#158).
        assert_eq!(b.display_name, "b");

        // Cross-checked directly against the field `etv_next.rs` uses to
        // build `ChannelRender` — same source, not a second computation.
        for ch in &station.channels {
            let listed = listing.iter().find(|c| c.name == ch.name).unwrap();
            assert_eq!(listed.number, ch.config.number);
        }
    }

    /// A numeric channel argument matches the declared `number:`, never the
    /// channel's position; a miss refuses and names the numbers that exist (#421).
    #[test]
    fn resolve_channels_matches_declared_number_not_position() {
        let dir = tempfile::tempdir().unwrap();
        write_channel_toml(dir.path(), "a.toml", 7, None);
        write_channel_toml(dir.path(), "b.toml", 121, None);
        write_station_toml(dir.path(), &["a.toml", "b.toml"]);
        let station = config::load(&dir.path().join("station.toml")).unwrap();

        let hit = resolve_channels(&station, "121").unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].name, "b");

        // 2 is a valid position but not a declared number.
        let err = resolve_channels(&station, "2").unwrap_err();
        assert_eq!(err, "no channel number 2; the station has numbers 7, 121");
    }

    /// A one-shot mode names every channel config the loader dropped, with its
    /// path and reason, and stays silent when all of them loaded (#420).
    #[test]
    fn dropped_notice_names_each_dropped_channel() {
        let dir = tempfile::tempdir().unwrap();
        write_channel_toml(dir.path(), "a.toml", 1, None);
        write_station_toml(dir.path(), &["a.toml"]);
        let station = config::load(&dir.path().join("station.toml")).unwrap();
        assert_eq!(dropped_notice("audit", &station), None);

        std::fs::write(dir.path().join("b.toml"), "name = \"b\"\n").unwrap();
        write_station_toml(dir.path(), &["a.toml", "b.toml"]);
        let station = config::load(&dir.path().join("station.toml")).unwrap();
        let notice = dropped_notice("audit", &station).expect("b.toml was dropped");
        assert!(
            notice.starts_with(
                "audit: 1 channel config(s) failed to load and are left out (1 load):"
            ),
            "{notice}"
        );
        assert!(notice.contains("b.toml: "), "{notice}");
    }

    /// When entries differ, the output reports the entry divergence once.
    /// The audit trail line only prints when entries match but audits differ,
    /// not when entries already diverge (#402).
    #[test]
    fn determinism_check_entry_divergence_no_secondary_audit_line() {
        use etv_station::determinism::{DeterminismReport, Difference};

        // Simulate a Difference where entries diverge — entry_a = Some("item1"),
        // entry_b = Some("item2"), audits also differ trivially.
        let report = DeterminismReport {
            channel: "test-ch".to_string(),
            pass_a_len: 5,
            pass_b_len: 5,
            difference: Some(Difference {
                position: 2,
                entry_a: Some("item1".to_string()),
                entry_b: Some("item2".to_string()),
                audit_a: Some(r#"{"stage":"pool"}"#.to_string()),
                audit_b: Some(r#"{"stage":"sequence"}"#.to_string()),
            }),
        };

        let messages = divergence_messages(&report);
        assert_eq!(messages.len(), 1, "should produce exactly one message");
        assert!(
            messages[0].contains("DIFFERS at position 2"),
            "must report entry divergence"
        );
        assert!(
            !messages[0].contains("audit trail"),
            "must not report audit divergence when entries already differ"
        );
    }

    /// When entries match but audit trails differ, the output reports only the
    /// audit divergence (#402, #391). This detects non-deterministic `audit()`
    /// implementations even when `pick()` is stable.
    #[test]
    fn determinism_check_matching_entries_different_audits() {
        use etv_station::determinism::{DeterminismReport, Difference};

        // Simulate a Difference where entries match but audits differ.
        let report = DeterminismReport {
            channel: "test-ch".to_string(),
            pass_a_len: 3,
            pass_b_len: 3,
            difference: Some(Difference {
                position: 1,
                entry_a: Some("item1".to_string()),
                entry_b: Some("item1".to_string()),
                audit_a: Some(r#"{"stage":"pool","verdict":"picked"}"#.to_string()),
                audit_b: Some(r#"{"stage":"pool","verdict":"shuffled"}"#.to_string()),
            }),
        };

        let messages = divergence_messages(&report);
        assert_eq!(messages.len(), 1, "should produce exactly one message");
        assert!(
            messages[0].contains("audit trail at position 1 differs"),
            "must report audit divergence when entries match: {}",
            messages[0]
        );
        assert!(
            !messages[0].contains("DIFFERS at position"),
            "must not report entry divergence when entries match"
        );
    }
}
