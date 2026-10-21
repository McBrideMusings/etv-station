//! Show arcs (#163): named absolute-episode ranges that cut one long season
//! into the pieces anyone actually watches it by (Black Clover is 170 episodes
//! in season 1; JoJo's season 1 holds two stories).
//!
//! One file per show, registered from the station's `arcs:` list. An arc is a
//! pseudo-season: `group_by: season` buckets an arc'd show by arc, and
//! `filter.seasons` accepts an arc's name beside season numbers. A show with no
//! file is untouched.
//!
//! The load checks everything it can without the catalog: each range parses,
//! arcs neither overlap nor leave a gap, and a show is declared once. A range
//! reaching past the show's last episode is fine — it just holds fewer
//! episodes — and episodes past the last arc form one trailing bucket, so a
//! newly ingested episode joins it rather than failing the load.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// An arcs file: one show and its arcs.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArcFile {
    /// The show's exact Plex title, matched against the catalog's `show`
    /// column the way a named show group's members are.
    pub show: String,
    pub arcs: Vec<ArcDecl>,
}

/// One arc as authored: a name and an absolute-episode range.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArcDecl {
    pub name: String,
    /// `"21-40"`, or a single episode `"7"`. Absolute numbering, inclusive.
    pub episodes: String,
}

/// One validated arc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArcRange {
    pub name: String,
    pub first: i64,
    pub last: i64,
}

impl ArcRange {
    fn range(&self) -> String {
        format!("{}-{}", self.first, self.last)
    }
}

/// Where an episode falls inside an arc'd show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArcBucket<'a> {
    /// Inside the arc called this.
    Named(&'a str),
    /// Past the last arc.
    Trailing,
}

/// One show's arcs, sorted by first episode and contiguous from episode 1.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ShowArcs {
    pub show: String,
    pub arcs: Vec<ArcRange>,
}

/// Every declared show's arcs, keyed by the show title.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Arcs {
    by_show: BTreeMap<String, ShowArcs>,
}

impl Arcs {
    /// Build from `(file, parsed)` pairs in load order. A bad range, an
    /// overlap, a gap, a repeated arc name, or two files for one show fails
    /// naming the show and what clashed.
    pub fn from_files(files: Vec<(PathBuf, ArcFile)>) -> Result<Self, String> {
        let mut by_show: BTreeMap<String, ShowArcs> = BTreeMap::new();
        let mut at: BTreeMap<String, PathBuf> = BTreeMap::new();
        for (path, file) in files {
            let here = path.display();
            if file.show.trim().is_empty() {
                return Err(format!("arcs in {here}: show is empty"));
            }
            if let Some(first) = at.get(&file.show) {
                return Err(format!(
                    "show {:?} has arcs declared twice: in {} and in {here}",
                    file.show,
                    first.display()
                ));
            }
            let show = parse_show(&file).map_err(|e| format!("arcs in {here}: {e}"))?;
            at.insert(file.show.clone(), path);
            by_show.insert(file.show, show);
        }
        Ok(Self { by_show })
    }

    pub fn is_empty(&self) -> bool {
        self.by_show.is_empty()
    }

    /// True when some show declares an arc called `name`.
    pub fn has_arc_named(&self, name: &str) -> bool {
        self.by_show
            .values()
            .any(|s| s.arcs.iter().any(|a| a.name == name))
    }

    /// Every distinct arc name, sorted — for an unknown-name error.
    pub fn arc_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .by_show
            .values()
            .flat_map(|s| s.arcs.iter().map(|a| a.name.as_str()))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Where absolute episode `absolute` of `show` falls, or `None` when the
    /// show declares no arcs.
    pub fn bucket(&self, show: &str, absolute: i64) -> Option<ArcBucket<'_>> {
        let arcs = &self.by_show.get(show)?.arcs;
        Some(
            arcs.iter()
                .find(|a| (a.first..=a.last).contains(&absolute))
                .map_or(ArcBucket::Trailing, |a| ArcBucket::Named(&a.name)),
        )
    }
}

