//! `suggest-category` (#175): from a set of example titles, rank the catalog
//! tags those titles share, for a human to review and freeze into a named
//! category (#170).
//!
//! The ranking reads only enrichment the catalog already holds, so the same
//! seeds against the same catalog snapshot give the same output. It is a
//! suggestion tool: a query built from the top candidates alone recovers most of
//! a hand-curated set at low precision, and the output says so.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::catalog::{Catalog, CatalogError, TagNs};

/// The tag namespaces ranked, in the order the output lists them.
pub const NAMESPACES: [TagNs; 6] = [
    TagNs::Country,
    TagNs::Genre,
    TagNs::Director,
    TagNs::Cast,
    TagNs::Writer,
    TagNs::Label,
];

#[derive(Debug, thiserror::Error)]
pub enum SuggestError {
    #[error("seed {0:?} matches no catalog item")]
    UnknownSeed(String),
    #[error(
        "seed {title:?} matches {n} catalog items ({ids}); write it as \"Title (Year)\" to pick one"
    )]
    AmbiguousSeed {
        title: String,
        n: usize,
        ids: String,
    },
    #[error("counter-example {0:?} matches no catalog item")]
    UnknownCounter(String),
    #[error(
        "counter-example {0:?} matches more than one catalog item; write it as \"Title (Year)\" to pick one"
    )]
    AmbiguousCounter(String),
    #[error("{0:?} is in both the seeds and the counter-examples")]
    InBothLists(String),
    #[error(
        "need at least 2 seeds with enrichment — shared tags need something to be shared across \
         (got {0})"
    )]
    TooFewSeeds(usize),
    #[error("the {0} seeds share no tag; they may not be one group — infer each group separately")]
    NoSharedTags(usize),
    #[error(
        "all {0} shared tags are also carried by every counter-example; nothing is left to suggest"
    )]
    AllSharedTagsDropped(usize),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
}

/// One ranked tag.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub value: String,
    /// Seeds carrying the tag.
    pub seeds: usize,
    /// Counter-examples carrying the tag.
    pub counters: usize,
    /// Non-episode catalog entries carrying the tag, in the same namespace.
    pub corpus: i64,
    /// `ln(1 / corpus frequency)` weighted by the share of seeds carrying it and
    /// by the share of counter-examples that do not.
    pub score: f64,
}

/// A seed and what resolving it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSeed {
    pub title: String,
    pub entry_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    /// Distinct catalog items the seeds resolved to.
    pub resolved: usize,
    /// Seeds that carry at least one tag in a ranked namespace.
    pub enriched: usize,
    /// Seeds with no tag in any ranked namespace: reported, not ranked.
    pub unenriched: Vec<String>,
    pub corpus_size: i64,
    /// Distinct catalog items the counter-examples resolved to.
    pub counters_resolved: usize,
    /// Counter-examples that carry at least one tag in a ranked namespace.
    pub counters_enriched: usize,
    /// Counter-examples with no tag in any ranked namespace: reported, not counted.
    pub counters_unenriched: Vec<String>,
    /// Tags shared by the seeds that every enriched counter-example also carries,
    /// in [`NAMESPACES`] order: what the two groups have in common.
    pub dropped: Vec<(TagNs, Candidate)>,
    /// Candidates per namespace, ranked, in [`NAMESPACES`] order.
    pub groups: Vec<(TagNs, Vec<Candidate>)>,
}

/// Resolve `title` or `Title (Year)` to exactly one non-episode entry.
fn resolve_seed(catalog: &Catalog, spec: &str) -> Result<ResolvedSeed, SuggestError> {
    let spec = spec.trim();
    let year_form = spec
        .strip_suffix(')')
        .and_then(|rest| rest.rsplit_once(" ("))
        .and_then(|(t, y)| Some((t.trim(), y.parse::<i64>().ok()?)));
    let ids = match year_form {
        Some((title, year)) => catalog.entry_ids_by_title_year(title, year)?,
        None => catalog.entry_ids_by_title(spec)?,
    };
    match ids.as_slice() {
        [] => Err(SuggestError::UnknownSeed(spec.to_string())),
        [one] => Ok(ResolvedSeed {
            title: spec.to_string(),
            entry_id: one.clone(),
        }),
        many => Err(SuggestError::AmbiguousSeed {
            title: spec.to_string(),
            n: many.len(),
            ids: many.join(", "),
        }),
    }
}

