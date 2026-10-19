//! Annotate hook (ADR 0017) — a channel names an `annotate:` script that
//! attaches metadata to items on the already-built schedule without selecting
//! or ordering anything.
//!
//! ```rhai
//! fn hooks() { ["annotate"] }
//! fn capabilities() { ["http"] }   // optional; the channel must grant it too
//!
//! // Called once per item, in schedule order. Returns a map of metadata keys
//! // to add, or () to add nothing.
//! fn annotate(ctx, item) {
//!     // ctx.channel — the channel's name
//!     // ctx.http(url, method, body) — only with the `http` capability
//!     // item        — the catalog item map ([`crate::score::load_items`]),
//!     //               or #{ entry_id } for an inline item, plus `block`
//!     //               (the `rule.blocks` index) and `metadata` (what the
//!     //               item already carries, or ())
//!     #{ rating: 8 }
//! }
//! ```
//!
//! The returned map merges into the item's existing metadata key by key. A key
//! the item already carries keeps its value, so a `pool_provider` plugin's own
//! `pick()` metadata is never overwritten. A script that fails leaves the item
//! as it was and logs a warning: the station keeps airing without the
//! annotation instead of going dark over it.

use std::path::{Path, PathBuf};

use etv_overlay::config_carrier;
use rhai::serde::DynamicDeserializer;
use rhai::{Dynamic, EvalAltResult, Map, Scope};

use crate::catalog::Catalog;
use crate::resolve::ResolvedItem;

/// Capability names an annotate plugin may declare. The channel's
/// `annotate.capabilities` must grant exactly the declared set (ADR 0015).
pub const ANNOTATE_CAPABILITIES: &[&str] = &["http"];

/// Run `script`'s `annotate(ctx, item)` over every item and merge each
/// returned patch into that item's metadata. `http` is whether the channel
/// granted that capability; config load has already proven the grant equals
/// what the script declares.
pub fn annotate_items(
    script: &Path,
    http: bool,
    channel: &str,
    items: &mut [ResolvedItem],
    catalog: Option<&Catalog>,
) {
    let ast = match crate::score::compile_plugin(script) {
        Ok(ast) => ast,
        Err(e) => {
            tracing::warn!(
                event = "annotate.failed",
                channel,
                script = %script.display(),
                error = %e,
                "annotate script did not compile; items keep the metadata they had"
            );
            return;
        }
    };
    // One budget for the whole pass, not one per item: a schedule of N items
    // must not be able to hold generation for N times the per-call cap.
    let budget = crate::plugin_http::Budget::new(crate::plugin_http::GENERATION_BUDGET);
    let mut engine = crate::score::engine();
    engine.register_fn(
        "http",
        move |_ctx: &mut Map,
              url: &str,
              method: &str,
              body: &str|
              -> Result<String, Box<EvalAltResult>> {
            if !http {
                return Err("capability `http` not declared".into());
            }
            Ok(budget.within("ctx.http()", |timeout| {
                crate::plugin_http::call(url, method, body, timeout)
            })?)
        },
    );

    let mut ctx = Map::new();
    ctx.insert("channel".into(), channel.to_string().into());
    let ctx = Dynamic::from_map(ctx);

    let catalog_items = catalog_item_maps(channel, items, catalog);
    let mut annotated = 0usize;
    for item in items.iter_mut() {
        let arg = item_map(item, catalog_items.get(&item.id).cloned());
        let mut scope = Scope::new();
        let reply: Dynamic = match engine.call_fn(&mut scope, &ast, "annotate", (ctx.clone(), arg))
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    event = "annotate.failed",
                    channel,
                    script = %script.display(),
                    entry_id = %item.id,
                    error = %e,
                    "annotate() failed; this item keeps the metadata it had"
                );
                continue;
            }
        };
        match patch_from(reply) {
            Ok(None) => {}
            Ok(Some(patch)) => {
                let sent = patch.len();
                let collided = merge_patch(&mut item.metadata, patch);
                if collided.len() < sent {
                    annotated += 1;
                }
                for key in &collided {
                    tracing::warn!(
                        event = "annotate.key_collision",
                        channel,
                        entry_id = %item.id,
                        key = %key,
                        "annotate() returned a key the item already carries; the existing value stays"
                    );
                }
            }
            Err(e) => tracing::warn!(
                event = "annotate.failed",
                channel,
                script = %script.display(),
                entry_id = %item.id,
                error = %e,
                "annotate() returned an unusable value; this item keeps the metadata it had"
            ),
        }
    }
    tracing::info!(
        event = "annotate.done",
        channel,
        script = %script.display(),
        items = items.len(),
        annotated,
        "annotate hook ran over the schedule"
    );
}

