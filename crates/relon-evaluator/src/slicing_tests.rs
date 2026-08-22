//! Tree-walk oracle coverage for the pure `std/list` slicing helpers
//! `take` / `slice` / `zip`.
//!
//! These are authored entirely in `std_relon/list.relon` over `range` +
//! index + comprehension + tuple — there is no backing intrinsic. They
//! are total (never raise): bounds clamp into `[0, len]` before any
//! index is taken, and `range(lo, hi)` with `lo >= hi` is the empty
//! list. The cases here pin those clamp / truncation semantics directly
//! at the evaluator, independent of the docs-golden pipeline, and
//! confirm the wrappers dispatch through the imported `std/list` module.

use crate::{Context, RuntimeError, Scope, TreeWalkEvaluator, Value};
use std::sync::Arc;

fn tree_walk_result(source: &str) -> Result<Value, RuntimeError> {
    let node = relon_parser::parse_document(source).expect("fixture must parse");
    let mut ctx = Context::new().with_root(node);
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let ctx = Arc::new(ctx);
    TreeWalkEvaluator::new(Arc::clone(&ctx)).eval_root(&Arc::new(Scope::default()))
}

fn tree_walk(source: &str) -> Value {
    tree_walk_result(source).expect("fixture must evaluate")
}

/// Evaluate a `std/list` expression (wrapped in a one-field document so
/// the `#import` has a braced root to attach to) and return its JSON
/// projection. `Tuple` serializes untagged, identically to a `List`, so
/// `zip` results appear as nested JSON arrays.
fn list_json(expr: &str) -> serde_json::Value {
    let src = format!("#import list from \"std/list\"\n{{ r: {expr} }}");
    let Value::Dict(dict) = tree_walk(&src) else {
        panic!("wrapper document must evaluate to a dict");
    };
    serde_json::to_value(dict.map.get("r").expect("field r")).expect("result serializes to JSON")
}

// ---- take -----------------------------------------------------------------

#[test]
fn take_prefix_and_clamps() {
    // n in (0, len): the prefix.
    assert_eq!(
        list_json("list.take([1, 2, 3, 4], 2)"),
        serde_json::json!([1, 2])
    );
    // n == 0 and n < 0 both yield the empty list (never error).
    assert_eq!(
        list_json("list.take([1, 2, 3, 4], 0)"),
        serde_json::json!([])
    );
    assert_eq!(
        list_json("list.take([1, 2, 3, 4], -5)"),
        serde_json::json!([])
    );
    // n >= len copies the whole list.
    assert_eq!(
        list_json("list.take([1, 2, 3, 4], 4)"),
        serde_json::json!([1, 2, 3, 4])
    );
    assert_eq!(
        list_json("list.take([1, 2, 3, 4], 99)"),
        serde_json::json!([1, 2, 3, 4])
    );
    // Empty receiver is total for any n.
    assert_eq!(list_json("list.take([], 3)"), serde_json::json!([]));
}

// ---- slice ----------------------------------------------------------------

#[test]
fn slice_clamps_both_bounds() {
    // Interior half-open window.
    assert_eq!(
        list_json("list.slice([1, 2, 3, 4], 1, 3)"),
        serde_json::json!([2, 3])
    );
    // end past len clamps to len.
    assert_eq!(
        list_json("list.slice([1, 2, 3, 4], 2, 99)"),
        serde_json::json!([3, 4])
    );
    // Negative start clamps to 0.
    assert_eq!(
        list_json("list.slice([1, 2, 3, 4], -1, 2)"),
        serde_json::json!([1, 2])
    );
    // Negative end clamps to 0, so start >= end -> [].
    assert_eq!(
        list_json("list.slice([1, 2, 3, 4], 0, -1)"),
        serde_json::json!([])
    );
    // Inverted (start > end after clamp) -> [].
    assert_eq!(
        list_json("list.slice([1, 2, 3, 4], 3, 1)"),
        serde_json::json!([])
    );
    // Equal bounds -> [] (half-open).
    assert_eq!(
        list_json("list.slice([1, 2, 3, 4], 2, 2)"),
        serde_json::json!([])
    );
    // Full span.
    assert_eq!(
        list_json("list.slice([1, 2, 3, 4], 0, 4)"),
        serde_json::json!([1, 2, 3, 4])
    );
}

// ---- zip ------------------------------------------------------------------

#[test]
fn zip_truncates_to_shorter_side() {
    // Equal length.
    assert_eq!(
        list_json(r#"list.zip([1, 2], ["x", "y"])"#),
        serde_json::json!([[1, "x"], [2, "y"]])
    );
    // Left longer -> truncate to the right.
    assert_eq!(
        list_json(r#"list.zip([1, 2, 3], ["a", "b"])"#),
        serde_json::json!([[1, "a"], [2, "b"]])
    );
    // Right longer -> truncate to the left.
    assert_eq!(
        list_json(r#"list.zip([1], ["a", "b", "c"])"#),
        serde_json::json!([[1, "a"]])
    );
    // Either side empty -> [].
    assert_eq!(list_json("list.zip([], [1, 2])"), serde_json::json!([]));
    assert_eq!(list_json("list.zip([1, 2], [])"), serde_json::json!([]));
}

#[test]
fn zip_pairs_are_tuples_not_lists() {
    // The comprehension mints `(a[i], b[i])` tuple literals, so each
    // pair is a `Value::Tuple` (distinct from a `List` at the value
    // level even though both serialize untagged to a JSON array).
    let src = "#import list from \"std/list\"\n{ r: list.zip([1, 2], [3, 4]) }";
    let Value::Dict(dict) = tree_walk(src) else {
        panic!("wrapper document must evaluate to a dict");
    };
    let Value::List(pairs) = dict.map.get("r").expect("field r") else {
        panic!("zip must evaluate to a list");
    };
    assert_eq!(pairs.len(), 2);
    for pair in pairs.iter() {
        assert!(
            matches!(pair, Value::Tuple(_)),
            "each zip pair must be a Tuple, got {pair:?}"
        );
    }
}
