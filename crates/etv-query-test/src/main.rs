use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

mod cache;
mod cases;
mod cel_eval;
mod fs_catalog;
mod normalize;
mod output;
mod plex;

#[derive(Parser)]
#[command(
    name = "etv-query-test",
    about = "Phase A — CEL feasibility study harness"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Batch {
        #[arg(long, default_value = "crates/etv-query-test/cases")]
        cases_dir: PathBuf,
    },
    Query {
        expr: Option<String>,
        /// Catalog to draw from: all (default), plex, fs
        #[arg(long, value_enum, default_value_t = Catalog::All)]
        source: Catalog,
        #[arg(long)]
        order: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
        format: OutputFormat,
    },
}

#[derive(Copy, Clone, ValueEnum, Default)]
pub enum Catalog {
    #[default]
    All,
    Plex,
    Fs,
}

#[derive(Copy, Clone, ValueEnum, Default)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Batch { cases_dir } => cases::run_batch(&cases_dir),
        Command::Query {
            expr,
            source,
            order,
            limit,
            format,
        } => run_query(expr, source, order, limit, format),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_query(
    expr: Option<String>,
    catalog: Catalog,
    order: Option<String>,
    limit: Option<usize>,
    format: OutputFormat,
) -> Result<(), Error> {
    let expr = expr
        .or_else(|| prompt_for("CEL expression or search term", None))
        .ok_or(Error::MissingArg("expression"))?;
    let ingest_start = std::time::Instant::now();
    let items = ingest_catalog(catalog)?;
    let ingest_elapsed = ingest_start.elapsed();

    let compile_start = std::time::Instant::now();
    let (program, effective_expr) = compile_with_freetext_fallback(&expr, items.first());
    let compile_elapsed = compile_start.elapsed();
    let program = program?;

    let eval_start = std::time::Instant::now();
    let mut matched: Vec<normalize::NormalizedItem> = items
        .iter()
        .filter(|item| program.matches(item).unwrap_or(false))
        .cloned()
        .collect();
    let eval_elapsed = eval_start.elapsed();

    if let Some(keys) = order.as_deref() {
        normalize::sort_by_keys(&mut matched, keys);
    }
    if let Some(n) = limit {
        matched.truncate(n);
    }

    output::render(
        format,
        &output::RenderInput {
            query: &effective_expr,
            candidate_count: items.len(),
            matched_count: matched.len(),
            ingest_elapsed,
            compile_elapsed,
            eval_elapsed,
            helpers_used: program.helpers_referenced(),
            items: &matched,
        },
    );
    Ok(())
}

/// Try the user's input as CEL; if it doesn't compile or doesn't evaluate to
/// a bool on a sample item, fall back to a free-text substring search across
/// title/categories/collections. Returns the compile result plus the
/// expression that was actually used (which differs from the input on
/// fallback — surfaced so the output panel shows what really ran).
pub(crate) fn compile_with_freetext_fallback(
    input: &str,
    sample: Option<&normalize::NormalizedItem>,
) -> (
    Result<cel_eval::CompiledProgram, cel_eval::CelError>,
    String,
) {
    let trimmed = input.trim();
    // cel_eval::compile() lowercases internally for case-insensitive matching.
    let compile_result = cel_eval::compile(trimmed);
    let looks_like_cel = ["==", "!=", "&&", "||", ">=", "<="]
        .iter()
        .any(|tok| trimmed.contains(tok))
        || trimmed.contains(['(', ')', '"']);

    // Keep the original compile result when:
    //   - it succeeds and either we have no sample or it evaluates on the sample, OR
    //   - it fails but the input clearly looks like CEL (so the user sees the real error).
    // When compile succeeds but sample eval errors (e.g. type mismatch), we fall
    // through to free-text. Intentional for a study harness: bad expressions silently
    // yield 0 matches rather than hard-stopping the run.
    let keep_original = match (&compile_result, looks_like_cel) {
        (Ok(program), _) => sample.is_none_or(|item| program.matches(item).is_ok()),
        (Err(_), true) => true,
        _ => false,
    };
    if keep_original {
        return (compile_result, trimmed.to_string());
    }

    let synthesized = cel_eval::free_text_expression(trimmed);
    let res = cel_eval::compile(&synthesized);
    (res, synthesized)
}

fn prompt_for(label: &str, default: Option<&str>) -> Option<String> {
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return None;
    }
    let mut stdout = std::io::stdout();
    let suffix = match default {
        Some(d) => format!(" [{d}]"),
        None => String::new(),
    };
    write!(stdout, "{label}{suffix}: ").ok()?;
    stdout.flush().ok()?;
    let mut buf = String::new();
    stdin.lock().read_line(&mut buf).ok()?;
    let trimmed = buf.trim().to_string();
    if trimmed.is_empty() {
        default.map(str::to_string)
    } else {
        Some(trimmed)
    }
}

