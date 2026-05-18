//! v6-γ M4: three-way differential smoke tests.
//!
//! Each test case feeds `(source, args)` into
//! [`diff_test_3way`] and asserts the resulting
//! [`ThreeWayResult`] is one of the passing variants. The trace-JIT
//! path only synthesises the Phase-1 ArithControl subset today, so
//! every test here uses a `#main(Int x, Int y) -> Int : x op y`
//! shape.

use std::collections::HashMap;

use relon_eval_api::Value;
use relon_test_harness::three_way::{diff_test_3way, ThreeWayResult};

fn args(x: i64, y: i64) -> HashMap<String, Value> {
    let mut h = HashMap::new();
    h.insert("x".to_string(), Value::Int(x));
    h.insert("y".to_string(), Value::Int(y));
    h
}

/// Drive one case and pretty-print the outcome on assertion failure.
fn run(source: &'static str, args: HashMap<String, Value>, expected: Value) {
    let result = diff_test_3way(source, args).expect("backend setup");
    match &result {
        ThreeWayResult::AllAgree(v) => {
            assert_eq!(*v, expected, "AllAgree returned wrong value");
        }
        other => panic!("expected AllAgree({expected:?}), got {other:?} for `{source}`"),
    }
}

#[test]
fn add_two_positive_ints() {
    run(
        "#main(Int x, Int y) -> Int\nx + y",
        args(40, 2),
        Value::Int(42),
    );
}

#[test]
fn sub_yields_difference() {
    run(
        "#main(Int x, Int y) -> Int\nx - y",
        args(50, 8),
        Value::Int(42),
    );
}

#[test]
fn mul_yields_product() {
    run(
        "#main(Int x, Int y) -> Int\nx * y",
        args(6, 7),
        Value::Int(42),
    );
}

#[test]
fn div_yields_quotient() {
    run(
        "#main(Int x, Int y) -> Int\nx / y",
        args(84, 2),
        Value::Int(42),
    );
}

#[test]
fn add_negative_ints() {
    run(
        "#main(Int x, Int y) -> Int\nx + y",
        args(-100, 142),
        Value::Int(42),
    );
}

#[test]
fn sub_negative_y() {
    run(
        "#main(Int x, Int y) -> Int\nx - y",
        args(40, -2),
        Value::Int(42),
    );
}

#[test]
fn mul_with_negative_x() {
    run(
        "#main(Int x, Int y) -> Int\nx * y",
        args(-6, -7),
        Value::Int(42),
    );
}

#[test]
fn div_signed_negative_lhs() {
    run(
        "#main(Int x, Int y) -> Int\nx / y",
        args(-84, 2),
        Value::Int(-42),
    );
}

#[test]
fn add_zero_yields_y() {
    run(
        "#main(Int x, Int y) -> Int\nx + y",
        args(0, 42),
        Value::Int(42),
    );
}

#[test]
fn mul_by_one_is_identity() {
    run(
        "#main(Int x, Int y) -> Int\nx * y",
        args(42, 1),
        Value::Int(42),
    );
}

/// Source outside the synthesis envelope still produces a "pass"
/// variant. This is the canary that the harness doesn't FAIL on
/// rich corpus cases — it surfaces them as
/// `TraceJitNotApplicable` / `CraneliftUnsupported`.
#[test]
fn rich_source_falls_back_gracefully() {
    let result = diff_test_3way("#main(Int x, Int y) -> Int\nx + y * 2", args(10, 16))
        .expect("backend setup");
    assert!(
        result.is_pass(),
        "rich expressions must fall back through a non-failing variant; got {result:?}"
    );
}