fn parse_show(file: &ArcFile) -> Result<ShowArcs, String> {
    let show = &file.show;
    if file.arcs.is_empty() {
        return Err(format!("show {show:?}: arcs is empty"));
    }
    let mut arcs = Vec::with_capacity(file.arcs.len());
    for decl in &file.arcs {
        if decl.name.trim().is_empty() {
            return Err(format!("show {show:?}: an arc has an empty name"));
        }
        let (first, last) = parse_range(&decl.episodes).map_err(|e| {
            format!(
                "show {show:?}: arc {:?} episodes {:?} {e}",
                decl.name, decl.episodes
            )
        })?;
        arcs.push(ArcRange {
            name: decl.name.clone(),
            first,
            last,
        });
    }
    arcs.sort_by_key(|a| (a.first, a.last));
    for (i, a) in arcs.iter().enumerate() {
        if arcs[..i].iter().any(|b| b.name == a.name) {
            return Err(format!(
                "show {show:?}: arc name {:?} is used twice",
                a.name
            ));
        }
    }
    let mut next = 1;
    let mut prev: Option<&ArcRange> = None;
    for a in &arcs {
        if a.first < next {
            let p = prev.expect("an arc starts below 1 only after another arc");
            return Err(format!(
                "show {show:?}: arcs {:?} ({}) and {:?} ({}) overlap",
                p.name,
                p.range(),
                a.name,
                a.range()
            ));
        }
        if a.first > next {
            return Err(format!(
                "show {show:?}: episodes {next}-{} belong to no arc ({})",
                a.first - 1,
                match prev {
                    Some(p) => format!("between {:?} and {:?}", p.name, a.name),
                    None => format!("before {:?}; arcs start at episode 1", a.name),
                }
            ));
        }
        next = a.last + 1;
        prev = Some(a);
    }
    Ok(ShowArcs {
        show: show.clone(),
        arcs,
    })
}

/// The highest absolute episode number an arc may name — far past any show,
/// and low enough that range arithmetic cannot overflow.
const MAX_EPISODE: i64 = 1_000_000;

/// `"21-40"` or `"7"` to an inclusive `(first, last)`, both at least 1.
fn parse_range(text: &str) -> Result<(i64, i64), String> {
    let num = |s: &str| -> Result<i64, String> {
        s.trim()
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=MAX_EPISODE).contains(n))
            .ok_or_else(|| {
                "is not a range like \"21-40\" or an episode like \"7\" (1 to 1000000)".to_string()
            })
    };
    let (first, last) = match text.split_once('-') {
        Some((a, b)) => (num(a)?, num(b)?),
        None => {
            let n = num(text)?;
            (n, n)
        }
    };
    if first > last {
        return Err("starts after it ends".into());
    }
    Ok((first, last))
}

