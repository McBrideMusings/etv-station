//! Debug tool for any `pool_provider` scorer plugin pool (#74) — taste-cosine
//! (#254, #278) and endless-distance (#176) both work here unmodified: runs
//! the *real* `pick()` twice against the real catalog and plexdb-reader
//! snapshot — once as a live generation would (real target_count, real
//! `config:`) and once with `exploration_fraction` forced to 0 (a no-op for a
//! script that doesn't read that key, like endless-distance) and a huge
//! target_count, so a script that ranks or drains a candidate set gives up
//! its whole ordering — and prints both. Whatever `metadata` the script
//! attaches (taste-cosine's `score`/`on_profile_keywords`/`source`,
//! endless-distance's `distance`) is dumped as-is, field names unassumed.
//!
//! Reuses `etv_station::score::{ScoreCache, pick}` exactly as
//! `pattern::resolve_pool_sources` does, so there is no second
//! implementation of the cosine math to drift from the script.
//!
//! Usage:
//!   cargo run --bin taste-debug -- \
//!     --channel deploy/appdata/channels/002-for-pierce.yaml \
//!     --catalog /path/to/catalog.db \
//!     --account-id 12345
//!
//! Omit `--account-id` to score against the pooled (house-wide) taste vector,
//! matching what 001-for-you.yaml does.

use std::path::{Path, PathBuf};

use clap::Parser;
use etv_station::catalog::Catalog;
use etv_station::config::{self, DatastoreGrant};
use etv_station::score::{GrantedCapabilities, PickedItem, ScoreCache, ScoreInputs};
use etv_station::tautulli::{self, HistoryScope};

#[derive(Parser, Debug)]
#[command(
    about = "Explain a scorer plugin pool's ranking against the real catalog + plexdb snapshot"
)]
struct Cli {
    /// Channel config file (e.g. deploy/appdata/channels/002-for-pierce.yaml).
    #[arg(long)]
    channel: PathBuf,

    /// The station's catalog sqlite database.
    #[arg(long)]
    catalog: PathBuf,

    /// Which pool in the channel's rule.blocks[0] to explain.
    #[arg(long, default_value = "movies")]
    pool: String,

    /// Plex account id to score against. A `single_user`-scoped channel
    /// (`scoring.taste_scope: single_user`) resolves this on its own, from
    /// its own `scoring.user`, via Tautulli — same as a live generation —
    /// so this is only needed to override that, or when TAUTULLI_URL/
    /// TAUTULLI_API_KEY aren't set. Omit entirely on an `all_users` channel
    /// like 001-for-you.yaml for the pooled (house-wide) taste vector.
    #[arg(long)]
    account_id: Option<i64>,

    /// Override the plexdb snapshot path instead of reading it from the
    /// pool's own `datastores:` entry (whose path may be an unexpanded
    /// `${VAR}` — read_channel does not expand it; this tool does, from the
    /// current environment, same as `source .env`).
    #[arg(long)]
    plexdb: Option<PathBuf>,

    /// How many rows of the full ranking to print.
    #[arg(long, default_value_t = 30)]
    top: usize,

    /// target_count for the "as it would really air" run — match the
    /// channel's pattern step take if you want a realistic slate.
    #[arg(long, default_value_t = 20)]
    target_count: usize,

    /// target_count for the second, extended run (exploration_fraction
    /// forced to 0). A pure ranker like taste-cosine drains this instantly
    /// regardless of size, so its default is generous; a sequential walk
    /// like endless-distance does real per-step work for every one of these,
    /// so raise it only as far as you need — thousands of steps can take
    /// minutes.
    #[arg(long, default_value_t = 200)]
    extended_target_count: usize,

    /// Skip the second, extended run entirely — just the realistic slate.
    #[arg(long)]
    no_extended: bool,
}

