//! Audio facts per catalog entry (`entry_audio`, schema v12).
//!
//! One row per entry the analyzer has been run on. A row whose `error` is NULL
//! carries the analyzer's JSON in `facts`; a row with an `error` records that
//! the analyzer ran against `source_path` and failed, so the queue does not
//! retry the same file every pass. A changed `source_path` makes an entry
//! pending again.

use rusqlite::{OptionalExtension, params};

use super::{Catalog, CatalogError};

/// One `entry_audio` row.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioRow {
    pub source_path: String,
    pub analyzed_at: String,
    /// Why the analyzer failed on `source_path`; `None` when `facts` is set.
    pub error: Option<String>,
    pub facts: Option<serde_json::Value>,
}

/// How much of the catalog has been analyzed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioCounts {
    /// Live entries with facts stored.
    pub analyzed: i64,
    /// Live entries whose analysis failed.
    pub failed: i64,
    /// Live entries.
    pub total: i64,
}

/// The live source an entry is analyzed from: the lowest playback path among
/// its sources that are present and not marked unairable.
const PENDING_SQL: &str = "\
    SELECT e.entry_id, MIN(s.playback_path) \
      FROM entries e \
      JOIN entry_sources s ON s.entry_id = e.entry_id \
       AND s.missing_since IS NULL AND s.unairable_reason IS NULL \
      LEFT JOIN entry_audio a ON a.entry_id = e.entry_id \
     WHERE e.missing_since IS NULL \
     GROUP BY e.entry_id \
    HAVING MIN(a.source_path) IS NULL OR MIN(a.source_path) <> MIN(s.playback_path) \
     ORDER BY e.entry_id \
     LIMIT ?1";

impl Catalog {
    /// Store the outcome of analyzing `source_path` for `entry_id`, replacing
    /// any earlier row. `Ok(facts)` stores the JSON; `Err(message)` stores the
    /// failure.
    pub fn set_audio_fact(
        &self,
        entry_id: &str,
        source_path: &str,
        analyzed_at: &str,
        outcome: Result<&serde_json::Value, &str>,
    ) -> Result<(), CatalogError> {
        let (facts, error) = match outcome {
            Ok(v) => (Some(v.to_string()), None),
            Err(e) => (None, Some(e)),
        };
        self.conn.execute(
            "INSERT INTO entry_audio (entry_id, source_path, analyzed_at, error, facts) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(entry_id) DO UPDATE SET source_path = excluded.source_path, \
               analyzed_at = excluded.analyzed_at, error = excluded.error, facts = excluded.facts",
            params![entry_id, source_path, analyzed_at, error, facts],
        )?;
        Ok(())
    }

    /// The audio row for an entry, if the analyzer has run on it.
    pub fn audio_fact(&self, entry_id: &str) -> Result<Option<AudioRow>, CatalogError> {
        let row = self
            .conn
            .query_row(
                "SELECT source_path, analyzed_at, error, facts FROM entry_audio WHERE entry_id = ?1",
                params![entry_id],
                audio_row,
            )
            .optional()?;
        row.transpose()
    }

    /// The stored facts for the entry that plays from `playback_path`, when
    /// that entry has been analyzed successfully. This is what `ctx.probe`
    /// returns under `audio`.
    pub fn audio_facts_for_path(
        &self,
        playback_path: &str,
    ) -> Result<Option<serde_json::Value>, CatalogError> {
        let facts: Option<String> = self
            .conn
            .query_row(
                "SELECT a.facts FROM entry_audio a \
                   JOIN entry_sources s ON s.entry_id = a.entry_id \
                  WHERE s.playback_path = ?1 AND a.facts IS NOT NULL LIMIT 1",
                params![playback_path],
                |r| r.get(0),
            )
            .optional()?;
        facts
            .map(|text| {
                serde_json::from_str(&text).map_err(|e| CatalogError::BadRow {
                    field: "entry_audio.facts",
                    message: e.to_string(),
                })
            })
            .transpose()
    }

