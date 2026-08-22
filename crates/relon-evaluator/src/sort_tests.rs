//! Tree-walk oracle coverage for the `std/list` sort family
//! (`_list_sort_by` / `_list_sort_by_desc` and their `sort` /
//! `sort_by` / `sort_desc` / `sort_by_desc` wrappers).
//!
//! These pin the reference (tree-walk) semantics defined in the sort
//! contract: stable bottom-up merge sort, the total preorder on keys
//! (Int / Bool / String / Float with `-0.0 == +0.0` and `NaN` after
//! `+inf`), stability-driven tie behaviour (`sort_desc` is NOT
//! `reverse(sort(...))`), and loud errors on mixed / non-orderable
//! keys. The compiled backends must byte-match every case here for the
//! Int / Float element lists; String / Bool are the tree-walk-only cap.
//!
//! All cases run through the tree-walker directly (parse + eval,
//! bypassing the analyzer) so the fixtures can use `sqrt(-1.0)` to
//! mint a `NaN` and raw nested lists as decorated `[key, tag]` pairs.

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

/// Run a sort over decorated `[key, tag]` pairs and return the `tag`
/// column in sorted order. Lets a test observe stability / tie order /
/// float-key semantics without the (JSON-hostile) key values leaking
/// into the assertion.
fn sorted_tags(src: &str) -> Vec<String> {
    let Value::List(items) = tree_walk(src) else {
        panic!("sort fixture must evaluate to a list: {src}");
    };
    items
        .iter()
        .map(|item| {
            let Value::List(pair) = item else {
                panic!("each element must be a [key, tag] pair: {src}");
            };
            match &pair[1] {
                Value::String(s) => s.to_string(),
                other => panic!("tag must be a string, got {other:?}"),
            }
        })
        .collect()
}

/// Run a sort of plain scalars and return the serialized JSON. Only for
/// JSON-safe element types (Int / String / Bool — never a bare `NaN`).
fn sorted_json(src: &str) -> serde_json::Value {
    serde_json::to_value(tree_walk(src)).expect("sorted result serializes to JSON")
}

#[test]
fn sorts_ints_ascending_and_descending() {
    assert_eq!(
        sorted_json("_list_sort_by([3, 1, 2], (x) => x)"),
        serde_json::json!([1, 2, 3])
    );
    assert_eq!(
        sorted_json("_list_sort_by_desc([1, 3, 2], (x) => x)"),
        serde_json::json!([3, 2, 1])
    );
    // Already-sorted and reverse-sorted inputs round-trip.
    assert_eq!(
        sorted_json("_list_sort_by([1, 2, 3], (x) => x)"),
        serde_json::json!([1, 2, 3])
    );
    assert_eq!(
        sorted_json("_list_sort_by([3, 2, 1], (x) => x)"),
        serde_json::json!([1, 2, 3])
    );
}