/// Rank the tags `seeds` share, lowering a tag's score by the share of
/// `counters` that carry it. `top` caps each namespace's list.
pub fn suggest(
    catalog: &Catalog,
    seeds: &[String],
    counters: &[String],
    top: usize,
) -> Result<Suggestion, SuggestError> {
    let mut resolved: Vec<ResolvedSeed> = Vec::new();
    for spec in seeds {
        let seed = resolve_seed(catalog, spec)?;
        if !resolved.iter().any(|r| r.entry_id == seed.entry_id) {
            resolved.push(seed);
        }
    }

    let mut resolved_counters: Vec<ResolvedSeed> = Vec::new();
    for spec in counters {
        let counter = resolve_seed(catalog, spec).map_err(|e| match e {
            SuggestError::UnknownSeed(t) => SuggestError::UnknownCounter(t),
            SuggestError::AmbiguousSeed { title, .. } => SuggestError::AmbiguousCounter(title),
            other => other,
        })?;
        if resolved.iter().any(|r| r.entry_id == counter.entry_id) {
            return Err(SuggestError::InBothLists(counter.title));
        }
        if !resolved_counters
            .iter()
            .any(|r| r.entry_id == counter.entry_id)
        {
            resolved_counters.push(counter);
        }
    }

    // Per namespace: lowercased value -> (first-seen spelling, seeds carrying it).
    let mut counts: BTreeMap<&'static str, BTreeMap<String, (String, usize)>> = BTreeMap::new();
    let mut enriched = 0usize;
    let mut unenriched = Vec::new();
    for seed in &resolved {
        let mut any = false;
        for ns in NAMESPACES {
            let mut values = catalog.tags_for(&seed.entry_id, ns)?;
            values.sort_by_key(|v| v.to_ascii_lowercase());
            values.dedup_by_key(|v| v.to_ascii_lowercase());
            for v in values {
                any = true;
                let slot = counts
                    .entry(ns.as_str())
                    .or_default()
                    .entry(v.to_ascii_lowercase())
                    .or_insert((v, 0));
                slot.1 += 1;
            }
        }
        if any {
            enriched += 1;
        } else {
            unenriched.push(seed.title.clone());
        }
    }
    if enriched < 2 {
        return Err(SuggestError::TooFewSeeds(enriched));
    }

    // Counter-examples carrying each tag, keyed like `counts`.
    let mut counter_counts: BTreeMap<&'static str, BTreeMap<String, usize>> = BTreeMap::new();
    let mut counters_enriched = 0usize;
    let mut counters_unenriched = Vec::new();
    for counter in &resolved_counters {
        let mut any = false;
        for ns in NAMESPACES {
            let mut values = catalog.tags_for(&counter.entry_id, ns)?;
            values.sort_by_key(|v| v.to_ascii_lowercase());
            values.dedup_by_key(|v| v.to_ascii_lowercase());
            for v in values {
                any = true;
                *counter_counts
                    .entry(ns.as_str())
                    .or_default()
                    .entry(v.to_ascii_lowercase())
                    .or_insert(0) += 1;
            }
        }
        if any {
            counters_enriched += 1;
        } else {
            counters_unenriched.push(counter.title.clone());
        }
    }

    let corpus_size = catalog.non_episode_count()?;
    let mut dropped = Vec::new();
    let mut groups = Vec::new();
    for ns in NAMESPACES {
        let mut cands = Vec::new();
        if let Some(values) = counts.get(ns.as_str()) {
            for (value, seed_count) in values.values() {
                if *seed_count < 2 {
                    continue;
                }
                let corpus = catalog.non_episode_count_with_tag(ns, value)?.max(1);
                let counter_count = counter_counts
                    .get(ns.as_str())
                    .and_then(|m| m.get(&value.to_ascii_lowercase()))
                    .copied()
                    .unwrap_or(0);
                let seed_freq = *seed_count as f64 / enriched as f64;
                let corpus_freq = corpus as f64 / corpus_size.max(1) as f64;
                let counter_freq = if counters_enriched == 0 {
                    0.0
                } else {
                    counter_count as f64 / counters_enriched as f64
                };
                let cand = Candidate {
                    value: value.clone(),
                    seeds: *seed_count,
                    counters: counter_count,
                    corpus,
                    score: seed_freq * (1.0 / corpus_freq).ln() * (1.0 - counter_freq),
                };
                if counters_enriched > 0 && counter_count == counters_enriched {
                    dropped.push((ns, cand));
                } else {
                    cands.push(cand);
                }
            }
        }
        cands.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(b.seeds.cmp(&a.seeds))
                .then_with(|| {
                    a.value
                        .to_ascii_lowercase()
                        .cmp(&b.value.to_ascii_lowercase())
                })
        });
        cands.truncate(top);
        groups.push((ns, cands));
    }

    if groups
        .iter()
        .all(|(_, c): &(TagNs, Vec<Candidate>)| c.is_empty())
    {
        if !dropped.is_empty() {
            return Err(SuggestError::AllSharedTagsDropped(dropped.len()));
        }
        return Err(SuggestError::NoSharedTags(enriched));
    }

    Ok(Suggestion {
        resolved: resolved.len(),
        enriched,
        unenriched,
        corpus_size,
        counters_resolved: resolved_counters.len(),
        counters_enriched,
        counters_unenriched,
        dropped,
        groups,
    })
}

