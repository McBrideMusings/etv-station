use std::time::Duration;

use ersatztv_playout::playout::{OverlaySpec, PlayoutItem, ProgramMetadata};
use time::OffsetDateTime;

use crate::resolve::ResolvedItem;

pub trait Rule {
    fn items_covering(
        &self,
        anchor_utc: OffsetDateTime,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Vec<PlayoutItem>;
}

/// Play a resolved list once, end to end, from a given start.
///
/// This is the only emission model. Each generation covers the span after the
/// last one, the already-written chunk JSON is the durable timeline, and the
/// `.resume` sidecar carries only where the next span picks up. A channel whose
/// list never changes resolves the same list every generation, and those laid
/// end-to-end are the loop — so looping needs no separate rule.
///
/// `start_utc` is where this sequence begins, not a repeating phase origin: the
/// sequence ends when the items run out, and the next generation continues
/// after it.
pub struct Sequential<'a> {
    items: &'a [ResolvedItem],
    durations: &'a [Duration],
    overlay: Option<OverlaySpec>,
}

impl<'a> Sequential<'a> {
    pub fn new(items: &'a [ResolvedItem], durations: &'a [Duration]) -> Self {
        assert_eq!(
            items.len(),
            durations.len(),
            "items/durations length mismatch"
        );
        Self {
            items,
            durations,
            overlay: None,
        }
    }

    pub fn with_overlay(mut self, overlay: Option<OverlaySpec>) -> Self {
        self.overlay = overlay;
        self
    }

    /// Wall-clock length of the whole sequence — how far forward one generation
    /// reaches, which is what bounds the emission window.
    pub fn total_duration(&self) -> time::Duration {
        time::Duration::seconds_f64(self.durations.iter().map(|d| d.as_secs_f64()).sum())
    }
}