#[test]
fn sorts_strings_by_code_point() {
    // Code-point order, NOT case-insensitive: 'A'=0x41 < 'Z'=0x5A <
    // 'a'=0x61 < 'b'=0x62, so uppercase sorts before lowercase.
    assert_eq!(
        sorted_json(r#"_list_sort_by(["a", "Z", "b", "A"], (x) => x)"#),
        serde_json::json!(["A", "Z", "a", "b"])
    );
}

#[test]
fn sorts_bools_false_before_true() {
    assert_eq!(
        sorted_json("_list_sort_by([true, false, true, false], (x) => x)"),
        serde_json::json!([false, false, true, true])
    );
}

#[test]
fn sort_by_uses_the_key_closure() {
    // Sort strings by byte length (a homogeneous Int key).
    assert_eq!(
        sorted_json(r#"_list_sort_by(["ccc", "a", "bb"], (s) => _len(s))"#),
        serde_json::json!(["a", "bb", "ccc"])
    );
}

#[test]
fn empty_and_single_return_as_is() {
    assert_eq!(
        sorted_json("_list_sort_by([], (x) => x)"),
        serde_json::json!([])
    );
    assert_eq!(
        sorted_json("_list_sort_by([42], (x) => x)"),
        serde_json::json!([42])
    );
    // A single-element list never errors even when its key is a
    // non-orderable type (no comparison happens).
    assert_eq!(
        sorted_json("_list_sort_by([[1, 2]], (x) => x)"),
        serde_json::json!([[1, 2]])
    );
}

#[test]
fn ascending_ties_keep_input_order() {
    // Two elements share key 1; stability keeps "a" before "b".
    assert_eq!(
        sorted_tags(r#"_list_sort_by([[1, "a"], [1, "b"], [0, "c"]], (p) => p[0])"#),
        vec!["c", "a", "b"]
    );
}

#[test]
fn descending_is_not_reverse_of_ascending() {
    // Descending flips the key comparison but keeps equal-key ties in
    // ASCENDING input order. `reverse(sort(...))` would instead give
    // ["b", "a", "c"] — the observable difference the contract pins.
    let desc = sorted_tags(r#"_list_sort_by_desc([[1, "a"], [1, "b"], [0, "c"]], (p) => p[0])"#);
    assert_eq!(desc, vec!["a", "b", "c"]);
    assert_ne!(
        desc,
        vec!["b", "a", "c"],
        "sort_desc must not be reverse(sort)"
    );
}

#[test]
fn signed_zero_keys_compare_equal_and_stay_stable() {
    // `-0.0 == +0.0` in the key order, so stability alone decides their
    // relative position: whichever came first in the input stays first.
    assert_eq!(
        sorted_tags(r#"_list_sort_by([[0.0, "pos"], [-0.0, "neg"]], (p) => p[0])"#),
        vec!["pos", "neg"]
    );
    assert_eq!(
        sorted_tags(r#"_list_sort_by([[-0.0, "neg"], [0.0, "pos"]], (p) => p[0])"#),
        vec!["neg", "pos"]
    );
}

#[test]
fn nan_keys_sort_after_positive_infinity() {
    // `NaN` is greater than every non-NaN, so the NaN-keyed element
    // lands last regardless of its input position.
    assert_eq!(
        sorted_tags(r#"_list_sort_by([[1.0, "a"], [sqrt(-1.0), "nan"], [2.0, "b"]], (p) => p[0])"#),
        vec!["a", "b", "nan"]
    );
    // Multiple NaNs compare equal to each other → stable among
    // themselves, and still after the finite key.
    assert_eq!(
        sorted_tags(
            r#"_list_sort_by([[sqrt(-1.0), "n1"], [3.0, "fin"], [sqrt(-1.0), "n2"]], (p) => p[0])"#
        ),
        vec!["fin", "n1", "n2"]
    );
}

#[test]
fn float_keys_order_infinities() {
    // -inf < finite < +inf < NaN.
    assert_eq!(
        sorted_tags(
            r#"_list_sort_by([[1.0, "one"], [Infinity, "pinf"], [-Infinity, "ninf"], [sqrt(-1.0), "nan"]], (p) => p[0])"#
        ),
        vec!["ninf", "one", "pinf", "nan"]
    );
}

#[test]
fn mixed_int_and_float_keys_are_a_loud_error() {
    let err = tree_walk_result("_list_sort_by([1, 2.0], (x) => x)")
        .expect_err("mixed Int/Float keys must raise a type error");
    assert!(
        matches!(&err, RuntimeError::TypeMismatch { .. }),
        "expected TypeMismatch for mixed keys, got {err:?}"
    );
}

#[test]
fn non_orderable_keys_are_a_loud_error() {
    // List keys.
    let err = tree_walk_result("_list_sort_by([[1], [2]], (x) => x)")
        .expect_err("List keys must raise a type error");
    assert!(
        matches!(&err, RuntimeError::TypeMismatch { .. }),
        "got {err:?}"
    );
    // None keys.
    let err = tree_walk_result("_list_sort_by([None, None], (x) => x)")
        .expect_err("None keys must raise a type error");
    assert!(
        matches!(&err, RuntimeError::TypeMismatch { .. }),
        "got {err:?}"
    );
    // Dict keys.
    let err = tree_walk_result("_list_sort_by([{ a: 1 }, { a: 2 }], (x) => x)")
        .expect_err("Dict keys must raise a type error");
    assert!(
        matches!(&err, RuntimeError::TypeMismatch { .. }),
        "got {err:?}"
    );
}

#[test]
fn wrappers_dispatch_through_the_list_module() {
    // The user-facing wrappers desugar to the intrinsics.
    let src = r#"#import list from "std/list"
{
    a: list.sort([3, 1, 2]),
    d: list.sort_desc([1, 3, 2]),
    b: list.sort_by(["ccc", "a", "bb"], (s) => _len(s)),
    e: list.sort_by_desc([1, 2, 3], (x) => x)
}"#;
    let value = tree_walk(src);
    let Value::Dict(dict) = &value else {
        panic!("expected dict, got {value:?}");
    };
    let get = |k: &str| serde_json::to_value(dict.map.get(k).unwrap()).unwrap();
    assert_eq!(get("a"), serde_json::json!([1, 2, 3]));
    assert_eq!(get("d"), serde_json::json!([3, 2, 1]));
    assert_eq!(get("b"), serde_json::json!(["a", "bb", "ccc"]));
    assert_eq!(get("e"), serde_json::json!([3, 2, 1]));
}
