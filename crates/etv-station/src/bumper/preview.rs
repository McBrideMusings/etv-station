//! Resolve one bumper script on one gap without a station: the engine behind
//! `etv-station bumper-preview` and `admin bumper-watch`.
//!
//! It runs the same three steps the generator will — [`hook::build`],
//! [`timeline::fit`], [`emit::emit_timeline`] — so what it prints is what the
//! scheduler would write for that gap. Unlike the generator it does not fall
//! back to black when the script fails: a preview that hides a script error
//! behind a black card would show the author nothing.

use std::path::{Path, PathBuf};

use rhai::{Dynamic, Map};
use time::OffsetDateTime;

use crate::catalog::Catalog;
use ersatztv_playout::playout::PlayoutItem;

use super::emit::{EmitContext, emit_timeline, otio_json};
use super::hook::{self, BumperInput, ChannelInfo};
use super::timeline::{ResolvedTimeline, fit};

/// What to preview.
pub struct PreviewRequest<'a> {
    pub script: &'a Path,
    pub gap_ms: u64,
    pub fps: f64,
    /// The item after the gap; `None` previews a frontier gap.
    pub next: Option<Map>,
    pub channel: ChannelInfo,
    /// Directory a relative overlay script path is resolved against.
    pub overlay_root: &'a Path,
    pub start: OffsetDateTime,
}

/// A resolved gap: the timeline the solver fitted and the items it emits.
pub struct Preview {
    pub resolved: ResolvedTimeline,
    pub items: Vec<PlayoutItem>,
    pub fps: f64,
}

impl Preview {
    /// The items as one pretty-printed JSON array.
    pub fn items_json(&self) -> String {
        serde_json::to_string_pretty(&self.items).expect("playout items serialize")
    }

    /// The items as one JSON object per line.
    pub fn items_ndjson(&self) -> String {
        self.items
            .iter()
            .map(|i| serde_json::to_string(i).expect("playout item serializes"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn otio(&self) -> String {
        serde_json::to_string_pretty(&otio_json(&self.resolved, self.fps))
            .expect("otio document serializes")
    }
}

/// Build, fit and emit `req.script` for `req.gap_ms`.
///
/// The script is granted exactly the capabilities it declares, as a channel
/// that granted them all would.
pub fn preview(req: &PreviewRequest<'_>) -> Result<Preview, String> {
    let granted: Vec<String> = crate::score::declared_capabilities(req.script, "bumper")?
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    hook::validate_script(req.script, &granted)?;
    let air_date = req
        .start
        .date()
        .format(&time::macros::format_description!("[year]-[month]-[day]"))
        .map_err(|e| e.to_string())?;
    let built = hook::build(
        req.script,
        &BumperInput {
            gap_ms: req.gap_ms,
            next: req.next.clone(),
            prev: None,
            channel: req.channel.clone(),
            air_date,
            draw: 0,
            granted,
            history: None,
        },
    )
    .map_err(|e| e.to_string())?;
    let resolved = fit(&built.timeline, req.gap_ms, req.fps).map_err(|e| e.to_string())?;
    let items = emit_timeline(
        &resolved,
        &EmitContext {
            channel: &req.channel.name,
            fps: req.fps,
            overlay_root: req.overlay_root,
        },
        req.start,
    );
    tracing::info!(
        event = "bumper.preview",
        channel = %req.channel.name,
        script = %req.script.display(),
        gap_ms = req.gap_ms,
        segments = resolved.segments.len(),
        items = items.len(),
        dropped = ?resolved.dropped,
        "resolved a bumper preview"
    );
    Ok(Preview {
        resolved,
        items,
        fps: req.fps,
    })
}

/// `45s`, `1500ms`, `2m 30s`: a gap length.
pub fn parse_gap(raw: &str) -> Result<u64, String> {
    let d = humantime::parse_duration(raw).map_err(|e| format!("gap {raw:?}: {e}"))?;
    let ms = d.as_millis() as u64;
    if ms == 0 {
        return Err(format!("gap {raw:?} is zero"));
    }
    Ok(ms)
}

/// The item map for `--next`: a media file on disk (stands in for a catalog
/// entry), or a catalog `entry_id` read from `catalog`.
pub fn next_item(spec: &str, catalog: Option<&Catalog>) -> Result<Map, String> {
    let path = PathBuf::from(spec);
    if path.is_file() {
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut m = Map::new();
        m.insert("entry_id".into(), spec.to_string().into());
        m.insert("title".into(), title.into());
        m.insert("path".into(), spec.to_string().into());
        return Ok(m);
    }
    let Some(catalog) = catalog else {
        return Err(format!(
            "--next {spec:?} is not a file, and no catalog is configured to look it up as an entry_id \
             (pass --config with a catalog_path)"
        ));
    };
    let items = crate::score::load_items(catalog, &[spec.to_string()])?;
    items
        .into_iter()
        .next()
        .and_then(|d: Dynamic| d.try_cast::<Map>())
        .ok_or_else(|| format!("no catalog entry {spec:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_script(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("b.rhai");
        std::fs::write(&p, body).unwrap();
        p
    }

    fn request<'a>(script: &'a Path, root: &'a Path, gap_ms: u64) -> PreviewRequest<'a> {
        PreviewRequest {
            script,
            gap_ms,
            fps: 30.0,
            next: None,
            channel: ChannelInfo {
                name: "preview".into(),
                number: 7,
                ..ChannelInfo::default()
            },
            overlay_root: root,
            start: OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
        }
    }

    const CARD: &str = r#"
fn hooks() { ["bumper"] }
fn build(ctx) {
    let t = timeline();
    t.push(segment().black().fixed(2.0));
    t.push(segment().black().overlay("card.rhai", #{ title: "x" }).flex(3.0, 20.0, 1).looped());
    t
}
"#;

    #[test]
    fn a_gap_resolves_to_items_that_sum_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), CARD);
        let p = preview(&request(&script, dir.path(), 45_000)).unwrap();
        assert_eq!(p.items.len(), 2);
        let first = p.items.first().unwrap();
        let last = p.items.last().unwrap();
        assert_eq!((last.finish - first.start).whole_milliseconds(), 45_000);
        assert_eq!(p.resolved.total_ms(), 45_000);
    }

    #[test]
    fn otio_names_every_segment() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), CARD);
        let p = preview(&request(&script, dir.path(), 45_000)).unwrap();
        let otio: serde_json::Value = serde_json::from_str(&p.otio()).unwrap();
        assert_eq!(otio["OTIO_SCHEMA"], "Timeline.1");
        assert_eq!(
            otio["tracks"]["children"][0]["children"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn a_script_error_is_returned_not_painted_black() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            r#"fn hooks() { ["bumper"] } fn build(ctx) { throw "boom"; }"#,
        );
        let err = preview(&request(&script, dir.path(), 10_000))
            .err()
            .unwrap();
        assert!(err.contains("boom"), "got {err}");
    }
}