/// The catalog's item map for every resolved id the catalog knows, read in
/// bulk ([`crate::score::load_items`] is written for that). An inline item has
/// no catalog row and gets the bare map from [`item_map`].
fn catalog_item_maps(
    channel: &str,
    items: &[ResolvedItem],
    catalog: Option<&Catalog>,
) -> std::collections::HashMap<String, Map> {
    let Some(catalog) = catalog else {
        return Default::default();
    };
    let mut ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
    ids.sort();
    ids.dedup();
    let loaded = catalog
        .entries_for(&ids)
        .map_err(|e| e.to_string())
        .and_then(|found| {
            ids.retain(|id| found.contains_key(id));
            crate::score::load_items(catalog, &ids)
        });
    match loaded {
        Ok(maps) => maps
            .into_iter()
            .filter_map(|d| d.try_cast::<Map>())
            .filter_map(|m| {
                let id = m.get("entry_id")?.clone().into_string().ok()?;
                Some((id, m))
            })
            .collect(),
        Err(e) => {
            tracing::warn!(
                event = "annotate.catalog_failed",
                channel,
                error = %e,
                "reading catalog rows for annotate failed; items reach the script as bare entry_id maps"
            );
            Default::default()
        }
    }
}

fn item_map(item: &ResolvedItem, from_catalog: Option<Map>) -> Dynamic {
    let mut map = from_catalog.unwrap_or_else(|| {
        let mut m = Map::new();
        m.insert("entry_id".into(), item.id.clone().into());
        m
    });
    map.insert("block".into(), (item.block as i64).into());
    let existing = item
        .metadata
        .as_ref()
        .and_then(|v| rhai::serde::to_dynamic(v).ok())
        .unwrap_or(Dynamic::UNIT);
    map.insert("metadata".into(), existing);
    Dynamic::from_map(map)
}

