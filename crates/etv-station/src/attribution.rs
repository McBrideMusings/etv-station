//! Who has been watching a given film or episode, as a line of English (#113).
//!
//! The station already pulls the server's recent watch history to feed a scorer
//! plugin (#74). Those rows carry who watched what, and until now that was
//! consumed to produce a float and thrown away. This turns the same rows into a
//! sentence a viewer can read — in the guide via `ProgramMetadata.description`,
//! and on screen via the overlay's program context.
//!
//! # What it can and cannot say
//!
//! The line is built from whatever [`crate::tautulli`] last fetched, which is
//! the most recent `HISTORY_ROWS` rows and has no time filter — Tautulli's
//! `get_history` has no "since" parameter. So the honest phrasing is
//! "recently", never "this week": the rows reach back as far as they reach.
//!
//! It is also only as current as the generation that wrote the chunk. A chunk
//! written now and aired in six hours carries six-hour-old attribution, and
//! there is no path from etv-next back to the station to refresh it in place.
//! A channel that wants it fresh wants a short `window_days` and a short
//! `chunk_hours`, the same way the For You sample already does for its ranking.
//!
//! # Names, not counts
//!
//! This server is private and its owner asked for real names, so the line names
//! people. On a shared server that is a privacy decision to revisit before
//! turning `attribution` on — every viewer of the channel sees who watched what.

use std::collections::HashMap;

use crate::score::WatchEvent;

/// How many names to print before collapsing the rest into "and N others".
///
/// Three keeps the line readable in a guide cell and on a lower third; a
/// channel whose whole audience watched something would otherwise produce a
/// twenty-name sentence that fits nowhere.
const MAX_NAMES: usize = 3;

/// Who watched each entry, newest watch first, deduplicated by person.
///
/// Built once per generation and consulted per item, because a channel lays
/// down hundreds of items against a thousand history rows and rescanning the
/// rows per item is the same quadratic scan #126 removed at the fetch layer.
#[derive(Debug, Default)]
pub struct Attribution {
    by_entry: HashMap<String, Vec<String>>,
}

impl Attribution {
    /// Index a generation's watch history by entry.
    ///
    /// A person who watched the same thing twice is named once — the line says
    /// who has been watching, not how many plays there were. Within an entry,
    /// names are ordered by that person's most recent watch, so the people who
    /// are watching it *now* are the ones who survive the `MAX_NAMES` cut.
    pub fn build(history: &[WatchEvent]) -> Self {
        // Most recent first, so the first time a name is seen for an entry is
        // that person's latest watch of it.
        let mut ordered: Vec<&WatchEvent> = history.iter().collect();
        ordered.sort_by(|a, b| b.watched_at.cmp(&a.watched_at));

        let mut by_entry: HashMap<String, Vec<String>> = HashMap::new();
        for event in ordered {
            let Some(name) = event.watcher.as_deref() else {
                continue;
            };
            let names = by_entry.entry(event.entry_id.clone()).or_default();
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        Self { by_entry }
    }

    /// The attribution line for one entry, or `None` when nobody on record has
    /// watched it.
    ///
    /// `None` is the common case on a big library and is why this is not a
    /// `String`: an item nobody has watched must get no line at all rather than
    /// "watched recently by nobody", and must leave the item's existing
    /// description untouched.
    pub fn line_for(&self, entry_id: &str) -> Option<String> {
        let names = self.by_entry.get(entry_id)?;
        match names.len() {
            0 => None,
            _ => Some(format!("Watched recently by {}", join_names(names))),
        }
    }

    /// How many entries have anyone against them — for the log line that tells
    /// an operator whether attribution is doing anything at all.
    pub fn len(&self) -> usize {
        self.by_entry.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_entry.is_empty()
    }
}

/// `["a"]` → `a`; `["a","b"]` → `a and b`; `["a","b","c"]` → `a, b and c`;
/// beyond [`MAX_NAMES`] → `a, b, c and 4 others`.
fn join_names(names: &[String]) -> String {
    if names.len() > MAX_NAMES {
        let shown = names[..MAX_NAMES].join(", ");
        let rest = names.len() - MAX_NAMES;
        let others = if rest == 1 {
            "1 other"
        } else {
            &format!("{rest} others")
        };
        return format!("{shown} and {others}");
    }
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [head @ .., last] => format!("{} and {}", head.join(", "), last),
    }
}

