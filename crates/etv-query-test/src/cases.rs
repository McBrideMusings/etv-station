use std::path::Path;
use std::time::Instant;

use serde::Deserialize;
use thiserror::Error;

use crate::OutputFormat;
use crate::ingest;
use crate::normalize::{self, NormalizedItem};
use crate::output;

#[derive(Debug, Error)]
pub enum CaseError {
    #[error("io: {0}")]
    Io(String),
    #[error("parse {0}: {1}")]
    Parse(String, String),
}

#[derive(Debug, Deserialize)]
struct CaseFile {
    name: String,
    description: Option<String>,
    source: String,
    query: String,
    order: Option<String>,
    limit: Option<usize>,
}

pub fn run_batch(cases_dir: &Path) -> Result<(), crate::Error> {
    let mut entries: Vec<_> = std::fs::read_dir(cases_dir)
        .map_err(|e| CaseError::Io(e.to_string()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "toml").unwrap_or(false))
        .collect();
    entries.sort();

    for path in entries {
        let raw = std::fs::read_to_string(&path).map_err(|e| CaseError::Io(e.to_string()))?;
        let case: CaseFile = toml::from_str(&raw)
            .map_err(|e| CaseError::Parse(path.display().to_string(), e.to_string()))?;
        run_case(&case)?;
    }
    Ok(())
}

fn run_case(case: &CaseFile) -> Result<(), crate::Error> {
    println!();
    println!("══ {} ══", case.name);
    if let Some(desc) = &case.description {
        println!("{desc}");
    }

    let source = crate::SourceSpec::parse(&case.source);
    let ingest_start = Instant::now();
    let items = match ingest(&source) {
        Ok(items) => items,
        Err(e) => {
            println!("(skipped: {e})");
            return Ok(());
        }
    };
    let ingest_elapsed = ingest_start.elapsed();

    let compile_start = Instant::now();
    let (program, effective_query) =
        crate::compile_with_freetext_fallback(&case.query, items.first());
    let compile_elapsed = compile_start.elapsed();
    let program = program?;

    let eval_start = Instant::now();
    let mut matched: Vec<NormalizedItem> = items
        .iter()
        .filter(|item| program.matches(item).unwrap_or(false))
        .cloned()
        .collect();
    let eval_elapsed = eval_start.elapsed();

    if let Some(keys) = case.order.as_deref() {
        normalize::sort_by_keys(&mut matched, keys);
    }
    if let Some(n) = case.limit {
        matched.truncate(n);
    }

    output::render(
        OutputFormat::Table,
        &output::RenderInput {
            query: &effective_query,
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
