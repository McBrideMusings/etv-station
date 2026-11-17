//! The renderer mixes and cuts what a plan lists and decides nothing about how
//! music is arranged (ADR 0023). This fails when the renderer's source names an
//! arrangement concept — a transition, a recipe, a seam, a hook, a song's
//! structure — in code or in a comment, since either one is the first step to a
//! mixing choice living in Rust instead of a plugin.

use std::fs;
use std::path::{Path, PathBuf};

/// Words that name an arrangement, each matched as a whole word of an
/// identifier or comment (`vocal_over_bars` is the words `vocal`, `over`,
/// `bars`), ignoring case.
const WORDS: &[&str] = &[
    "transition",
    "transitions",
    "recipe",
    "recipes",
    "seam",
    "seams",
    "hook",
    "hooks",
    "chorus",
    "verse",
    "landing",
    "bpm",
    "camelot",
    "megamix",
    "chapter",
    "chapters",
    "song",
    "songs",
    "bar",
    "bars",
    "beat",
    "beats",
];

/// Arrangements named by two words in a row.
const PHRASES: &[[&str; 2]] = &[
    ["vocal", "over"],
    ["vocals", "over"],
    ["bass", "swap"],
    ["echo", "out"],
    ["drop", "mix"],
];

/// The words of `line`, split at anything not a letter or digit and at a
/// lower-to-upper case change (`BassSwap` is `bass`, `swap`).
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for c in line.chars() {
        let boundary = !c.is_ascii_alphanumeric() || (c.is_ascii_uppercase() && prev_lower);
        if boundary && !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
        if c.is_ascii_alphanumeric() {
            word.push(c.to_ascii_lowercase());
        }
        prev_lower = c.is_ascii_lowercase();
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// Every arrangement word or phrase in `line`.
fn findings(line: &str) -> Vec<String> {
    let ws = words(line);
    let mut found: Vec<String> = ws
        .iter()
        .filter(|w| WORDS.contains(&w.as_str()))
        .cloned()
        .collect();
    for pair in ws.windows(2) {
        if PHRASES.iter().any(|p| p[0] == pair[0] && p[1] == pair[1]) {
            found.push(format!("{} {}", pair[0], pair[1]));
        }
    }
    found
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_renderer_source_names_no_arrangement() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    files.sort();
    assert!(!files.is_empty(), "no Rust files under {}", src.display());
    let mut hits = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let found = findings(line);
            if !found.is_empty() {
                hits.push(format!(
                    "{}:{}: {} — {}",
                    file.display(),
                    n + 1,
                    found.join(", "),
                    line.trim()
                ));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "the renderer names an arrangement concept; that choice belongs in the plugin that writes the plan (ADR 0023):\n{}",
        hits.join("\n")
    );
}

#[test]
fn the_matcher_finds_words_inside_identifiers_and_phrases_across_them() {
    assert_eq!(
        findings("pub vocal_over_bars: u32,"),
        ["bars", "vocal over"]
    );
    assert_eq!(findings("Recipe::BassSwap"), ["recipe", "bass swap"]);
    assert_eq!(findings("fn bass_swap()"), ["bass swap"]);
    assert_eq!(findings("let hooks = plan.hooks;"), ["hooks", "hooks"]);
    assert!(findings("Filter::Echo { in_gain, out_gain }").is_empty());
    assert!(findings("a tempo ride steps once per step_src_s").is_empty());
}
