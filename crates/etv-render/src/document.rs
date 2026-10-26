//! A plan file: either a chapter plan or a set plan.
//!
//! A set plan names itself with `"kind": "set"`. A file without a `kind` is a
//! chapter plan, the format plugins have always written, so those files and
//! their job ids stay as they were.

use std::path::Path;

use serde::{Serialize, Serializer};

use crate::plan::{Plan, PlanError};
use crate::set::SetPlan;

#[derive(Debug, Clone, PartialEq)]
pub enum Document {
    Chapter(Plan),
    Set(SetPlan),
}

impl From<Plan> for Document {
    fn from(plan: Plan) -> Self {
        Document::Chapter(plan)
    }
}

impl From<SetPlan> for Document {
    fn from(plan: SetPlan) -> Self {
        Document::Set(plan)
    }
}

impl Serialize for Document {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Document::Chapter(plan) => plan.serialize(serializer),
            Document::Set(plan) => plan.serialize(serializer),
        }
    }
}

impl Document {
    pub fn parse(json: &str) -> Result<Self, PlanError> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        match value.get("kind") {
            None => Plan::parse(json).map(Document::Chapter),
            Some(kind) if kind.as_str() == Some("set") => SetPlan::parse(json).map(Document::Set),
            Some(kind) => Err(PlanError::Invalid(format!(
                "unknown plan kind {kind}: a plan is a chapter plan (no kind) or has kind \"set\""
            ))),
        }
    }

    pub fn validate(&self) -> Result<(), PlanError> {
        match self {
            Document::Chapter(plan) => plan.validate(),
            Document::Set(plan) => plan.validate(),
        }
    }

    pub fn duration_s(&self) -> f64 {
        match self {
            Document::Chapter(plan) => plan.duration_s(),
            Document::Set(plan) => plan.duration_s(),
        }
    }

    pub fn output(&self) -> &Path {
        match self {
            Document::Chapter(plan) => &plan.output,
            Document::Set(plan) => &plan.output,
        }
    }

    /// `chapter` or `set`, for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Document::Chapter(_) => "chapter",
            Document::Set(_) => "set",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SET: &str = r#"{
        "kind": "set",
        "tracks": [
            {"entry_id": "a", "path": "/m/a.mp4", "native_bpm": 120.0, "enter_s": 16.0, "ratio": 1.0},
            {"entry_id": "b", "path": "/m/b.mp4", "native_bpm": 120.0, "enter_s": 0.0, "ratio": 1.0}
        ],
        "seams": [{"recipe": "drop_mix", "out_s": 96.0}],
        "end_s": 60.0,
        "output": "/o.mp4"
    }"#;

    #[test]
    fn a_plan_with_kind_set_is_a_set_plan() {
        let doc = Document::parse(SET).unwrap();
        assert_eq!(doc.kind(), "set");
        // 80 s of a, 60 s of b.
        assert!((doc.duration_s() - 140.0).abs() < 1e-9);
        assert_eq!(doc.output(), Path::new("/o.mp4"));
    }

    #[test]
    fn a_plan_without_a_kind_is_a_chapter_plan() {
        let text = include_str!("../tests/fixtures/chapter.plan.json").replace("@DIR@", "/d");
        assert_eq!(Document::parse(&text).unwrap().kind(), "chapter");
    }

    #[test]
    fn an_unknown_kind_is_refused_by_name() {
        let err = Document::parse(r#"{"kind": "playlist"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown plan kind"), "{err}");
    }

    #[test]
    fn a_chapter_plan_serializes_as_it_always_did() {
        let text = include_str!("../tests/fixtures/chapter.plan.json").replace("@DIR@", "/d");
        let plan = Plan::parse(&text).unwrap();
        let direct = serde_json::to_string(&plan).unwrap();
        let wrapped = serde_json::to_string(&Document::from(plan)).unwrap();
        assert_eq!(direct, wrapped);
    }
}
