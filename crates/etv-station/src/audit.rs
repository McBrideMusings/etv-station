//! Writing an audit-trail stage record from station code, for the producers
//! that are not a Rhai plugin.
//!
//! A `pool_provider` plugin explains itself in its own `audit()` function and
//! [`crate::score`] merges what it returns (ADR 0011). Nothing else did, so on
//! the 60 channels with no `plugin:` pool every item read `(unexplained — no
//! audit trail)` in the report — which is exactly backwards, since a channel
//! whose selection logic is *not* custom is the one a reader has the least
//! other way to reason about.
//!
//! This module is the station's own side of that contract: the same
//! `#{ stage, by, verdict, detail }` shape, the same closed
//! [`crate::score::KNOWN_STAGES`] set, written from the code that actually made
//! the decision rather than from a script.

use serde_json::{Map, Value};

/// Push one stage record onto `metadata`'s `audit` array, creating both if
/// this is the item's first stage.
///
/// `metadata` is the item's opaque blob — the same field a plugin pick's
/// `metadata` lands in — so an item drawn by a plugin pool and then acted on
/// by a built-in stage accumulates both, in the order they acted. That
/// ordering is the whole point of the trail being an array (ADR 0011): a pool
/// ranked it, a draw took it, a constraint kept or moved it, and each has
/// something different to say.
///
/// `stage` must be in [`crate::score::KNOWN_STAGES`]. It is not validated here
/// because every caller is station code passing a literal — unlike the plugin
/// path, where the name arrives from a script and is refused at pick time.
/// A `debug_assert` catches a typo in a test run without costing anything in
/// the daemon.
pub fn push_stage(
    metadata: &mut Option<Value>,
    stage: &'static str,
    by: String,
    verdict: String,
    detail: Option<Value>,
) {
    debug_assert!(
        crate::score::KNOWN_STAGES.contains(&stage),
        "stage {stage:?} is not one of {:?}",
        crate::score::KNOWN_STAGES,
    );

    let mut record = Map::new();
    record.insert("stage".into(), Value::String(stage.to_string()));
    record.insert("by".into(), Value::String(by));
    record.insert("verdict".into(), Value::String(verdict));
    if let Some(detail) = detail {
        record.insert("detail".into(), detail);
    }

    let blob = metadata.get_or_insert_with(|| Value::Object(Map::new()));
    // A non-object metadata blob is a plugin returning something odd, which
    // `score` already refuses at pick time. Reached here it would mean the
    // refusal was bypassed, so drop the record rather than panicking in the
    // generation loop — a missing audit line costs a line in a text file.
    let Some(obj) = blob.as_object_mut() else {
        return;
    };
    obj.entry("audit")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .expect("`audit` is always inserted as an array")
        .push(Value::Object(record));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_stage_creates_the_blob_and_the_array() {
        let mut metadata = None;
        push_stage(
            &mut metadata,
            "pool",
            "expr:shows".into(),
            "drawn".into(),
            None,
        );
        let trail = metadata.as_ref().unwrap()["audit"].as_array().unwrap();
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0]["stage"], "pool");
        assert_eq!(trail[0]["by"], "expr:shows");
    }

    /// The ordering ADR 0011 exists for: several mechanisms act on one item and
    /// each appends, so the array reads in the order they acted.
    #[test]
    fn stages_accumulate_in_the_order_they_acted() {
        let mut metadata = None;
        push_stage(&mut metadata, "pool", "a".into(), "first".into(), None);
        push_stage(&mut metadata, "pattern", "b".into(), "second".into(), None);
        let trail = metadata.as_ref().unwrap()["audit"].as_array().unwrap();
        assert_eq!(trail.len(), 2);
        assert_eq!(trail[0]["by"], "a");
        assert_eq!(trail[1]["by"], "b");
    }

    /// An item a plugin already attached metadata to keeps it — the audit array
    /// is a sibling key, never a replacement (ADR 0012's rule for `reason_set`
    /// applies to every other key too).
    #[test]
    fn an_existing_metadata_key_survives() {
        let mut metadata = Some(serde_json::json!({ "score": 0.82 }));
        push_stage(&mut metadata, "pool", "a".into(), "drawn".into(), None);
        let blob = metadata.as_ref().unwrap();
        assert_eq!(blob["score"], 0.82);
        assert_eq!(blob["audit"].as_array().unwrap().len(), 1);
    }
}