/// Read one arcs file (YAML or TOML by extension, like every config file).
pub fn read_arc_file(path: &Path) -> Result<ArcFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let is_yaml = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"));
    if is_yaml {
        serde_norway::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    } else {
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(show: &str, arcs: &[(&str, &str)]) -> ArcFile {
        ArcFile {
            show: show.into(),
            arcs: arcs
                .iter()
                .map(|(name, episodes)| ArcDecl {
                    name: (*name).into(),
                    episodes: (*episodes).into(),
                })
                .collect(),
        }
    }

    fn one(arcs: &[(&str, &str)]) -> Result<Arcs, String> {
        Arcs::from_files(vec![(PathBuf::from("bc.yaml"), file("Black Clover", arcs))])
    }

    #[test]
    fn episodes_fall_in_their_arc_and_past_the_last_into_the_trailing_bucket() {
        let arcs = one(&[("Dungeon", "1-20"), ("Royal Knights", "21-40")]).unwrap();
        assert_eq!(
            arcs.bucket("Black Clover", 1),
            Some(ArcBucket::Named("Dungeon"))
        );
        assert_eq!(
            arcs.bucket("Black Clover", 21),
            Some(ArcBucket::Named("Royal Knights"))
        );
        assert_eq!(arcs.bucket("Black Clover", 41), Some(ArcBucket::Trailing));
        assert_eq!(arcs.bucket("Naruto", 1), None);
    }

    #[test]
    fn arcs_sort_by_first_episode_not_authored_order() {
        let arcs = one(&[("Two", "21-40"), ("One", "1-20")]).unwrap();
        assert_eq!(
            arcs.bucket("Black Clover", 5),
            Some(ArcBucket::Named("One"))
        );
    }

    #[test]
    fn an_overlap_fails_naming_the_show_and_both_ranges() {
        let err = one(&[("A", "1-20"), ("B", "18-30")]).unwrap_err();
        assert!(err.contains("Black Clover"), "{err}");
        assert!(err.contains("(1-20)") && err.contains("(18-30)"), "{err}");
        assert!(err.contains("overlap"), "{err}");
    }

    #[test]
    fn an_interior_gap_fails_naming_the_show_and_the_missing_range() {
        let err = one(&[("A", "1-20"), ("B", "41-60")]).unwrap_err();
        assert!(err.contains("Black Clover"), "{err}");
        assert!(err.contains("21-40 belong to no arc"), "{err}");
    }

    #[test]
    fn arcs_that_do_not_start_at_episode_one_leave_a_gap() {
        let err = one(&[("A", "5-20")]).unwrap_err();
        assert!(err.contains("1-4 belong to no arc"), "{err}");
    }

    #[test]
    fn two_files_for_one_show_fail_naming_both() {
        let err = Arcs::from_files(vec![
            (
                PathBuf::from("a.yaml"),
                file("Black Clover", &[("A", "1-5")]),
            ),
            (
                PathBuf::from("b.yaml"),
                file("Black Clover", &[("B", "1-5")]),
            ),
        ])
        .unwrap_err();
        assert!(err.contains("Black Clover"), "{err}");
        assert!(err.contains("a.yaml") && err.contains("b.yaml"), "{err}");
    }

    #[test]
    fn a_repeated_arc_name_fails() {
        let err = one(&[("A", "1-5"), ("A", "6-9")]).unwrap_err();
        assert!(err.contains("used twice"), "{err}");
    }

    #[test]
    fn a_malformed_range_fails_naming_the_arc() {
        for bad in ["", "x", "0-5", "9-3", "1-", "1-9223372036854775807"] {
            let err = one(&[("A", bad)]).unwrap_err();
            assert!(err.contains("arc \"A\""), "{bad:?}: {err}");
        }
    }

    #[test]
    fn a_single_episode_is_a_range_of_one() {
        let arcs = one(&[("Pilot", "1"), ("Rest", "2-9")]).unwrap();
        assert_eq!(
            arcs.bucket("Black Clover", 1),
            Some(ArcBucket::Named("Pilot"))
        );
        assert_eq!(
            arcs.bucket("Black Clover", 2),
            Some(ArcBucket::Named("Rest"))
        );
    }

    #[test]
    fn arc_names_are_known_station_wide() {
        let arcs = one(&[("Dungeon", "1-20")]).unwrap();
        assert!(arcs.has_arc_named("Dungeon"));
        assert!(!arcs.has_arc_named("Nope"));
        assert_eq!(arcs.arc_names(), ["Dungeon"]);
    }

    #[test]
    fn parses_a_yaml_file() {
        let f: ArcFile = serde_norway::from_str(
            "show: \"Black Clover\"\narcs:\n  - { name: \"Dungeon\", episodes: \"1-20\" }\n",
        )
        .unwrap();
        assert_eq!(f.show, "Black Clover");
        assert_eq!(f.arcs[0].episodes, "1-20");
    }
}
