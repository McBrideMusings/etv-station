//! `plexdb-read` — the reader crate's accessors as a command line: one
//! subcommand per accessor, JSON on stdout, a message on stderr and a non-zero
//! exit on any error.
//!
//! This is the one search implementation the Python side (`plexdb search`) and
//! the Plex DB-EX Search tab call, so they cannot disagree with a Rust consumer.
//!
//! Where the files are comes from a flag, else the environment:
//!
//! | flag | environment |
//! |---|---|
//! | `--snapshot` | `PLEXDB_SNAPSHOT_PATH` |
//! | `--frames` | `PLEXDB_FRAMES_DIR` (the file is `frames.db` inside it) |
//! | `--phrase-cache` | `PLEXDB_PHRASE_CACHE` |
//! | `--encoder-url` | `PLEXDB_FRAME_ENCODER_URL` |
//! | `--encoder-model` | `PLEXDB_FRAME_MODEL` |

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use plexdb_reader::{PhraseCache, Reader, Search, Url};
use serde::Serialize;

const USAGE: &str = "usage: plexdb-read [--snapshot P] [--frames P] [--phrase-cache P] \
[--encoder-url U] [--encoder-model M] <subcommand> [args]

subcommands:
  enrichment-for <item_id> <namespace>
  enrichment-for-many <namespace> <item_id>...
  keyword-for-surface <surface>
  surfaces-for-keyword <keyword>
  display-name <keyword>
  edges-from <item_id> <edge_type>
  edges-to <item_id> <edge_type>
  collections-for <item_id>
  taste-vector-for <account_id>
  watched-units-for <account_id>
  watched-units
  pooled-taste-vector
  cuts <item_id>
  chapters <item_id>
  capabilities
  search <phrase> (--pool <item_id> [--pool ...] | --pool-file <path|->)
         [--not <phrase> [--not ...]] [--weight w] [--temperature T] [--draw n --seed s]

--pool and --not take one value each and repeat: --pool a --pool b --not x --not y.
Flags may come before or after the phrase.
--help prints this and exits 0.";

enum Failure {
    Help,
    Usage(String),
    Run(String),
}

impl<E: std::fmt::Display> From<E> for Failure {
    fn from(err: E) -> Self {
        Failure::Run(err.to_string())
    }
}

#[derive(Default)]
struct Args {
    positional: Vec<String>,
    flags: BTreeMap<String, Vec<String>>,
}

impl Args {
    fn parse(raw: Vec<String>) -> Result<Self, Failure> {
        let mut args = Args::default();
        let mut tokens = raw.into_iter().peekable();
        while let Some(token) = tokens.next() {
            if token == "--help" {
                return Err(Failure::Help);
            }
            if !token.starts_with("--") {
                args.positional.push(token);
                continue;
            }
            // Every flag takes exactly one value, whatever it starts with, so a
            // repeated flag builds a list and a negative may begin with `--`.
            let value = tokens
                .next()
                .ok_or_else(|| Failure::Usage(format!("{token} needs a value")))?;
            args.flags.entry(token).or_default().push(value);
        }
        Ok(args)
    }

    fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }

    fn list(&self, name: &str) -> Vec<&str> {
        self.flags
            .get(name)
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    fn number<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, Failure> {
        self.flag(name)
            .map(|v| {
                v.parse::<T>()
                    .map_err(|_| Failure::Usage(format!("{name} is not a number: {v}")))
            })
            .transpose()
    }

    /// The `n`th positional, named for the usage error.
    fn arg(&self, n: usize, name: &str) -> Result<&str, Failure> {
        self.positional
            .get(n)
            .map(String::as_str)
            .ok_or_else(|| Failure::Usage(format!("missing <{name}>")))
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn open(args: &Args) -> Result<Reader, Failure> {
    let snapshot = args
        .flag("--snapshot")
        .map(str::to_string)
        .or_else(|| env("PLEXDB_SNAPSHOT_PATH"))
        .ok_or_else(|| {
            Failure::Usage("no snapshot: pass --snapshot or set PLEXDB_SNAPSHOT_PATH".into())
        })?;
    let frames = args
        .flag("--frames")
        .map(PathBuf::from)
        .or_else(|| env("PLEXDB_FRAMES_DIR").map(|d| PathBuf::from(d).join("frames.db")));
    let cache = args
        .flag("--phrase-cache")
        .map(PathBuf::from)
        .or_else(|| env("PLEXDB_PHRASE_CACHE").map(PathBuf::from));
    let encoder = args
        .flag("--encoder-url")
        .map(str::to_string)
        .or_else(|| env("PLEXDB_FRAME_ENCODER_URL"));
    let model = args
        .flag("--encoder-model")
        .map(str::to_string)
        .or_else(|| env("PLEXDB_FRAME_MODEL"));
    let phrases = match (cache, encoder) {
        (Some(path), Some(url)) => Some(PhraseCache {
            path,
            encoder_url: Url::parse(&url)
                .map_err(|e| Failure::Usage(format!("encoder URL {url}: {e}")))?,
            request_model: model.unwrap_or_default(),
        }),
        _ => None,
    };
    Ok(Reader::open(snapshot, frames.as_deref(), phrases)?)
}

/// Serialised straight to text: going through `Value` would widen every `f32`
/// to an `f64` and print digits the similarity never had.
fn json<T: Serialize>(value: &T) -> Result<String, Failure> {
    Ok(serde_json::to_string(value)?)
}

fn pool_ids(args: &Args) -> Result<Vec<String>, Failure> {
    let mut ids: Vec<String> = args
        .list("--pool")
        .into_iter()
        .map(str::to_string)
        .collect();
    if let Some(path) = args.flag("--pool-file") {
        let mut text = String::new();
        if path == "-" {
            std::io::stdin().read_to_string(&mut text)?;
        } else {
            text = std::fs::read_to_string(path)?;
        }
        ids.extend(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string),
        );
    }
    Ok(ids)
}

fn run(raw: Vec<String>) -> Result<String, Failure> {
    let args = Args::parse(raw)?;
    let command = args.arg(0, "subcommand")?.to_string();
    let reader = open(&args)?;
    match command.as_str() {
        "enrichment-for" => {
            json(&reader.enrichment_for(args.arg(1, "item_id")?, args.arg(2, "namespace")?)?)
        }
        "enrichment-for-many" => {
            let ids = args.positional[2.min(args.positional.len())..]
                .iter()
                .map(String::as_str);
            json(&reader.enrichment_for_many(ids, args.arg(1, "namespace")?)?)
        }
        "keyword-for-surface" => json(&reader.keyword_for_surface(args.arg(1, "surface")?)?),
        "surfaces-for-keyword" => json(&reader.surfaces_for_keyword(args.arg(1, "keyword")?)?),
        "display-name" => json(&reader.display_name(args.arg(1, "keyword")?)?),
        "edges-from" => {
            json(&reader.edges_from(args.arg(1, "item_id")?, args.arg(2, "edge_type")?)?)
        }
        "edges-to" => json(&reader.edges_to(args.arg(1, "item_id")?, args.arg(2, "edge_type")?)?),
        "collections-for" => json(&reader.collections_for(args.arg(1, "item_id")?)?),
        "taste-vector-for" => json(&reader.taste_vector_for(account(&args)?)?),
        "watched-units-for" => json(&reader.watched_units_for(account(&args)?)?),
        "watched-units" => json(&reader.watched_units()?),
        "pooled-taste-vector" => json(&reader.pooled_taste_vector()?),
        "cuts" => json(&reader.cuts(args.arg(1, "item_id")?)?),
        "chapters" => json(&reader.chapters(args.arg(1, "item_id")?)?),
        "capabilities" => json(&reader.capabilities()),
        "search" => search(&reader, &args),
        other => Err(Failure::Usage(format!("unknown subcommand {other}"))),
    }
}

fn account(args: &Args) -> Result<i64, Failure> {
    let raw = args.arg(1, "account_id")?;
    raw.parse()
        .map_err(|_| Failure::Usage(format!("<account_id> is not a number: {raw}")))
}

fn search(reader: &Reader, args: &Args) -> Result<String, Failure> {
    if let Some(extra) = args.positional.get(2) {
        return Err(Failure::Usage(format!(
            "unexpected argument {extra}: --pool and --not take one value each, repeat the flag"
        )));
    }
    let pool = pool_ids(args)?;
    let pool: Vec<&str> = pool.iter().map(String::as_str).collect();
    let q = Search {
        pool: &pool,
        phrase: args.arg(1, "phrase")?,
        negatives: &args.list("--not"),
        weight: args.number("--weight")?,
        temperature: args.number("--temperature")?,
    };
    let mut found = reader.search_shots(&q)?;
    match (
        args.number::<usize>("--draw")?,
        args.number::<u64>("--seed")?,
    ) {
        (Some(n), seed) => found.shots = reader.draw(&found, n, seed.unwrap_or(0)),
        (None, Some(_)) => return Err(Failure::Usage("--seed needs --draw".into())),
        (None, None) => {}
    }
    json(&found)
}

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(Failure::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Err(Failure::Usage(message)) => {
            eprintln!("error: {message}\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Run(message)) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}
