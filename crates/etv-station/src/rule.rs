use std::time::Duration;

use ersatztv_playout::playout::{OverlaySpec, PlayoutItem, ProgramMetadata};
use time::OffsetDateTime;

use crate::config::ItemConfig;

pub trait Rule {
    fn items_covering(
        &self,
        anchor_utc: OffsetDateTime,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Vec<PlayoutItem>;
}

pub struct LoopForever<'a> {
    items: &'a [ItemConfig],
    durations: &'a [Duration],
    total_secs: f64,
    overlay: Option<OverlaySpec>,
}

impl<'a> LoopForever<'a> {
    pub fn new(items: &'a [ItemConfig], durations: &'a [Duration]) -> Self {
        assert_eq!(
            items.len(),
            durations.len(),
            "items/durations length mismatch"
        );
        let total_secs: f64 = durations.iter().map(|d| d.as_secs_f64()).sum();
        Self {
            items,
            durations,
            total_secs,
            overlay: None,
        }
    }

    pub fn with_overlay(mut self, overlay: Option<OverlaySpec>) -> Self {
        self.overlay = overlay;
        self
    }
}

impl<'a> Rule for LoopForever<'a> {
    fn items_covering(
        &self,
        anchor_utc: OffsetDateTime,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Vec<PlayoutItem> {
        if self.items.is_empty() || self.total_secs == 0.0 || to <= from {
            return Vec::new();
        }

        let from_offset_secs = (from - anchor_utc).as_seconds_f64();
        let elapsed_in_loop = from_offset_secs.rem_euclid(self.total_secs);

        let (mut idx, start_in_item) = walk_to_offset(self.durations, elapsed_in_loop);
        let mut item_start_utc = from - time::Duration::seconds_f64(start_in_item);

        let mut out = Vec::new();
        loop {
            let dur = self.durations[idx];
            let item_finish_utc = item_start_utc + time::Duration::seconds_f64(dur.as_secs_f64());

            out.push(build_playout_item(
                &self.items[idx],
                item_start_utc,
                item_finish_utc,
                self.overlay.as_ref(),
            ));

            if item_finish_utc >= to {
                break;
            }
            item_start_utc = item_finish_utc;
            idx = (idx + 1) % self.items.len();
        }
        out
    }
}

fn walk_to_offset(durations: &[Duration], offset_secs: f64) -> (usize, f64) {
    let mut remaining = offset_secs;
    for (i, d) in durations.iter().enumerate() {
        let secs = d.as_secs_f64();
        if remaining < secs {
            return (i, remaining);
        }
        remaining -= secs;
    }
    (0, 0.0)
}

fn build_playout_item(
    item: &ItemConfig,
    start: OffsetDateTime,
    finish: OffsetDateTime,
    overlay: Option<&OverlaySpec>,
) -> PlayoutItem {
    PlayoutItem {
        id: item.id.clone(),
        start,
        finish,
        source: Some(item.to_playout_source()),
        tracks: None,
        watermark: None,
        program: item.program.as_ref().map(clone_program),
        overlay: overlay.cloned(),
    }
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

    fn lavfi(id: &str, secs: u64) -> ItemConfig {
        ItemConfig {
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
        let rule = LoopForever::new(&items, &durs);
        let t = datetime!(2026-04-13 00:00 UTC);
        assert!(rule.items_covering(t, t, t).is_empty());
    }

    #[test]
    fn covers_a_single_item_window() {
        let items = vec![lavfi("a", 60), lavfi("b", 60)];
        let durs = vec![Duration::from_secs(60), Duration::from_secs(60)];
        let rule = LoopForever::new(&items, &durs);
        let anchor = datetime!(2026-04-13 00:00 UTC);
        let from = datetime!(2026-04-13 00:00 UTC);
        let to = datetime!(2026-04-13 00:00:30 UTC);
        let result = rule.items_covering(anchor, from, to);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "a");
        assert_eq!(result[0].start, from);
        assert_eq!(result[0].finish, datetime!(2026-04-13 00:01 UTC));
    }

    #[test]
    fn loops_across_window() {
        let items = vec![lavfi("a", 60), lavfi("b", 60)];
        let durs = vec![Duration::from_secs(60), Duration::from_secs(60)];
        let rule = LoopForever::new(&items, &durs);
        let anchor = datetime!(2026-04-13 00:00 UTC);
        let from = anchor;
        let to = datetime!(2026-04-13 00:05 UTC);
        let result = rule.items_covering(anchor, from, to);
        // 5 minutes / 1 minute per item = 5 items, alternating a/b
        assert_eq!(result.len(), 5);
        let ids: Vec<&str> = result.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "a", "b", "a"]);
    }

    #[test]
    fn determinism_byte_equal() {
        let items = vec![lavfi("a", 30), lavfi("b", 45), lavfi("c", 90)];
        let durs = vec![
            Duration::from_secs(30),
            Duration::from_secs(45),
            Duration::from_secs(90),
        ];
        let anchor = datetime!(2026-04-13 00:00 UTC);
        let from = datetime!(2026-04-13 02:00 UTC);
        let to = datetime!(2026-04-13 03:30 UTC);

        let r1 = LoopForever::new(&items, &durs).items_covering(anchor, from, to);
        let r2 = LoopForever::new(&items, &durs).items_covering(anchor, from, to);

        let j1 = serde_json::to_vec(&r1).unwrap();
        let j2 = serde_json::to_vec(&r2).unwrap();
        assert_eq!(j1, j2);
    }

    #[test]
    fn first_item_can_start_before_window() {
        let items = vec![lavfi("a", 60), lavfi("b", 60)];
        let durs = vec![Duration::from_secs(60), Duration::from_secs(60)];
        let rule = LoopForever::new(&items, &durs);
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