    /// Up to `limit` live entries still to analyze, as `(entry_id, path)`: no
    /// row yet, or a row recorded against a different path.
    pub fn audio_pending(&self, limit: usize) -> Result<Vec<(String, String)>, CatalogError> {
        let mut stmt = self.conn.prepare(PENDING_SQL)?;
        let rows = stmt.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Analyzed / failed / total over live entries.
    pub fn audio_counts(&self) -> Result<AudioCounts, CatalogError> {
        let (analyzed, failed, total) = self.conn.query_row(
            "SELECT \
               COALESCE(SUM(a.facts IS NOT NULL), 0), \
               COALESCE(SUM(a.error IS NOT NULL), 0), \
               COUNT(*) \
             FROM entries e LEFT JOIN entry_audio a ON a.entry_id = e.entry_id \
             WHERE e.missing_since IS NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(AudioCounts {
            analyzed,
            failed,
            total,
        })
    }
}

fn audio_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<AudioRow, CatalogError>> {
    let facts: Option<String> = r.get(3)?;
    let facts = facts
        .map(|text| {
            serde_json::from_str(&text).map_err(|e| CatalogError::BadRow {
                field: "entry_audio.facts",
                message: e.to_string(),
            })
        })
        .transpose();
    Ok(facts.map(|facts| AudioRow {
        source_path: r.get_unwrap(0),
        analyzed_at: r.get_unwrap(1),
        error: r.get_unwrap(2),
        facts,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Entry, EntrySource, Source};
    use serde_json::json;

    fn source(entry: &str, source_id: &str, path: &str) -> EntrySource {
        EntrySource {
            source: Source::LocalFs,
            source_id: source_id.into(),
            entry_id: entry.into(),
            playback_path: path.into(),
            last_seen: None,
            missing_since: None,
            unairable_reason: None,
        }
    }

    fn catalog_with(entries: &[(&str, &str)]) -> Catalog {
        let cat = Catalog::open_in_memory().unwrap();
        for (id, path) in entries {
            cat.upsert_entry(&Entry::new(*id, "movie", *id, Source::LocalFs))
                .unwrap();
            cat.add_source(&source(id, id, path)).unwrap();
        }
        cat
    }

    #[test]
    fn a_stored_fact_is_read_back_by_entry_and_by_path_and_counted() {
        let cat = catalog_with(&[("a", "/m/a.mp4"), ("b", "/m/b.mp4")]);
        assert_eq!(cat.audio_pending(10).unwrap().len(), 2);
        let facts = json!({"bpm": 120.0, "key": "C"});
        cat.set_audio_fact("a", "/m/a.mp4", "2026-01-01T00:00:00Z", Ok(&facts))
            .unwrap();
        assert_eq!(
            cat.audio_facts_for_path("/m/a.mp4").unwrap(),
            Some(facts.clone())
        );
        assert_eq!(cat.audio_facts_for_path("/m/b.mp4").unwrap(), None);
        assert_eq!(cat.audio_fact("a").unwrap().unwrap().facts, Some(facts));
        assert_eq!(
            cat.audio_pending(10).unwrap(),
            vec![("b".to_string(), "/m/b.mp4".to_string())]
        );
        assert_eq!(
            cat.audio_counts().unwrap(),
            AudioCounts {
                analyzed: 1,
                failed: 0,
                total: 2
            }
        );
    }

    #[test]
    fn a_failed_file_is_not_retried_until_its_path_changes() {
        let cat = catalog_with(&[("a", "/m/a.mp4")]);
        cat.set_audio_fact("a", "/m/a.mp4", "2026-01-01T00:00:00Z", Err("no audio"))
            .unwrap();
        assert!(cat.audio_pending(10).unwrap().is_empty());
        assert_eq!(cat.audio_facts_for_path("/m/a.mp4").unwrap(), None);
        assert_eq!(cat.audio_counts().unwrap().failed, 1);

        // The file moves: its old row goes missing and a new one appears.
        cat.conn
            .execute("UPDATE entry_sources SET missing_since = 'x'", [])
            .unwrap();
        cat.add_source(&source("a", "a2", "/m/moved.mp4")).unwrap();
        assert_eq!(
            cat.audio_pending(10).unwrap(),
            vec![("a".to_string(), "/m/moved.mp4".to_string())]
        );
    }
}