impl Candidate {
    /// How many times likelier a seed is to carry the tag than a catalog entry.
    pub fn lift(&self, enriched: usize, corpus_size: i64) -> f64 {
        let seed_freq = self.seeds as f64 / enriched as f64;
        let corpus_freq = self.corpus as f64 / corpus_size.max(1) as f64;
        seed_freq / corpus_freq
    }
}

/// Render as a category file (#170's `categories:` shape) with each keyword's
/// numbers in a trailing comment, so freezing a reviewed set is a copy-paste.
pub fn render(s: &Suggestion, name: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# SUGGESTION ONLY — these candidates need a human to read and cut them. A query\n\
         # built from the top candidates alone recovered 73-77% of a hand-curated set at\n\
         # 14-15% precision (about six unwanted films per wanted one). Never use this\n\
         # unreviewed."
    );
    let _ = writeln!(
        out,
        "# Seeds: {} given, {} ranked, {} without enrichment. Corpus: {} non-episode entries.",
        s.resolved,
        s.enriched,
        s.unenriched.len(),
        s.corpus_size,
    );
    for title in &s.unenriched {
        let _ = writeln!(out, "# no enrichment, not ranked: {title}");
    }
    if s.counters_resolved > 0 {
        let _ = writeln!(
            out,
            "# Counter-examples: {} given, {} counted, {} without enrichment. A tag's score falls\n\
             # with the share of counted counter-examples carrying it; one every counted\n\
             # counter-example carries is dropped.",
            s.counters_resolved,
            s.counters_enriched,
            s.counters_unenriched.len(),
        );
        for title in &s.counters_unenriched {
            let _ = writeln!(out, "# no enrichment, not counted: {title}");
        }
        for (ns, c) in &s.dropped {
            let _ = writeln!(
                out,
                "# dropped, shared with every counter-example: {} ({}) · seeds {}/{} · counter {}/{}",
                json(&c.value),
                ns.as_str(),
                c.seeds,
                s.enriched,
                c.counters,
                s.counters_enriched,
            );
        }
    }
    let _ = writeln!(
        out,
        "# Each line: seeds carrying the tag / ranked seeds; entries carrying it / corpus (same\n\
         # namespace); lift = seed share / corpus share. Ranked by seed share x ln(1/corpus share).\n\
         # A tag carried by only one seed is not listed."
    );
    let keywords: usize = s.groups.iter().map(|(_, c)| c.len()).sum();
    let _ = writeln!(out, "categories:");
    let _ = writeln!(out, "  - name: {}", json(name));
    let _ = writeln!(
        out,
        "    threshold: {}  # REVIEW: set after cutting the keywords; a count of keywords an item must match",
        keywords.clamp(0, 2)
    );
    let _ = writeln!(out, "    keywords:");
    let mut written: Vec<String> = Vec::new();
    for (ns, cands) in &s.groups {
        if cands.is_empty() {
            continue;
        }
        let _ = writeln!(out, "      # {}", ns.as_str());
        for c in cands {
            let dup = written.contains(&c.value.to_ascii_lowercase());
            written.push(c.value.to_ascii_lowercase());
            if dup {
                continue;
            }
            let counter_note = if s.counters_enriched > 0 {
                format!(" · counter {}/{}", c.counters, s.counters_enriched)
            } else {
                String::new()
            };
            let _ = writeln!(
                out,
                "      - {}  # seeds {}/{} ({:.0}%) · corpus {}/{} ({:.2}%) · lift {:.1}x{}",
                json(&c.value),
                c.seeds,
                s.enriched,
                100.0 * c.seeds as f64 / s.enriched as f64,
                c.corpus,
                s.corpus_size,
                100.0 * c.corpus as f64 / s.corpus_size.max(1) as f64,
                c.lift(s.enriched, s.corpus_size),
                counter_note,
            );
        }
    }
    out
}

