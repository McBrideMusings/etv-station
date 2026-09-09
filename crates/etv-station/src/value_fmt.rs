//! A single `serde_json::Value` renderer shared by every diagnostic surface
//! that prints an opaque plugin-authored map — [`crate::audit_report`]'s
//! per-item audit trail and `taste-debug`'s per-candidate metadata dump.
//!
//! Both surfaces render `metadata`/`detail` maps a Rhai plugin attached, whose
//! keys neither surface knows in advance (ADR 0002: metadata is opaque to the
//! station). Before this module existed the two surfaces carried the same
//! function copied into each binary (etv-station-398); a fix to how a value
//! renders in one silently left the other rendering it the old way, which
//! matters here specifically because the two surfaces exist to be compared
//! against each other when diagnosing a channel.

/// Render one JSON value the way a human reads it, not the way `serde_json`
/// would serialize it.
pub fn format_value(v: &serde_json::Value) -> String {
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

    #[test]
    fn a_whole_number_float_renders_whole() {
        assert_eq!(format_value(&serde_json::json!(11543.0)), "11543");
        assert_eq!(format_value(&serde_json::json!(9.0)), "9");
    }

    #[test]
    fn a_genuine_fraction_keeps_four_decimals_trimmed() {
        assert_eq!(
            format_value(&serde_json::json!(7.718826072245981)),
            "7.7188"
        );
        assert_eq!(format_value(&serde_json::json!(0.5)), "0.5");
    }

    #[test]
    fn a_string_renders_with_no_quotes() {
        assert_eq!(format_value(&serde_json::json!("picked")), "picked");
    }

    #[test]
    fn an_array_renders_each_element_recursively() {
        assert_eq!(
            format_value(&serde_json::json!([1.0, "x", 2.5])),
            "[1, x, 2.5]"
        );
    }
}
