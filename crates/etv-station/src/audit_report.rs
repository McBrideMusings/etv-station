//! One-shot upcoming-schedule report: `etv-station --audit <channel> [--next
//! N]` (#390, ADR 0011).
//!
//! This reads the channel's chunk files exactly as they sit on disk — the
//! same files ETV-next reads to play the channel — and reports what will
//! actually air, not what a re-simulation of a plugin's `pick()` believes
//! would air. Opens no catalog, no plexdb snapshot, and evaluates no plugin.
//!
//! Reading and rendering are two separate functions on purpose: [`upcoming`]
//! does the file I/O, [`render`] turns an already-built item list into text,
//! so the writer is testable against a fixture list with no filesystem at
//! all.

use std::collections::HashSet;
use std::path::Path;

use ersatztv_playout::playout::Playout;
use time::OffsetDateTime;

use crate::errors::StationError;
use crate::scan;

/// One item on the upcoming schedule, flattened out of whichever chunk file
/// held it. Plain owned data — no [`ersatztv_playout::playout::PlayoutItem`]
/// in this shape — so a test can build one by hand with no fixture file.
#[derive(Debug, Clone)]
pub struct ReportItem {
    pub id: String,
    pub start: OffsetDateTime,
    pub finish: OffsetDateTime,
    pub title: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// Read `output_folder`'s chunk files, discard chunks that have already
/// finished, flatten and sort the survivors by start, and return the first
/// `next` items from `now` forward.
///
/// A boundary-straddling item is written whole into both of its neighbouring
/// chunk files (ADR 0003), so a naive flatten would report it twice; this
/// dedupes on `(id, start)` after sorting.
pub async fn upcoming(
    output_folder: &Path,
    now: OffsetDateTime,
    next: usize,
) -> Result<Vec<ReportItem>, StationError> {
    let files = scan::scan_output_folder(output_folder).await?;

    let mut items: Vec<ReportItem> = Vec::new();
    for file in &files {
        // The filename finish is authoritative for every chunk but the
        // frontier one (see scan::highest_finish's doc comment); a whole
        // chunk that finished before `now` cannot contribute an unfinished
        // item, so it is skipped without being opened.
        if file.finish <= now {
            continue;
        }
        let bytes = tokio::fs::read(&file.path)
            .await
            .map_err(|source| StationError::Io {
                path: file.path.clone(),
                source,
            })?;
        let playout: Playout =
            serde_json::from_slice(&bytes).map_err(|source| StationError::PlayoutCorrupt {
                path: file.path.clone(),
                source,
            })?;
        for item in playout.items {
            if item.finish <= now {
                continue;
            }
            let title = item
                .program
                .as_ref()
                .and_then(|p| p.title.clone().or_else(|| p.sub_title.clone()));
            items.push(ReportItem {
                id: item.id,
                start: item.start,
                finish: item.finish,
                title,
                metadata: item.metadata,
            });
        }
    }

    items.sort_by_key(|i| i.start);

    let mut seen: HashSet<(String, OffsetDateTime)> = HashSet::new();
    items.retain(|i| seen.insert((i.id.clone(), i.start)));

    items.truncate(next);
    Ok(items)
}

/// Render `items` (already the upcoming slice — this does no filtering or
/// truncation) as a plain-text report.
///
/// Each item's audit trail comes from `metadata.audit`, an ordered array
/// whose per-stage record is a map of `stage`/`by`/`verdict` plus whatever
/// else the script put under `detail` (ADR 0011, ADR 0002) — every key is
/// printed generically, the same way `taste-debug`'s metadata printer works,
/// so a script adding a new `detail` key needs no station change to appear
/// here. An item carrying no audit trail (every item written before #389, and
/// every item on a channel with no plugin pool) renders one "unexplained"
/// line rather than erroring.
pub fn render(channel: &str, generated_at: OffsetDateTime, items: &[ReportItem]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "== {channel}: upcoming schedule, generated {generated_at} ({} item(s)) ==\n",
        items.len(),
    ));
    for item in items {
        out.push('\n');
        let title = item.title.as_deref().unwrap_or(item.id.as_str());
        out.push_str(&format!("{} — {title}\n", clock(item.start)));
        render_audit_trail(&mut out, item.metadata.as_ref());
    }
    out
}