fn json(s: &str) -> String {
    serde_json::to_string(s).expect("a string always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Entry, Source};

    fn movie(c: &Catalog, id: &str, title: &str, year: i64, tags: &[(TagNs, &str)]) {
        let mut e = Entry::new(id, "movie", title, Source::Plex);
        e.year = Some(year);
        c.upsert_entry(&e).unwrap();
        for (ns, v) in tags {
            c.add_tag(id, *ns, v).unwrap();
        }
    }

    /// Four seeds out of a ten-entry corpus plus one episode that must not
    /// count toward the corpus.
    fn fixture() -> Catalog {
        let c = Catalog::open_in_memory().unwrap();
        let hk = (TagNs::Country, "Hong Kong");
        let usa = (TagNs::Country, "USA");
        movie(
            &c,
            "s1",
            "Alpha",
            1970,
            &[hk, (TagNs::Cast, "Jet Li"), (TagNs::Genre, "Action")],
        );
        movie(
            &c,
            "s2",
            "Beta",
            1971,
            &[hk, (TagNs::Cast, "Jet Li"), (TagNs::Genre, "Action")],
        );
        movie(&c, "s3", "Gamma", 1972, &[hk, (TagNs::Genre, "Action")]);
        movie(&c, "s4", "Delta", 1973, &[usa, (TagNs::Cast, "Solo Actor")]);
        movie(&c, "s5", "Bare", 1974, &[]);
        movie(&c, "o1", "Other1", 1980, &[usa, (TagNs::Genre, "Action")]);
        movie(&c, "o2", "Other2", 1981, &[usa, (TagNs::Genre, "Action")]);
        movie(&c, "o3", "Other3", 1982, &[usa, (TagNs::Genre, "Action")]);
        movie(&c, "o4", "Other4", 1983, &[usa, (TagNs::Genre, "Drama")]);
        movie(&c, "o5", "Other5", 1984, &[usa]);
        let mut ep = Entry::new("ep1", "episode", "Episode", Source::Plex);
        ep.year = Some(1999);
        c.upsert_entry(&ep).unwrap();
        c.add_tag("ep1", TagNs::Country, "Hong Kong").unwrap();
        c
    }

    fn seeds(titles: &[&str]) -> Vec<String> {
        titles.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn ranks_by_seed_share_and_corpus_rarity_and_pins_the_output() {
        let c = fixture();
        let s = suggest(
            &c,
            &seeds(&["Alpha", "Beta (1971)", "Gamma", "Delta"]),
            &[],
            10,
        )
        .unwrap();
        assert_eq!(s.enriched, 4);
        assert_eq!(s.corpus_size, 10);
        let out = render(&s, "hk");
        let expected = "\
# SUGGESTION ONLY — these candidates need a human to read and cut them. A query
# built from the top candidates alone recovered 73-77% of a hand-curated set at
# 14-15% precision (about six unwanted films per wanted one). Never use this
# unreviewed.
# Seeds: 4 given, 4 ranked, 0 without enrichment. Corpus: 10 non-episode entries.
# Each line: seeds carrying the tag / ranked seeds; entries carrying it / corpus (same
# namespace); lift = seed share / corpus share. Ranked by seed share x ln(1/corpus share).
# A tag carried by only one seed is not listed.
categories:
  - name: \"hk\"
    threshold: 2  # REVIEW: set after cutting the keywords; a count of keywords an item must match
    keywords:
      # country
      - \"Hong Kong\"  # seeds 3/4 (75%) · corpus 3/10 (30.00%) · lift 2.5x
      # genre
      - \"Action\"  # seeds 3/4 (75%) · corpus 6/10 (60.00%) · lift 1.2x
      # cast
      - \"Jet Li\"  # seeds 2/4 (50%) · corpus 2/10 (20.00%) · lift 2.5x
";
        assert_eq!(out, expected);
    }

    #[test]
    fn the_same_seeds_give_the_same_output() {
        let c = fixture();
        let a = render(
            &suggest(&c, &seeds(&["Alpha", "Beta", "Gamma"]), &[], 10).unwrap(),
            "x",
        );
        let b = render(
            &suggest(&c, &seeds(&["Gamma", "Beta", "Alpha"]), &[], 10).unwrap(),
            "x",
        );
        assert_eq!(a, b);
    }

    #[test]
    fn an_unknown_seed_fails_naming_it() {
        let c = fixture();
        let err = suggest(&c, &seeds(&["Alpha", "Nonesuch"]), &[], 10).unwrap_err();
        assert!(
            matches!(&err, SuggestError::UnknownSeed(t) if t == "Nonesuch"),
            "{err}"
        );
    }

    #[test]
    fn a_seed_without_enrichment_is_reported_not_ranked() {
        let c = fixture();
        let s = suggest(&c, &seeds(&["Alpha", "Beta", "Bare"]), &[], 10).unwrap();
        assert_eq!(s.enriched, 2);
        assert_eq!(s.unenriched, vec!["Bare".to_string()]);
        assert!(render(&s, "x").contains("# no enrichment, not ranked: Bare"));
    }

    #[test]
    fn seeds_sharing_no_tag_fail_instead_of_printing_an_unloadable_category() {
        let c = fixture();
        // Delta is USA/Solo Actor; Gamma is Hong Kong/Action: nothing in common.
        assert!(matches!(
            suggest(&c, &seeds(&["Gamma", "Delta"]), &[], 10),
            Err(SuggestError::NoSharedTags(2))
        ));
    }

    #[test]
    fn fewer_than_two_seeds_fails() {
        let c = fixture();
        assert!(matches!(
            suggest(&c, &seeds(&["Alpha"]), &[], 10),
            Err(SuggestError::TooFewSeeds(1))
        ));
        assert!(matches!(
            suggest(&c, &seeds(&["Alpha", "Bare"]), &[], 10),
            Err(SuggestError::TooFewSeeds(1))
        ));
        assert!(matches!(
            suggest(&c, &seeds(&["Alpha", "Alpha"]), &[], 10),
            Err(SuggestError::TooFewSeeds(1))
        ));
    }

    #[test]
    fn the_output_parses_as_a_category_file() {
        let c = fixture();
        let out = render(
            &suggest(&c, &seeds(&["Alpha", "Beta", "Gamma"]), &[], 10).unwrap(),
            "hk",
        );
        let file: crate::config::categories::CategoryFile = serde_norway::from_str(&out).unwrap();
        assert_eq!(file.categories.len(), 1);
        assert_eq!(
            file.categories[0].keywords,
            vec!["Hong Kong", "Action", "Jet Li"]
        );
    }

    #[test]
    fn a_tag_every_counter_example_carries_is_dropped_and_named() {
        let c = fixture();
        // Other1..3 are USA/Action; Action is also on all three seeds.
        let s = suggest(
            &c,
            &seeds(&["Alpha", "Beta", "Gamma"]),
            &seeds(&["Other1", "Other2"]),
            10,
        )
        .unwrap();
        assert_eq!(s.counters_enriched, 2);
        assert_eq!(s.dropped.len(), 1);
        assert_eq!(s.dropped[0].1.value, "Action");
        assert_eq!((s.dropped[0].1.seeds, s.dropped[0].1.counters), (3, 2));
        let out = render(&s, "hk");
        assert!(out.contains("# dropped, shared with every counter-example: \"Action\" (genre) · seeds 3/3 · counter 2/2"), "{out}");
        assert!(!out.contains("      - \"Action\""), "{out}");
        assert!(out.contains("      - \"Hong Kong\"  # seeds 3/3 (100%) · corpus 3/10 (30.00%) · lift 3.3x · counter 0/2"), "{out}");
    }

    #[test]
    fn a_tag_two_of_three_counter_examples_carry_scores_a_third_of_its_old_score() {
        let c = Catalog::open_in_memory().unwrap();
        for (id, title, tags) in [
            ("a", "A", vec!["Mid", "Clean"]),
            ("b", "B", vec!["Mid", "Clean"]),
            ("x", "X", vec!["Mid"]),
            ("y", "Y", vec!["Mid"]),
            ("z", "Z", vec!["Z"]),
        ] {
            let mut e = Entry::new(id, "movie", title, Source::Plex);
            e.year = Some(1970);
            c.upsert_entry(&e).unwrap();
            for t in tags {
                c.add_tag(id, TagNs::Genre, t).unwrap();
            }
        }
        let score = |counters: &[String], value: &str| {
            let s = suggest(&c, &seeds(&["A", "B"]), counters, 10).unwrap();
            s.groups
                .iter()
                .flat_map(|(_, c)| c)
                .find(|c| c.value == value)
                .map(|c| c.score)
                .unwrap()
        };
        let counters = seeds(&["X", "Y", "Z"]);
        let plain = score(&[], "Mid");
        assert!((score(&counters, "Mid") - plain / 3.0).abs() < 1e-9);
        assert!((score(&counters, "Clean") - score(&[], "Clean")).abs() < 1e-9);
        assert!(score(&counters, "Mid") < score(&counters, "Clean"));
    }

    #[test]
    fn a_title_in_both_lists_fails_naming_it() {
        let c = fixture();
        let err = suggest(
            &c,
            &seeds(&["Alpha", "Beta"]),
            &seeds(&["Alpha (1970)"]),
            10,
        )
        .unwrap_err();
        assert!(
            matches!(&err, SuggestError::InBothLists(t) if t == "Alpha (1970)"),
            "{err}"
        );
    }

    #[test]
    fn an_unknown_counter_example_fails_naming_it() {
        let c = fixture();
        let err = suggest(&c, &seeds(&["Alpha", "Beta"]), &seeds(&["Nonesuch"]), 10).unwrap_err();
        assert!(
            matches!(&err, SuggestError::UnknownCounter(t) if t == "Nonesuch"),
            "{err}"
        );
    }

    #[test]
    fn no_counter_examples_leave_the_output_unchanged() {
        let c = fixture();
        let out = render(
            &suggest(&c, &seeds(&["Alpha", "Beta", "Gamma"]), &[], 10).unwrap(),
            "x",
        );
        assert!(!out.contains("counter"), "{out}");
    }
}
