//! Two-way differential smoke tests.
//!
//! Each test case feeds `(source, args)` into [`diff_test_2way`] and
//! asserts the result is [`TwoWayResult::Agree`] with the expected
//! value — both backends must match bitwise AND produce the right
//! answer. Cases use a `#main(Int x, Int y) -> Int` shape.

use std::collections::HashMap;

use relon_eval_api::Value;
use relon_test_harness::two_way::{diff_test_2way, TwoWayResult};

fn args(x: i64, y: i64) -> HashMap<String, Value> {
    let mut h = HashMap::new();
    h.insert("x".to_string(), Value::Int(x));
    h.insert("y".to_string(), Value::Int(y));
    h
}

/// Drive one case and pretty-print the outcome on assertion failure.
fn run(source: &'static str, args: HashMap<String, Value>, expected: Value) {
    let result = diff_test_2way(source, args).expect("backend setup");
    match &result {
        TwoWayResult::Agree(v) => {
            assert_eq!(*v, expected, "Agree returned wrong value");
        }
        other => panic!("expected Agree({expected:?}), got {other:?} for `{source}`"),
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

/// A richer expression shape (mixed precedence) still agrees across
/// both backends — and the runner classifies it through a passing
/// variant rather than failing setup.
#[test]
fn rich_source_agrees() {
    let result = diff_test_2way("#main(Int x, Int y) -> Int\nx + y * 2", args(10, 16))
        .expect("backend setup");
    assert!(
        result.is_pass(),
        "rich expressions must land on a passing variant; got {result:?}"
    );
    assert!(
        matches!(result, TwoWayResult::Agree(Value::Int(42))),
        "expected Agree(Int(42)); got {result:?}"
    );
}
