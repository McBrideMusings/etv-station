//! Order resolution engine (#69).
//!
//! Takes a resolved set of `entry_id`s (e.g. from [`super::Catalog::resolve_query`])
//! and an [`Order`](crate::config::order::Order) spec, and returns the set in
//! the ordered sequence a channel plays it in. [`super::Catalog::resolve_order`]
//! is the entry point; this module holds the pure helpers.
//!
//! Locked behaviours (#46/#69):
//! - `field:dir` sorts on any sortable scalar column; compound terms apply in
//!   priority order; a trailing implicit `entry_id` key guarantees a total,
//!   reproducible order even when the primary key ties or is null.
//! - Nulls sort last regardless of direction.
//! - `manual` preserves the authored (input) order; `random` is a seeded
//!   shuffle.
//! - A collection's authored sequence is *not* an order here (#107). It belongs
//!   to the (collection, item) pair, not to the item, so a flat set of ids can't
//!   say which collection's positions to read; it is emitted already-ordered by
//!   the `collection` entry via `Catalog::collection_members`.

use crate::config::{Dir, FieldSort};

use super::error::CatalogError;

/// Map an order field to its backing `entries` column, rejecting anything not
/// a sortable scalar (tag/multi-valued fields have no single sort value).
fn sort_column(field: &str) -> Option<&'static str> {
    Some(match field {
        "title" => "title",
        "title_sort" => "title_sort",
        "show" => "show",
        "year" => "year",
        "release_date" => "release_date",
        "season" => "season",
        "episode" => "episode",
        "absolute_episode" => "absolute_episode",
        "duration_ms" => "duration_ms",
        "content_rating" => "content_rating",
        "edition" => "edition",
        _ => return None,
    })
}

/// Build the `ORDER BY` body for a compound `field:dir` sort, with nulls forced
/// last per term and a final `entry_id` tiebreaker for a total order.
pub(super) fn order_by_clause(fields: &[FieldSort]) -> Result<String, CatalogError> {
    let mut parts = Vec::with_capacity(fields.len() + 1);
    for f in fields {
        let col = sort_column(&f.field).ok_or_else(|| {
            CatalogError::Query(format!(
                "cannot order by item.{} (not a sortable scalar field)",
                f.field
            ))
        })?;
        let dir = match f.dir {
            Dir::Asc => "ASC",
            Dir::Desc => "DESC",
        };
        // `col IS NULL` sorts 0 (non-null) before 1 (null) → nulls last in both
        // directions, which the `dir` on the value column alone would not give.
        parts.push(format!("entries.{col} IS NULL, entries.{col} {dir}"));
    }
    parts.push("entries.entry_id ASC".to_string());
    Ok(parts.join(", "))
}

/// Deterministically shuffle in place with a SplitMix64-seeded Fisher–Yates.
/// Same slice contents + same seed ⇒ same permutation. Callers sort first so
/// the result depends only on the multiset and seed, not input order.
pub(super) fn seeded_shuffle(ids: &mut [String], seed: u64) {
    let mut state = seed;
    for i in (1..ids.len()).rev() {
        let j = (next_u64(&mut state) % (i as u64 + 1)) as usize;
        ids.swap(i, j);
    }
}