/// The patch an `annotate()` return value names: `()` is none, a map is the
/// patch, anything else is refused.
fn patch_from(
    value: Dynamic,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, String> {
    if value.is_unit() {
        return Ok(None);
    }
    if !value.is_map() {
        return Err(format!(
            "annotate() must return a map or (), got {}",
            value.type_name()
        ));
    }
    let json = config_carrier::deserialize_config(DynamicDeserializer::new(&value))
        .map_err(|e| format!("annotate() metadata: {e}"))?;
    match json {
        Some(serde_json::Value::Object(map)) => Ok(Some(map)),
        _ => Ok(None),
    }
}

/// Merge `patch` into `existing`, key by key, keeping any value already
/// there. Returns the keys that collided. When the item's metadata is not an
/// object, nothing can merge into it and every patch key is reported as
/// collided.
fn merge_patch(
    existing: &mut Option<serde_json::Value>,
    patch: serde_json::Map<String, serde_json::Value>,
) -> Vec<String> {
    if patch.is_empty() {
        return Vec::new();
    }
    match existing {
        None => {
            *existing = Some(serde_json::Value::Object(patch));
            Vec::new()
        }
        Some(serde_json::Value::Object(have)) => {
            let mut collided = Vec::new();
            for (k, v) in patch {
                if have.contains_key(&k) {
                    collided.push(k);
                } else {
                    have.insert(k, v);
                }
            }
            collided
        }
        Some(_) => patch.into_iter().map(|(k, _)| k).collect(),
    }
}

/// `script` resolved against the channel config's directory.
pub fn script_path(config_path: &Path, annotate: &Path) -> PathBuf {
    let base_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    crate::score::resolve_plugin_path(base_dir, annotate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceConfig;
    use serde_json::json;
    use std::time::Duration;

    fn lavfi(id: &str, metadata: Option<serde_json::Value>) -> ResolvedItem {
        ResolvedItem {
            block: 2,
            id: id.into(),
            source: SourceConfig::Lavfi {
                params: format!("src={id}"),
            },
            in_point: Some(Duration::ZERO),
            out_point: Some(Duration::from_secs(5)),
            program: None,
            catalog_duration: None,
            error_card: false,
            metadata,
            guide: None,
            guide_fields: crate::guide::GuideFields::default(),
            tracks: None,
            selected: None,
            bumper: None,
        }
    }

    fn write(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let p = dir.path().join("annotate.rhai");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn a_patch_lands_on_an_item_with_no_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"fn hooks() { ["annotate"] }
fn annotate(ctx, item) { #{ who: ctx.channel + ":" + item.entry_id, block: item.block } }"#,
        );
        let mut items = vec![lavfi("a", None), lavfi("b", None)];
        annotate_items(&script, false, "ch", &mut items, None);
        assert_eq!(items[0].metadata, Some(json!({"who": "ch:a", "block": 2})));
        assert_eq!(items[1].metadata, Some(json!({"who": "ch:b", "block": 2})));
    }

    #[test]
    fn a_patch_adds_to_a_pool_providers_metadata_without_clobbering_it() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"fn hooks() { ["annotate"] }
fn annotate(ctx, item) { #{ reason: "annotated", rating: 7 } }"#,
        );
        let mut items = vec![lavfi("a", Some(json!({"reason": "won an Oscar"})))];
        annotate_items(&script, false, "ch", &mut items, None);
        assert_eq!(
            items[0].metadata,
            Some(json!({"reason": "won an Oscar", "rating": 7}))
        );
    }

    #[test]
    fn a_colliding_key_keeps_the_existing_value_and_a_non_object_takes_no_patch() {
        let mut have = Some(json!({"a": 1}));
        let patch = json!({"a": 2, "b": 3}).as_object().unwrap().clone();
        assert_eq!(merge_patch(&mut have, patch), vec!["a".to_string()]);
        assert_eq!(have, Some(json!({"a": 1, "b": 3})));

        let mut scalar = Some(json!("text"));
        let patch = json!({"a": 2}).as_object().unwrap().clone();
        assert_eq!(merge_patch(&mut scalar, patch), vec!["a".to_string()]);
        assert_eq!(scalar, Some(json!("text")));
    }

    #[test]
    fn the_item_sees_the_metadata_it_already_carries() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"fn hooks() { ["annotate"] }
fn annotate(ctx, item) { #{ seen: item.metadata.reason } }"#,
        );
        let mut items = vec![lavfi("a", Some(json!({"reason": "x"})))];
        annotate_items(&script, false, "ch", &mut items, None);
        assert_eq!(items[0].metadata, Some(json!({"reason": "x", "seen": "x"})));
    }

    #[test]
    fn unit_adds_nothing_and_a_throwing_item_keeps_what_it_had() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"fn hooks() { ["annotate"] }
fn annotate(ctx, item) {
    if item.entry_id == "boom" { throw "no" }
    if item.entry_id == "skip" { return (); }
    #{ ok: true }
}"#,
        );
        let mut items = vec![
            lavfi("boom", None),
            lavfi("skip", None),
            lavfi("fine", None),
        ];
        annotate_items(&script, false, "ch", &mut items, None);
        assert_eq!(items[0].metadata, None);
        assert_eq!(items[1].metadata, None);
        assert_eq!(items[2].metadata, Some(json!({"ok": true})));
    }

    #[test]
    fn http_needs_the_capability_declared() {
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            r#"fn hooks() { ["annotate"] }
fn annotate(ctx, item) { #{ body: ctx.http("http://127.0.0.1:9/", "GET", "") } }"#,
        );
        let mut items = vec![lavfi("a", None)];
        annotate_items(&script, false, "ch", &mut items, None);
        assert_eq!(items[0].metadata, None);
    }

    #[test]
    fn http_with_the_capability_reaches_out_and_parse_json_reads_it() {
        let (url, server) = crate::plugin_http::tests::serve_once("200 OK", r#"{"stars":4}"#);
        let dir = tempfile::tempdir().unwrap();
        let script = write(
            &dir,
            &format!(
                r#"fn hooks() {{ ["annotate"] }}
fn capabilities() {{ ["http"] }}
fn annotate(ctx, item) {{ #{{ stars: parse_json(ctx.http("{url}/r", "GET", "")).stars }} }}"#
            ),
        );
        let mut items = vec![lavfi("a", None)];
        annotate_items(&script, true, "ch", &mut items, None);
        server.join().unwrap();
        assert_eq!(items[0].metadata, Some(json!({"stars": 4})));
    }
}