fn render_audit_trail(out: &mut String, metadata: Option<&serde_json::Value>) {
    let audit = metadata
        .and_then(|m| m.as_object())
        .and_then(|m| m.get("audit"))
        .and_then(|a| a.as_array());

    let Some(audit) = audit else {
        out.push_str("    (unexplained — no audit trail)\n");
        return;
    };
    if audit.is_empty() {
        out.push_str("    (unexplained — no audit trail)\n");
        return;
    }

    for record in audit {
        let Some(map) = record.as_object() else {
            out.push_str("    (unexplained — malformed audit record)\n");
            continue;
        };
        let stage = map.get("stage").map(format_value).unwrap_or_default();
        let by = map.get("by").map(format_value).unwrap_or_default();
        let verdict = map.get("verdict").map(format_value).unwrap_or_default();
        out.push_str(&format!("    [{stage}] {by}: {verdict}\n"));

        if let Some(detail) = map.get("detail").and_then(|d| d.as_object()) {
            render_detail(out, detail);
        }
    }
}

/// Render a stage's `detail` map as an aligned block, one key per line.
///
/// It used to be one inline `(a=1 b=2 …)` parenthetical, which held up for the
/// three scalars #392 put there and collapsed the moment #393 added a list of
/// near-miss candidates: the whole list serialized as raw JSON on one line,
/// several hundred characters wide, and the numbers a reader actually wanted
/// were buried in it.
///
/// Still entirely key-agnostic (ADR 0002) — no key name appears in this
/// function, so a script adding one needs no change here. The only thing it
/// branches on is a value's JSON *shape*.
fn render_detail(out: &mut String, detail: &serde_json::Map<String, serde_json::Value>) {
    let width = detail.keys().map(|k| k.len()).max().unwrap_or(0);
    for (key, value) in detail {
        match value {
            // A list of records — near-miss candidates, and anything else
            // shaped like them — gets one line each rather than one long line.
            serde_json::Value::Array(items)
                if items.iter().any(|i| i.is_object()) && !items.is_empty() =>
            {
                out.push_str(&format!("        {key}:\n"));
                for entry in items {
                    match entry.as_object() {
                        Some(fields) => {
                            let rendered = fields
                                .iter()
                                .map(|(k, v)| format!("{k}={}", format_value(v)))
                                .collect::<Vec<_>>()
                                .join("  ");
                            out.push_str(&format!("          - {rendered}\n"));
                        }
                        None => out.push_str(&format!("          - {}\n", format_value(entry))),
                    }
                }
            }
            // An empty list is worth saying out loud: for a near-miss list it
            // means the pick beat nothing that placed near it, which is a real
            // finding and not the same as the key being absent.
            serde_json::Value::Array(items) if items.is_empty() => {
                out.push_str(&format!("        {key:<width$}  (none)\n"));
            }
            serde_json::Value::Object(fields) => {
                out.push_str(&format!("        {key}:\n"));
                let inner = fields.keys().map(|k| k.len()).max().unwrap_or(0);
                for (k, v) in fields {
                    out.push_str(&format!(
                        "          {k:<inner$}  {}\n",
                        format_value(v),
                        inner = inner
                    ));
                }
            }
            other => {
                out.push_str(&format!(
                    "        {key:<width$}  {}\n",
                    format_value(other),
                    width = width
                ));
            }
        }
    }
}

/// An item's start, to the second and without the trailing offset noise.
///
/// The raw `OffsetDateTime` renders as `2026-08-31 17:47:18.442285 +00:00:00`.
/// Sub-second precision on a programme start is noise a reader has to look
/// past on every line, and the offset is always UTC here because that is what
/// the playout files store.
fn clock(t: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
    )
}