/// SplitMix64 — a small, fixed, well-distributed PRNG. Fixed algorithm (not
/// `DefaultHasher`/`rand`) so a pinned seed reproduces a shuffle across builds.
fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use crate::catalog::{Catalog, Collection, Entry, Source};
    use crate::config::Order;

    fn ord(s: &str) -> Order {
        Order::parse(s).unwrap()
    }

    /// Three LOTR films + a decoy, with two sharing a release date to exercise
    /// the tiebreaker.
    fn seeded() -> (Catalog, Vec<String>) {
        let c = Catalog::open_in_memory().unwrap();
        let rows = [
            (
                "imdb:tt0120737",
                "The Fellowship of the Ring",
                2001,
                Some("2001-12-19"),
            ),
            ("imdb:tt0167261", "The Two Towers", 2002, Some("2002-12-18")),
            (
                "imdb:tt0167260",
                "The Return of the King",
                2003,
                Some("2003-12-17"),
            ),
            // Shares Fellowship's release_date → tiebreak must decide.
            ("imdb:tt9999999", "Twin Release", 2001, Some("2001-12-19")),
        ];
        for (id, title, year, date) in rows {
            let mut e = Entry::new(id, "movie", title, Source::Plex);
            e.year = Some(year);
            e.release_date = date.map(str::to_string);
            c.upsert_entry(&e).unwrap();
        }
        let ids: Vec<String> = rows.iter().map(|r| r.0.to_string()).collect();
        (c, ids)
    }

    #[test]
    fn release_date_asc_with_tiebreaker() {
        let (c, ids) = seeded();
        let got = c.resolve_order(&ids, &ord("release_date:asc"), 0).unwrap();
        // 2001 pair first, ordered by entry_id (…737 < …999); then 2002, 2003.
        assert_eq!(
            got,
            vec![
                "imdb:tt0120737",
                "imdb:tt9999999",
                "imdb:tt0167261",
                "imdb:tt0167260"
            ]
        );
    }

    #[test]
    fn descending_reverses_but_keeps_entry_id_tiebreak_ascending() {
        let (c, ids) = seeded();
        let got = c.resolve_order(&ids, &ord("release_date:desc"), 0).unwrap();
        // 2003, 2002, then the 2001 pair — still entry_id-ascending within the tie.
        assert_eq!(
            got,
            vec![
                "imdb:tt0167260",
                "imdb:tt0167261",
                "imdb:tt0120737",
                "imdb:tt9999999"
            ]
        );
    }

    #[test]
    fn nulls_sort_last_in_both_directions() {
        let c = Catalog::open_in_memory().unwrap();
        c.upsert_entry(&Entry::new("a", "movie", "A", Source::Plex))
            .unwrap(); // year NULL
        let mut e = Entry::new("b", "movie", "B", Source::Plex);
        e.year = Some(1999);
        c.upsert_entry(&e).unwrap();
        let ids = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            c.resolve_order(&ids, &ord("year:asc"), 0).unwrap(),
            vec!["b", "a"]
        );
        assert_eq!(
            c.resolve_order(&ids, &ord("year:desc"), 0).unwrap(),
            vec!["b", "a"]
        );
    }

    #[test]
    fn manual_preserves_authored_order() {
        let (c, ids) = seeded();
        let authored = vec![
            "imdb:tt0167260".to_string(),
            "imdb:tt0120737".to_string(),
            "imdb:tt0167261".to_string(),
        ];
        let _ = ids;
        assert_eq!(
            c.resolve_order(&authored, &Order::Manual, 0).unwrap(),
            authored
        );
    }

    #[test]
    fn random_is_deterministic_per_seed_and_a_permutation() {
        let (c, ids) = seeded();
        let a = c.resolve_order(&ids, &Order::Random, 42).unwrap();
        let b = c.resolve_order(&ids, &Order::Random, 42).unwrap();
        assert_eq!(a, b, "same seed must reproduce the order");
        let mut sorted = a.clone();
        sorted.sort();
        let mut expect = ids.clone();
        expect.sort();
        assert_eq!(sorted, expect, "shuffle must be a permutation of the input");
    }

    /// Collection order is not an [`Order`] at all (#107) — it is read straight
    /// off the collection, in stored `position` order, by the entry that names
    /// it. Kept here because this module documents the ordering contract, and
    /// "position order lives over there" is part of it.
    #[test]
    fn collection_members_read_in_position_order_with_entry_id_tiebreak() {
        let (c, _ids) = seeded();
        c.upsert_collection(&Collection {
            collection_id: "coll".into(),
            name: "Marathon".into(),
            source: Source::Plex,
        })
        .unwrap();
        c.add_collection_item("coll", "imdb:tt0167260", 0).unwrap();
        c.add_collection_item("coll", "imdb:tt0120737", 1).unwrap();
        // Shared position 1 → entry_id ascending breaks the tie: tt0120737 < tt0167261.
        c.add_collection_item("coll", "imdb:tt0167261", 1).unwrap();
        assert_eq!(
            c.collection_members("coll").unwrap(),
            vec!["imdb:tt0167260", "imdb:tt0120737", "imdb:tt0167261"]
        );
        // The seed's decoy entry is not a member, so it simply isn't emitted —
        // there is no resolved set to reconcile against.
        assert_eq!(c.collection_members("coll").unwrap().len(), 3);
    }

    #[test]
    fn compound_sort_applies_terms_in_priority() {
        let c = Catalog::open_in_memory().unwrap();
        for (id, season, episode) in [("e21", 2, 1), ("e13", 1, 3), ("e11", 1, 1)] {
            let mut e = Entry::new(id, "episode", id, Source::Plex);
            e.season = Some(season);
            e.episode = Some(episode);
            c.upsert_entry(&e).unwrap();
        }
        let ids = vec!["e21".to_string(), "e13".to_string(), "e11".to_string()];
        let got = c
            .resolve_order(&ids, &ord("season:asc,episode:asc"), 0)
            .unwrap();
        assert_eq!(got, vec!["e11", "e13", "e21"]);
    }

    #[test]
    fn non_sortable_field_is_an_error() {
        let (c, ids) = seeded();
        let e = c.resolve_order(&ids, &ord("genres:asc"), 0).unwrap_err();
        assert!(e.to_string().contains("not a sortable scalar field"));
    }

    #[test]
    fn score_order_is_unsupported() {
        let (c, ids) = seeded();
        let e = c.resolve_order(&ids, &Order::Score, 0).unwrap_err();
        assert!(e.to_string().contains("score"));
    }

    #[test]
    fn empty_input_is_empty_output() {
        let (c, _) = seeded();
        assert!(
            c.resolve_order(&[], &ord("year:asc"), 0)
                .unwrap()
                .is_empty()
        );
    }
}
