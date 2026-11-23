//! CEL evaluation over named values: the one place an expression from config is
//! compiled and run against variables. A fill entry's `when` calls it from Rust;
//! a script calls it as `cel(expr, vars)` and `cel_check(expr)`.

use rhai::{Dynamic, EvalAltResult, Map};

/// Compile `src`, or say why it is not valid CEL.
pub fn check(src: &str) -> Result<(), String> {
    cel::Program::compile(src)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A JSON value as CEL sees it. A whole number is an `int` (a `uint` only past
/// i64), so `n * 2` and `n < 3` work on the numbers config and scripts hand over;
/// serde would make every non-negative one a `uint`, which CEL refuses to mix.
fn to_cel(value: serde_json::Value) -> cel::Value {
    use serde_json::Value as J;
    use std::sync::Arc;
    match value {
        J::Null => cel::Value::Null,
        J::Bool(b) => cel::Value::Bool(b),
        J::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => cel::Value::Int(i),
            (None, Some(u)) => cel::Value::UInt(u),
            _ => cel::Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        J::String(s) => cel::Value::String(Arc::new(s)),
        J::Array(items) => cel::Value::List(Arc::new(items.into_iter().map(to_cel).collect())),
        J::Object(fields) => cel::Value::Map(cel::objects::Map {
            map: Arc::new(
                fields
                    .into_iter()
                    .map(|(k, v)| (cel::objects::Key::String(Arc::new(k)), to_cel(v)))
                    .collect(),
            ),
        }),
    }
}

/// Run `src` with each `(name, value)` bound as a variable.
pub fn eval(src: &str, vars: Vec<(String, serde_json::Value)>) -> Result<cel::Value, String> {
    let program = cel::Program::compile(src).map_err(|e| e.to_string())?;
    let mut ctx = cel::Context::default();
    for (name, value) in vars {
        ctx.add_variable_from_value(name, to_cel(value));
    }
    program.execute(&ctx).map_err(|e| e.to_string())
}

/// Run `src` and require a bool.
pub fn eval_bool(src: &str, vars: Vec<(String, serde_json::Value)>) -> Result<bool, String> {
    match eval(src, vars)? {
        cel::Value::Bool(b) => Ok(b),
        other => Err(format!("yielded {other:?}, not a bool")),
    }
}

fn script_error(msg: String) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(msg.into(), rhai::Position::NONE))
}

/// The script's `cel(expr, vars)`: each key of `vars` is a variable; the result
/// comes back as a script value. A compile or run error is thrown.
pub fn script_eval(src: &str, vars: Map) -> Result<Dynamic, Box<EvalAltResult>> {
    let mut bound = Vec::with_capacity(vars.len());
    for (name, value) in vars {
        let json: serde_json::Value = rhai::serde::from_dynamic(&value)?;
        bound.push((name.to_string(), json));
    }
    let value = eval(src, bound).map_err(|e| script_error(format!("cel `{src}`: {e}")))?;
    let json = value
        .json()
        .map_err(|e| script_error(format!("cel `{src}`: result is not JSON: {e:?}")))?;
    rhai::serde::to_dynamic(json)
}

/// The script's `cel_check(expr)`: throws when `expr` does not compile.
pub fn script_check(src: &str) -> Result<(), Box<EvalAltResult>> {
    check(src).map_err(|e| script_error(format!("cel `{src}` is not valid CEL: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(src: &str, vars: serde_json::Value) -> Result<serde_json::Value, String> {
        let engine = crate::score::engine();
        let mut scope = rhai::Scope::new();
        scope.push("vars", rhai::serde::to_dynamic(vars).unwrap());
        engine
            .eval_with_scope::<Dynamic>(&mut scope, &format!("cel({src:?}, vars)"))
            .map(|d| rhai::serde::from_dynamic(&d).unwrap())
            .map_err(|e| e.to_string())
    }

    #[test]
    fn a_script_reads_nested_values_and_gets_numbers_and_bools_back() {
        let vars = json!({ "seam": { "gap": 9.5, "n": 3 }, "b": { "genres": ["Pop", "Rock"] } });
        assert_eq!(run("seam.gap > 8.0 && b.genres.exists(g, g == \"Pop\")", vars.clone()).unwrap(), json!(true));
        assert_eq!(run("seam.n * 2", vars.clone()).unwrap(), json!(6));
        // An int against a float literal compares as numbers.
        assert_eq!(run("seam.n < 3.5", vars.clone()).unwrap(), json!(true));
        assert_eq!(run("has(b.mood)", vars).unwrap(), json!(false));
    }

    #[test]
    fn a_missing_field_and_a_syntax_error_throw_naming_the_expression() {
        let err = run("b.mood == \"calm\"", json!({ "b": {} })).unwrap_err();
        assert!(err.contains("b.mood"), "{err}");
        let err = run("seam.gap >", json!({})).unwrap_err();
        assert!(err.contains("seam.gap >"), "{err}");
        assert!(check("seam.gap >").is_err());
        assert!(check("seam.gap > 8.0").is_ok());
    }
}