/// Lifted from `taste-debug`'s printer (ADR 0002: metadata is opaque to the
/// station) so a `detail` value renders the same way here as it does there.
fn format_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(format_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        serde_json::Value::String(s) => s.clone(),
        // A whole number prints whole. Rhai has one numeric type, so a count
        // and a rank arrive as floats and rendered at a fixed 4dp they read as
        // `candidate_count=11543.0000` and `rank=9.0000` — precision that is
        // not merely noise but actively misleading about what the value is.
        // A genuine fraction keeps 4dp, trailing zeros trimmed.
        serde_json::Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            Some(f) => {
                let s = format!("{f:.4}");
                s.trim_end_matches('0').trim_end_matches('.').to_string()
            }
            None => n.to_string(),
        },
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ersatztv_playout::playout::{Playout, PlayoutItem, ProgramMetadata};
    use time::Duration;
    use time::macros::datetime;

    /// Write one chunk file holding `n` half-hour items laid end to end from
    /// `start`, named the way `emit` names a full chunk — mirrors
    /// `backfill.rs`'s `write_chunk` test helper.
    async fn write_chunk(
        folder: &Path,
        start: OffsetDateTime,
        n: usize,
        audit_for: impl Fn(usize) -> Option<serde_json::Value>,
    ) {
        let mut items = Vec::new();
        let mut cursor = start;
        for i in 0..n {
            let finish = cursor + Duration::minutes(30);
            items.push(PlayoutItem {
                id: format!("entry-{i}"),
                start: cursor,
                finish,
                source: None,
                tracks: None,
                watermark: None,
                program: Some(ProgramMetadata {
                    title: Some(format!("Title {i}")),
                    ..Default::default()
                }),
                metadata: audit_for(i),
            });
            cursor = finish;
        }
        let name = crate::emit::chunk_filename(start, cursor).unwrap();
        tokio::fs::write(
            folder.join(name),
            serde_json::to_vec(&Playout::new(items)).unwrap(),
        )
        .await
        .unwrap();
    }

    fn no_audit(_i: usize) -> Option<serde_json::Value> {
        None
    }

    #[tokio::test]
    async fn returns_exactly_n_items_in_start_order_from_first_unfinished() {
        let dir = tempfile::tempdir().unwrap();
        let start = datetime!(2026-04-20 12:00 UTC);
        write_chunk(dir.path(), start, 6, no_audit).await;

        // now sits partway through entry-1 (12:30-13:00): entry-0 has
        // finished, entry-1 has not.
        let now = datetime!(2026-04-20 12:45 UTC);
        let items = upcoming(dir.path(), now, 3).await.unwrap();

        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, "entry-1");
        assert_eq!(items[1].id, "entry-2");
        assert_eq!(items[2].id, "entry-3");
        assert!(items.windows(2).all(|w| w[0].start <= w[1].start));
    }

    #[tokio::test]
    async fn a_wholly_past_chunk_contributes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let past_start = datetime!(2026-04-20 06:00 UTC);
        write_chunk(dir.path(), past_start, 2, no_audit).await; // finishes 07:00

        let future_start = datetime!(2026-04-20 12:00 UTC);
        write_chunk(dir.path(), future_start, 2, no_audit).await;

        let now = datetime!(2026-04-20 08:00 UTC);
        let items = upcoming(dir.path(), now, 10).await.unwrap();

        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.start >= future_start));
    }

    #[tokio::test]
    async fn default_next_is_ten_and_override_is_honoured() {
        let dir = tempfile::tempdir().unwrap();
        let start = datetime!(2026-04-20 00:00 UTC);
        write_chunk(dir.path(), start, 20, no_audit).await;

        let now = start;
        let default_slice = upcoming(dir.path(), now, 10).await.unwrap();
        assert_eq!(default_slice.len(), 10);

        let overridden = upcoming(dir.path(), now, 4).await.unwrap();
        assert_eq!(overridden.len(), 4);
    }

    #[tokio::test]
    async fn an_item_with_no_audit_trail_is_reported_unexplained_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let start = datetime!(2026-04-20 00:00 UTC);
        write_chunk(dir.path(), start, 1, no_audit).await;

        let items = upcoming(dir.path(), start, 10).await.unwrap();
        assert_eq!(items.len(), 1);
        let report = render("ch", start, &items);
        assert!(report.contains("unexplained"));
    }

    #[tokio::test]
    async fn a_detail_key_the_test_invents_appears_with_no_code_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let start = datetime!(2026-04-20 00:00 UTC);
        write_chunk(dir.path(), start, 1, |_| {
            Some(serde_json::json!({
                "audit": [
                    {
                        "stage": "pool",
                        "by": "taste-cosine",
                        "verdict": "picked",
                        "detail": { "a_key_this_test_made_up": 42 },
                    }
                ]
            }))
        })
        .await;

        let items = upcoming(dir.path(), start, 10).await.unwrap();
        let report = render("ch", start, &items);
        // The key appears with no code here naming it, and its whole-number
        // value renders whole — `42`, not the `42.0000` a fixed 4dp produced.
        assert!(
            report.contains("a_key_this_test_made_up  42\n"),
            "expected the invented key rendered whole, got:\n{report}",
        );
    }

    /// A list of records renders one entry per line, not as raw JSON.
    ///
    /// #393's near-miss list was the case that broke the old inline
    /// parenthetical: several hundred characters of serialized JSON on one
    /// line, with the score and rank a reader wanted buried inside it. Still
    /// key-agnostic — this test invents its own field names and none of them
    /// appears in `render_detail`.
    #[test]
    fn a_list_of_records_renders_one_per_line() {
        let mut out = String::new();
        let detail = serde_json::json!({
            "candidate_count": 11543.0,
            "rank": 9.0,
            "score": 7.718826072245981,
            "beaten_by": [
                { "who": "imdb:tt1235827", "by_how_much": 0.5 },
                { "who": "imdb:tt11736638", "by_how_much": 1.25 },
            ],
            "nothing_here": [],
        });
        render_detail(&mut out, detail.as_object().unwrap());

        assert!(out.contains("candidate_count  11543\n"), "got:\n{out}");
        assert!(out.contains("rank             9\n"), "got:\n{out}");
        assert!(out.contains("score            7.7188\n"), "got:\n{out}");
        assert!(out.contains("beaten_by:\n"), "got:\n{out}");
        assert!(
            out.contains("          - by_how_much=0.5  who=imdb:tt1235827\n"),
            "got:\n{out}",
        );
        assert!(out.contains("(none)"), "an empty list says so; got:\n{out}");
        assert!(
            !out.contains('{') && !out.contains('"'),
            "no raw JSON should survive into the report; got:\n{out}",
        );
    }

    fn plain_item(id: &str, start: OffsetDateTime, finish: OffsetDateTime) -> PlayoutItem {
        PlayoutItem {
            id: id.to_string(),
            start,
            finish,
            source: None,
            tracks: None,
            watermark: None,
            program: None,
            metadata: None,
        }
    }

    #[tokio::test]
    async fn a_boundary_straddling_item_written_into_both_chunks_is_not_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        // Chunk A: [00:00, 01:00), holding a straddling item that really
        // finishes at 01:30, re-emitted whole into chunk B per ADR 0003.
        let straddle_start = datetime!(2026-04-20 00:30 UTC);
        let straddle_finish = datetime!(2026-04-20 01:30 UTC);

        let a_items = vec![
            plain_item(
                "before",
                datetime!(2026-04-20 00:00 UTC),
                datetime!(2026-04-20 00:30 UTC),
            ),
            plain_item("straddler", straddle_start, straddle_finish),
        ];
        let name_a = crate::emit::chunk_filename(
            datetime!(2026-04-20 00:00 UTC),
            datetime!(2026-04-20 01:00 UTC),
        )
        .unwrap();
        tokio::fs::write(
            dir.path().join(name_a),
            serde_json::to_vec(&Playout::new(a_items)).unwrap(),
        )
        .await
        .unwrap();

        let b_items = vec![plain_item("straddler", straddle_start, straddle_finish)];
        let name_b = crate::emit::chunk_filename(
            datetime!(2026-04-20 01:00 UTC),
            datetime!(2026-04-20 02:00 UTC),
        )
        .unwrap();
        tokio::fs::write(
            dir.path().join(name_b),
            serde_json::to_vec(&Playout::new(b_items)).unwrap(),
        )
        .await
        .unwrap();

        let now = datetime!(2026-04-20 00:00 UTC);
        let items = upcoming(dir.path(), now, 10).await.unwrap();

        let straddler_count = items.iter().filter(|i| i.id == "straddler").count();
        assert_eq!(straddler_count, 1, "boundary item must not be duplicated");
        assert_eq!(items.len(), 2, "before + straddler, deduped");
    }
}