/// Top-level ingest for the interactive `query` subcommand.
/// All → FS scan first, then merge Plex records in by path (dedup + enrich).
/// Plex → Plex-only (fast; includes items addressable from Plex regardless of FS).
/// Fs → FS-only from configured roots (no Plex API call).
pub(crate) fn ingest_catalog(catalog: Catalog) -> Result<Vec<normalize::NormalizedItem>, Error> {
    match catalog {
        Catalog::Fs => Ok(fs_catalog::ingest_all_roots()?.into_values().collect()),
        Catalog::Plex => load_plex_cached(),
        Catalog::All => {
            // FS scan first — each item keyed by canonical path.
            let mut by_path = fs_catalog::ingest_all_roots()?;

            // Plex scan — merge into FS records by path, or add as Plex-only.
            let plex_items = load_plex_cached()?;
            let mut extras: Vec<normalize::NormalizedItem> = Vec::new();
            for mut plex_item in plex_items {
                if let Some(fs_item) = by_path.get_mut(&plex_item.path) {
                    // Merge: Plex enriches the FS record.
                    merge_plex_into_fs(fs_item, &plex_item);
                } else {
                    // Plex-only (file not in any configured FS root).
                    plex_item.sources = vec!["plex".into()];
                    extras.push(plex_item);
                }
            }

            let mut items: Vec<_> = by_path.into_values().collect();
            items.extend(extras);
            Ok(items)
        }
    }
}

fn load_plex_cached() -> Result<Vec<normalize::NormalizedItem>, Error> {
    if let Some(cached) = cache::load("plex-all", None) {
        eprintln!("(cached: {} plex items)", cached.len());
        return Ok(cached);
    }
    let fresh = plex::resolve("")?;
    if let Err(e) = cache::store("plex-all", &fresh) {
        eprintln!("(cache store failed: {e})");
    }
    Ok(fresh)
}

/// Merge Plex metadata into an FS-sourced item in place.
/// FS keeps its path. Plex contributes title, year, season, episode, etc.
/// sources becomes ["plex", "fs"] — Plex listed first as primary/richest.
fn merge_plex_into_fs(fs: &mut normalize::NormalizedItem, plex: &normalize::NormalizedItem) {
    fs.sources = vec!["plex".into(), "fs".into()];
    // Prefer Plex type when it's more specific than "video".
    if fs.media_type == "video" {
        fs.media_type.clone_from(&plex.media_type);
    }
    // Enrich from Plex where FS has no data.
    if fs.title.is_empty() || fs.title == fs.path {
        fs.title.clone_from(&plex.title);
    }
    if fs.sub_title.is_none() {
        fs.sub_title.clone_from(&plex.sub_title);
    }
    if fs.season.is_none() {
        fs.season = plex.season;
    }
    if fs.episode.is_none() {
        fs.episode = plex.episode;
    }
    if fs.year.is_none() {
        fs.year = plex.year;
    }
    if fs.categories.is_empty() {
        fs.categories.clone_from(&plex.categories);
    }
    if fs.collections.is_empty() {
        fs.collections.clone_from(&plex.collections);
    }
    if fs.content_rating.is_none() {
        fs.content_rating.clone_from(&plex.content_rating);
    }
    if fs.runtime_secs.is_none() {
        fs.runtime_secs = plex.runtime_secs;
    }
    fs.rating_key.clone_from(&plex.rating_key);
}

/// Ingest for case files — supports named plex targets and explicit fs paths.
pub(crate) fn ingest(source: &SourceSpec) -> Result<Vec<normalize::NormalizedItem>, Error> {
    match source {
        SourceSpec::Plex(value) => Ok(plex::resolve(value)?),
        SourceSpec::Fs(path) => Ok(fs_catalog::ingest(path)?),
        SourceSpec::Auto(value) if value.is_empty() => ingest_catalog(Catalog::All),
        SourceSpec::Auto(value) => Ok(plex::resolve(value)?),
    }
}

#[derive(Debug, Clone)]
pub(crate) enum SourceSpec {
    Auto(String),
    Plex(String),
    Fs(PathBuf),
}

impl SourceSpec {
    pub(crate) fn parse(spec: &str) -> Self {
        if let Some(rest) = spec.strip_prefix("plex:") {
            return Self::Plex(rest.to_string());
        }
        if let Some(rest) = spec.strip_prefix("fs:") {
            return Self::Fs(PathBuf::from(rest));
        }
        Self::Auto(spec.to_string())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("missing required argument: {0}")]
    MissingArg(&'static str),
    #[error("plex: {0}")]
    Plex(#[from] plex::PlexError),
    #[error("fs catalog: {0}")]
    Fs(#[from] fs_catalog::FsError),
    #[error("cel: {0}")]
    Cel(#[from] cel_eval::CelError),
    #[error("cases: {0}")]
    Cases(#[from] cases::CaseError),
}