impl Rule for Sequential<'_> {
    fn items_covering(
        &self,
        start_utc: OffsetDateTime,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Vec<PlayoutItem> {
        if self.items.is_empty() || to <= from {
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut item_start_utc = start_utc;
        for (idx, dur) in self.durations.iter().enumerate() {
            let item_finish_utc = item_start_utc + time::Duration::seconds_f64(dur.as_secs_f64());
            // Past the window — the sequence is ordered, so nothing later can
            // qualify either.
            if item_start_utc >= to {
                break;
            }
            // An item that finishes exactly at `from` belongs to the previous
            // window, not this one; one straddling `from` is emitted whole so
            // the boundary never cuts a program in half.
            if item_finish_utc > from {
                out.push(build_playout_item(
                    &self.items[idx],
                    item_start_utc,
                    item_finish_utc,
                    self.overlay.as_ref(),
                ));
            }
            item_start_utc = item_finish_utc;
        }
        out
    }
}

fn build_playout_item(
    item: &ResolvedItem,
    start: OffsetDateTime,
    finish: OffsetDateTime,
    overlay: Option<&OverlaySpec>,
) -> PlayoutItem {
    let mut playout_item =
        PlayoutItem::scheduled(item.id.clone(), start, finish, item.to_playout_source());
    playout_item.program = item.program.as_ref().map(clone_program);
    playout_item.overlay = overlay.cloned();
    playout_item
}

fn clone_program(p: &ProgramMetadata) -> ProgramMetadata {
    ProgramMetadata {
        title: p.title.clone(),
        sub_title: p.sub_title.clone(),
        description: p.description.clone(),
        season: p.season,
        episode: p.episode,
        categories: p.categories.clone(),
        content_rating: p.content_rating.clone(),
        artwork_url: p.artwork_url.clone(),
        year: p.year,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceConfig;
    use time::macros::datetime;

    fn lavfi(id: &str, secs: u64) -> ResolvedItem {
        ResolvedItem {
            id: id.into(),
            source: SourceConfig::Lavfi {
                params: format!("src={id}"),
            },
            in_point: Some(Duration::ZERO),
            out_point: Some(Duration::from_secs(secs)),
            program: None,
        }
    }

    #[test]
    fn empty_window_yields_nothing() {
        let items = vec![lavfi("a", 30)];
        let durs = vec![Duration::from_secs(30)];
        let rule = Sequential::new(&items, &durs);
        let t = datetime!(2026-04-13 00:00 UTC);
        assert!(rule.items_covering(t, t, t).is_empty());
    }

    #[test]
    fn covers_a_single_item_window() {
        let items = vec![lavfi("a", 60), lavfi("b", 60)];
        let durs = vec![Duration::from_secs(60), Duration::from_secs(60)];
        let rule = Sequential::new(&items, &durs);
        let start = datetime!(2026-04-13 00:00 UTC);
        let result = rule.items_covering(start, start, datetime!(2026-04-13 00:00:30 UTC));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "a");
        assert_eq!(result[0].start, start);
        assert_eq!(result[0].finish, datetime!(2026-04-13 00:01 UTC));
    }

    /// The same list resolved again and laid after the first is the loop: no
    /// separate looping rule is needed to keep a fixed-list channel on air.
    #[test]
    fn consecutive_generations_of_one_list_are_a_loop() {
        let items = vec![lavfi("a", 60), lavfi("b", 60)];
        let durs = vec![Duration::from_secs(60), Duration::from_secs(60)];
        let rule = Sequential::new(&items, &durs);
        let mut ids = Vec::new();
        let mut start = datetime!(2026-04-13 00:00 UTC);
        for _ in 0..3 {
            let pass = rule.items_covering(start, start, start + rule.total_duration());
            start += rule.total_duration();
            ids.extend(pass.into_iter().map(|i| i.id));
        }
        assert_eq!(ids, vec!["a", "b", "a", "b", "a", "b"]);
    }

    #[test]
    fn determinism_byte_equal() {
        let items = vec![lavfi("a", 30), lavfi("b", 45), lavfi("c", 90)];
        let durs = vec![
            Duration::from_secs(30),
            Duration::from_secs(45),
            Duration::from_secs(90),
        ];
        let start = datetime!(2026-04-13 00:00 UTC);
        let to = datetime!(2026-04-13 03:30 UTC);

        let r1 = Sequential::new(&items, &durs).items_covering(start, start, to);
        let r2 = Sequential::new(&items, &durs).items_covering(start, start, to);

        let j1 = serde_json::to_vec(&r1).unwrap();
        let j2 = serde_json::to_vec(&r2).unwrap();
        assert_eq!(j1, j2);
    }

    // ---- Sequential (#72) --------------------------------------------------

    #[test]
    fn sequential_plays_the_list_once_and_stops() {
        let items = vec![lavfi("a", 60), lavfi("b", 60)];
        let durs = vec![Duration::from_secs(60), Duration::from_secs(60)];
        let rule = Sequential::new(&items, &durs);
        let start = datetime!(2026-04-13 00:00 UTC);
        // A window far longer than the sequence: it must not loop to fill it.
        let result = rule.items_covering(start, start, datetime!(2026-04-13 01:00 UTC));
        let ids: Vec<&str> = result.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(result[0].start, start);
        assert_eq!(result[1].finish, datetime!(2026-04-13 00:02 UTC));
    }

    #[test]
    fn sequential_slices_to_the_requested_window() {
        let items = vec![lavfi("a", 60), lavfi("b", 60), lavfi("c", 60)];
        let durs = vec![
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(60),
        ];
        let rule = Sequential::new(&items, &durs);
        let start = datetime!(2026-04-13 00:00 UTC);
        // Second minute only — "a" has finished, "c" has not begun.
        let result = rule.items_covering(
            start,
            datetime!(2026-04-13 00:01 UTC),
            datetime!(2026-04-13 00:02 UTC),
        );
        let ids: Vec<&str> = result.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["b"]);
    }

    #[test]
    fn sequential_emits_a_straddling_item_whole() {
        // A chunk boundary must not cut a program in half: an item running
        // across `from` is emitted with its real start.
        let items = vec![lavfi("a", 120)];
        let durs = vec![Duration::from_secs(120)];
        let rule = Sequential::new(&items, &durs);
        let start = datetime!(2026-04-13 00:00 UTC);
        let result = rule.items_covering(
            start,
            datetime!(2026-04-13 00:01 UTC),
            datetime!(2026-04-13 00:03 UTC),
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].start, start);
    }

    #[test]
    fn sequential_total_duration_bounds_the_window() {
        let items = vec![lavfi("a", 30), lavfi("b", 90)];
        let durs = vec![Duration::from_secs(30), Duration::from_secs(90)];
        let rule = Sequential::new(&items, &durs);
        assert_eq!(rule.total_duration(), time::Duration::seconds(120));
    }

    #[test]
    fn first_item_can_start_before_window() {
        let items = vec![lavfi("a", 60), lavfi("b", 60)];
        let durs = vec![Duration::from_secs(60), Duration::from_secs(60)];
        let rule = Sequential::new(&items, &durs);
        let anchor = datetime!(2026-04-13 00:00 UTC);
        // ask for window starting 30s into item 'a'
        let from = datetime!(2026-04-13 00:00:30 UTC);
        let to = datetime!(2026-04-13 00:02:00 UTC);
        let result = rule.items_covering(anchor, from, to);
        assert_eq!(result[0].id, "a");
        assert_eq!(result[0].start, anchor);
        assert_eq!(result[0].finish, datetime!(2026-04-13 00:01 UTC));
    }
}
