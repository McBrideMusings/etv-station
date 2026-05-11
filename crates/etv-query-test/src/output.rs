use std::time::Duration;

use serde::Serialize;

use crate::OutputFormat;
use crate::normalize::NormalizedItem;

pub struct RenderInput<'a> {
    pub query: &'a str,
    pub candidate_count: usize,
    pub matched_count: usize,
    pub ingest_elapsed: Duration,
    pub compile_elapsed: Duration,
    pub eval_elapsed: Duration,
    pub helpers_used: Vec<&'static str>,
    pub items: &'a [NormalizedItem],
}

pub fn render(format: OutputFormat, input: &RenderInput<'_>) {
    match format {
        OutputFormat::Table => render_table(input),
        OutputFormat::Json => render_json(input),
    }
}

fn render_table(input: &RenderInput<'_>) {
    println!("{}", "─".repeat(80));
    println!("query    {}", indent_continuation(input.query, 9));
    println!(
        "stats    {} matched / {} candidates",
        input.matched_count, input.candidate_count
    );
    println!(
        "timing   ingest {} / compile {} / eval {}",
        fmt_duration(input.ingest_elapsed),
        fmt_duration(input.compile_elapsed),
        fmt_duration(input.eval_elapsed),
    );
    if !input.helpers_used.is_empty() {
        println!("helpers  {}", input.helpers_used.join(", "));
    }
    println!("{}", "─".repeat(80));

    if input.items.is_empty() {
        println!("(no items)");
        return;
    }

    let cols = compute_columns(input.items);
    print_row(&cols, "title", "se", "year", "runtime", "library", "path");
    println!("{}", "─".repeat(80));
    for item in input.items {
        let se = match (item.season, item.episode) {
            (Some(s), Some(e)) => format!("S{s:02}E{e:02}"),
            _ => String::new(),
        };
        let year = item.year.map(|y| y.to_string()).unwrap_or_default();
        let runtime = item.runtime_secs.map(fmt_runtime).unwrap_or_default();
        print_row(
            &cols,
            &item.title,
            &se,
            &year,
            &runtime,
            &item.library,
            &item.path,
        );
    }
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    query: &'a str,
    candidate_count: usize,
    matched_count: usize,
    ingest_ms: u128,
    compile_ms: u128,
    eval_ms: u128,
    helpers_used: &'a [&'static str],
    items: &'a [NormalizedItem],
}

fn render_json(input: &RenderInput<'_>) {
    let out = JsonOutput {
        query: input.query,
        candidate_count: input.candidate_count,
        matched_count: input.matched_count,
        ingest_ms: input.ingest_elapsed.as_millis(),
        compile_ms: input.compile_elapsed.as_millis(),
        eval_ms: input.eval_elapsed.as_millis(),
        helpers_used: &input.helpers_used,
        items: input.items,
    };
    match serde_json::to_string_pretty(&out) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: {e}"),
    }
}

struct Cols {
    title: usize,
    se: usize,
    year: usize,
    runtime: usize,
    library: usize,
    path: usize,
}

fn compute_columns(items: &[NormalizedItem]) -> Cols {
    Cols {
        title: items
            .iter()
            .map(|i| i.title.len())
            .max()
            .unwrap_or(5)
            .clamp(5, 40),
        se: 6,
        year: 4,
        runtime: 8,
        library: items
            .iter()
            .map(|i| i.library.len())
            .max()
            .unwrap_or(7)
            .clamp(7, 20),
        path: 30,
    }
}

fn print_row(cols: &Cols, a: &str, b: &str, c: &str, d: &str, e: &str, f: &str) {
    println!(
        "{:<wt$}  {:<ws$}  {:<wy$}  {:<wr$}  {:<wl$}  {}",
        truncate(a, cols.title),
        truncate(b, cols.se),
        truncate(c, cols.year),
        truncate(d, cols.runtime),
        truncate(e, cols.library),
        truncate(f, cols.path),
        wt = cols.title,
        ws = cols.se,
        wy = cols.year,
        wr = cols.runtime,
        wl = cols.library,
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

fn fmt_runtime(secs: f64) -> String {
    let total = secs.round() as i64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

fn indent_continuation(text: &str, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let sep = format!("\n{pad}");
    text.lines().collect::<Vec<_>>().join(&sep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("abc", 5), "abc");
    }

    #[test]
    fn truncate_long() {
        assert_eq!(truncate("abcdefgh", 4), "abc…");
    }

    #[test]
    fn fmt_runtime_formats() {
        assert_eq!(fmt_runtime(45.0), "45s");
        assert_eq!(fmt_runtime(125.0), "2m05s");
        assert_eq!(fmt_runtime(3700.0), "1h01m");
    }
}