fn expand_env(raw: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unterminated ${{ in {raw:?}"))?;
        let var = &after[..end];
        let val = std::env::var(var)
            .map_err(|_| format!("env var `{var}` referenced by {raw:?} is not set"))?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn main() -> Result<(), String> {
    let cli = Cli::parse();

    let channel_dir = cli
        .channel
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let channel = config::read_channel(&cli.channel).map_err(|e| e.to_string())?;
    let block = channel
        .rule
        .blocks
        .first()
        .ok_or_else(|| format!("{}: rule has no blocks", cli.channel.display()))?;
    let pool_cfg = block
        .pools
        .iter()
        .find(|p| p.name == cli.pool)
        .ok_or_else(|| {
            format!(
                "{}: no pool named {:?} (have: {})",
                cli.channel.display(),
                cli.pool,
                block
                    .pools
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let plugin = pool_cfg.plugin.as_ref().ok_or_else(|| {
        format!(
            "pool {:?} has no `plugin:` — this tool only explains scorer plugin pools",
            cli.pool
        )
    })?;
    let script_path = if plugin.is_absolute() {
        plugin.clone()
    } else {
        channel_dir.join(plugin)
    };

    let plexdb_path = match &cli.plexdb {
        Some(p) => p.clone(),
        None => {
            let grant = pool_cfg.datastores.first().ok_or_else(|| {
                format!("pool {:?} declares no datastores; pass --plexdb", cli.pool)
            })?;
            PathBuf::from(expand_env(&grant.path)?)
        }
    };
    let datastore_name = pool_cfg
        .datastores
        .first()
        .map(|g| g.name.clone())
        .unwrap_or_else(|| "taste".to_string());

    // A single_user channel resolves its own account the same way a live
    // generation does — from its own scoring.user, via Tautulli — so this
    // tool needs no hardcoded name-to-id table (and commits none: channel
    // configs naming a real person live only under deploy/appdata, which is
    // gitignored). --account-id overrides; an all_users channel needs no
    // resolution at all.
    let account_id = match cli.account_id {
        Some(id) => Some(id),
        None => match channel.history_scope() {
            HistoryScope::AllUsers => None,
            scope @ HistoryScope::User(_) => {
                let (url, key) = tautulli::credentials_from_env().ok_or_else(|| {
                    format!(
                        "{}: scoring.taste_scope is single_user, but TAUTULLI_URL/\
                         TAUTULLI_API_KEY aren't set (source .env) — pass --account-id instead",
                        cli.channel.display()
                    )
                })?;
                let rows = tautulli::fetch_rows(&url, &key, &scope);
                tautulli::resolve_account_id(&scope, &rows)?
            }
        },
    };

    let catalog = Catalog::open_readonly(&cli.catalog).map_err(|e| e.to_string())?;

    let mut cache = ScoreCache::default();
    cache
        .prepare(&catalog, &script_path, pool_cfg.sources.as_ref())
        .map_err(|e| format!("prepare: {e}"))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let base_inputs = ScoreInputs {
        target_count: cli.target_count,
        now,
        account_id,
        ..Default::default()
    };

    let grant = |plexdb: &Path, name: &str| -> Result<GrantedCapabilities, String> {
        GrantedCapabilities::from_names(&pool_cfg.capabilities)
            .with_datastores(&[DatastoreGrant {
                name: name.to_string(),
                path: plexdb.display().to_string(),
            }])
    };

    // Run 1: realistic generation — the channel's own pool_config, real
    // target_count, exploration on. This is what would actually air.
    let selected = etv_station::score::pick(
        &cache,
        &script_path,
        pool_cfg.sources.as_ref(),
        &base_inputs,
        0,
        &cli.pool,
        pool_cfg.config.as_ref(),
        grant(&plexdb_path, &datastore_name)?,
    )
    .map_err(|e| format!("pick (selected): {e}"))?;

    // Run 2 (optional): extended run — same script, same candidates,
    // exploration forced off (a no-op for a script with no such tunable) and
    // a larger target_count, so a pure ranker like taste-cosine gives up its
    // whole ordering and a sequential walk like endless-distance shows more
    // steps than a live generation would ever ask for. Same real code path,
    // no reimplementation of either script's own math.
    let full = if cli.no_extended {
        None
    } else {
        let mut full_config = pool_cfg
            .config
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(obj) = full_config.as_object_mut() {
            obj.insert("exploration_fraction".to_string(), serde_json::json!(0.0));
        }
        let full_inputs = ScoreInputs {
            target_count: cli.extended_target_count,
            now,
            account_id,
            ..Default::default()
        };
        Some(
            etv_station::score::pick(
                &cache,
                &script_path,
                pool_cfg.sources.as_ref(),
                &full_inputs,
                0,
                &cli.pool,
                Some(&full_config),
                grant(&plexdb_path, &datastore_name)?,
            )
            .map_err(|e| format!("pick (extended run): {e}"))?,
        )
    };

    let title_of = |id: &str| -> String {
        match catalog.entry(id) {
            Ok(Some(e)) => match e.year {
                Some(y) => format!("{} ({y})", e.title),
                None => e.title,
            },
            _ => "<unknown>".to_string(),
        }
    };

    let who = match account_id {
        Some(id) => format!("account {id}"),
        None => "pooled (house-wide)".to_string(),
    };
    println!(
        "== {} / pool {:?}, scored against {who} ==",
        cli.channel.display(),
        cli.pool
    );
    println!("plugin:  {}", script_path.display());
    println!("plexdb:  {}", plexdb_path.display());
    println!();

    println!(
        "-- What this generation would air ({} slots, target_count={}) --",
        selected.len(),
        cli.target_count
    );
    print_rows(&selected, &title_of, selected.len());

    if let Some(full) = &full {
        println!();
        println!(
            "-- Extended run, exploration_fraction=0, target_count={} ({} candidates total, showing top {}) --",
            cli.extended_target_count,
            full.len(),
            cli.top.min(full.len())
        );
        let selected_ids: std::collections::HashSet<&str> =
            selected.iter().map(|p| p.id.as_str()).collect();
        print_rows_marked(full, &title_of, cli.top, &selected_ids);
    }

    Ok(())
}

fn print_rows(items: &[PickedItem], title_of: &impl Fn(&str) -> String, limit: usize) {
    print_rows_marked(items, title_of, limit, &std::collections::HashSet::new());
}

/// Prints whatever `metadata` keys the script actually attached, in whatever
/// shape it chose — `score`/`on_profile_keywords`/`source` for taste-cosine,
/// `distance` for endless-distance, anything a future plugin adds. No field
/// names are hardcoded: metadata is opaque to the station (ADR 0002) and this
/// tool has no more business assuming its shape than the daemon does.
fn print_rows_marked(
    items: &[PickedItem],
    title_of: &impl Fn(&str) -> String,
    limit: usize,
    also_in: &std::collections::HashSet<&str>,
) {
    for (i, item) in items.iter().take(limit).enumerate() {
        let title = title_of(&item.id);
        let fields = match &item.metadata {
            Some(serde_json::Value::Object(map)) => map
                .iter()
                .map(|(k, v)| format!("{k}={}", format_value(v)))
                .collect::<Vec<_>>()
                .join(" "),
            Some(v) => format_value(v),
            None => String::new(),
        };
        let mark = if !also_in.is_empty() && also_in.contains(item.id.as_str()) {
            " [SELECTED]"
        } else {
            ""
        };
        println!("{:3}. {:<45} {}{}", i + 1, title, fields, mark);
    }
}

fn format_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(format_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => match n.as_f64() {
            Some(f) => format!("{f:.4}"),
            None => n.to_string(),
        },
        other => other.to_string(),
    }
}