/// Append an attribution line to an existing description without destroying it.
///
/// A film out of the Plex catalog already has a synopsis in `description`, and
/// that synopsis is what a viewer opening the guide actually wants; the
/// attribution is an addition to it, never a replacement. Separated by a blank
/// line so the two do not read as one paragraph.
pub fn append_to_description(existing: Option<String>, line: &str) -> String {
    match existing {
        Some(text) if !text.trim().is_empty() => format!("{}\n\n{line}", text.trim_end()),
        _ => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(entry: &str, at: i64, who: Option<&str>) -> WatchEvent {
        WatchEvent {
            entry_id: entry.into(),
            watched_at: at,
            watcher: who.map(str::to_string),
        }
    }

    #[test]
    fn names_one_watcher() {
        let a = Attribution::build(&[watch("m1", 100, Some("Madi"))]);
        assert_eq!(a.line_for("m1").unwrap(), "Watched recently by Madi");
    }

    #[test]
    fn joins_two_with_and_and_three_with_a_comma() {
        let two = Attribution::build(&[
            watch("m1", 200, Some("Madi")),
            watch("m1", 100, Some("Pierce")),
        ]);
        assert_eq!(
            two.line_for("m1").unwrap(),
            "Watched recently by Madi and Pierce"
        );

        let three = Attribution::build(&[
            watch("m1", 300, Some("Madi")),
            watch("m1", 200, Some("Pierce")),
            watch("m1", 100, Some("Abby")),
        ]);
        assert_eq!(
            three.line_for("m1").unwrap(),
            "Watched recently by Madi, Pierce and Abby",
        );
    }

    /// A film the whole house watched must not produce a twenty-name sentence.
    #[test]
    fn collapses_a_long_list_into_and_n_others() {
        let events: Vec<WatchEvent> = ["Madi", "Pierce", "Abby", "Zions", "emma"]
            .iter()
            .enumerate()
            .map(|(i, n)| watch("m1", 500 - i as i64, Some(n)))
            .collect();
        assert_eq!(
            Attribution::build(&events).line_for("m1").unwrap(),
            "Watched recently by Madi, Pierce, Abby and 2 others",
        );
    }

    /// "and 1 other", not "and 1 others".
    #[test]
    fn a_single_leftover_is_singular() {
        let events: Vec<WatchEvent> = ["Madi", "Pierce", "Abby", "Zions"]
            .iter()
            .enumerate()
            .map(|(i, n)| watch("m1", 500 - i as i64, Some(n)))
            .collect();
        assert_eq!(
            Attribution::build(&events).line_for("m1").unwrap(),
            "Watched recently by Madi, Pierce, Abby and 1 other",
        );
    }

    /// Rewatching is not two people. The line says who has been watching, so a
    /// person who played something three times is still one name.
    #[test]
    fn a_rewatch_names_one_person_once() {
        let a = Attribution::build(&[
            watch("m1", 300, Some("Madi")),
            watch("m1", 200, Some("Madi")),
            watch("m1", 100, Some("Madi")),
        ]);
        assert_eq!(a.line_for("m1").unwrap(), "Watched recently by Madi");
    }

    /// The `MAX_NAMES` cut keeps whoever is watching it now, not whoever
    /// happened to sit earliest in the row order.
    #[test]
    fn the_most_recent_watchers_survive_the_cut() {
        let a = Attribution::build(&[
            watch("m1", 100, Some("Oldest")),
            watch("m1", 900, Some("Newest")),
            watch("m1", 800, Some("Second")),
            watch("m1", 700, Some("Third")),
        ]);
        assert_eq!(
            a.line_for("m1").unwrap(),
            "Watched recently by Newest, Second, Third and 1 other",
        );
    }

    /// Nobody has watched it → no line at all, so the item's own description
    /// survives untouched.
    #[test]
    fn an_unwatched_entry_gets_no_line() {
        let a = Attribution::build(&[watch("m1", 100, Some("Madi"))]);
        assert!(a.line_for("m2").is_none());
    }

    /// An anonymised row names nobody and must not become an empty credit.
    #[test]
    fn a_row_with_no_name_is_not_an_attribution() {
        let a = Attribution::build(&[watch("m1", 100, None)]);
        assert!(a.line_for("m1").is_none(), "a nameless row credits nobody");
        assert!(a.is_empty());
    }

    #[test]
    fn appending_keeps_the_existing_synopsis() {
        let got = append_to_description(
            Some("A hobbit sets out.".into()),
            "Watched recently by Madi",
        );
        assert_eq!(got, "A hobbit sets out.\n\nWatched recently by Madi");
    }

    #[test]
    fn appending_to_nothing_is_just_the_line() {
        assert_eq!(
            append_to_description(None, "Watched recently by Madi"),
            "Watched recently by Madi",
        );
        assert_eq!(
            append_to_description(Some("   ".into()), "Watched recently by Madi"),
            "Watched recently by Madi",
        );
    }
}
