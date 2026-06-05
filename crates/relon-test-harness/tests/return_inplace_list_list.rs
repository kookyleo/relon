//! S1 differential gate for the in-place region-walk return ABI.
//!
//! Asserts that a `#main(List<List<scalar>> xss) -> List<List<scalar>> =
//! xss` identity return produces **bit-identical** output on the
//! cranelift-AOT backend (which decodes the value in place at its source
//! region, gated by the host verifier) and the tree-walk golden oracle.
//!
//! Three layers:
//!  1. Hand-written edge cases (empty outer, empty rows, single element,
//!     many rows, `i64::MIN`/`MAX`, signed-zero / extreme floats, bools).
//!  2. A proptest generator feeding random `List<List<scalar>>` values
//!     through the same differential — the auto-shrinking "shapes you
//!     didn't think of" net.
//!  3. Loud-cap guards: `List<List<String>>` / `List<Schema>` /
//!     parameter-*field* `List<List<scalar>>` returns must still decline
//!     the cranelift shape (they fall back to tree-walk in production),
//!     never silently mis-decode.

use std::collections::HashMap;
use std::sync::Arc;

use ordered_float::OrderedFloat;
use proptest::prelude::*;
use relon::{new_evaluator, Backend, BackendError};
use relon_eval_api::Value;
use relon_test_harness::assert_all_backends_bit_equal;

const SRC_INT: &str = "#main(List<List<Int>> xss) -> List<List<Int>>\nxss";
const SRC_FLOAT: &str = "#main(List<List<Float>> xss) -> List<List<Float>>\nxss";
const SRC_BOOL: &str = "#main(List<List<Bool>> xss) -> List<List<Bool>>\nxss";

fn args(v: Value) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert("xss".to_string(), v);
    m
}

fn int_rows(rows: &[&[i64]]) -> Value {
    Value::List(Arc::new(
        rows.iter()
            .map(|r| Value::List(Arc::new(r.iter().copied().map(Value::Int).collect())))
            .collect(),
    ))
}

fn float_rows(rows: &[&[f64]]) -> Value {
    Value::List(Arc::new(
        rows.iter()
            .map(|r| {
                Value::List(Arc::new(
                    r.iter()
                        .copied()
                        .map(|f| Value::Float(OrderedFloat(f)))
                        .collect(),
                ))
            })
            .collect(),
    ))
}

fn bool_rows(rows: &[&[bool]]) -> Value {
    Value::List(Arc::new(
        rows.iter()
            .map(|r| Value::List(Arc::new(r.iter().copied().map(Value::Bool).collect())))
            .collect(),
    ))
}

/// Run the differential and assert cranelift actually compiled the shape
/// (an in-place return must NOT silently fall back to a skip here — the
/// whole point of S1 is that cranelift expresses it).
fn assert_cranelift_inplace(src: &str, v: Value) {
    let report = assert_all_backends_bit_equal(src, args(v));
    assert!(
        report.cranelift_compared,
        "cranelift must compile the in-place List<List<scalar>> return; skipped: {:?}",
        report.cranelift_skip_reason
    );
}

// ---- hand-written edge cases ----------------------------------------

#[test]
fn int_empty_outer() {
    assert_cranelift_inplace(SRC_INT, int_rows(&[]));
}

#[test]
fn int_empty_rows() {
    assert_cranelift_inplace(SRC_INT, int_rows(&[&[], &[]]));
}

#[test]
fn int_single_element() {
    assert_cranelift_inplace(SRC_INT, int_rows(&[&[42]]));
}

#[test]
fn int_mixed_lengths_with_blank() {
    assert_cranelift_inplace(SRC_INT, int_rows(&[&[1, 2, 3], &[], &[4], &[5, 6]]));
}

#[test]
fn int_extreme_values() {
    assert_cranelift_inplace(SRC_INT, int_rows(&[&[i64::MIN, i64::MAX, 0, -1], &[1]]));
}

#[test]
fn float_extremes_and_signed_zero() {
    assert_cranelift_inplace(
        SRC_FLOAT,
        float_rows(&[&[0.0, -0.0, f64::MIN, f64::MAX], &[3.5], &[]]),
    );
}

#[test]
fn bool_mixed() {
    assert_cranelift_inplace(SRC_BOOL, bool_rows(&[&[true, false, true], &[], &[false]]));
}

// ---- proptest: the "shapes you didn't think of" net ------------------

fn int_strat() -> impl Strategy<Value = i64> {
    prop_oneof![Just(i64::MIN), Just(i64::MAX), Just(0i64), any::<i64>()]
}

fn float_strat() -> impl Strategy<Value = f64> {
    // Exclude NaN: the bit-equal compare is bit-preserving, but a NaN
    // arg would force the generated-vs-decoded compare onto NaN-aware
    // paths unrelated to the ABI under test.
    prop_oneof![
        Just(0.0f64),
        Just(-0.0f64),
        Just(f64::MIN),
        Just(f64::MAX),
        -1e9f64..1e9f64,
    ]
}

fn list_list_int_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(prop::collection::vec(int_strat(), 0..5), 0..5).prop_map(|rows| {
        Value::List(Arc::new(
            rows.into_iter()
                .map(|r| Value::List(Arc::new(r.into_iter().map(Value::Int).collect())))
                .collect(),
        ))
    })
}

fn list_list_float_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(prop::collection::vec(float_strat(), 0..5), 0..5).prop_map(|rows| {
        Value::List(Arc::new(
            rows.into_iter()
                .map(|r| {
                    Value::List(Arc::new(
                        r.into_iter()
                            .map(|f| Value::Float(OrderedFloat(f)))
                            .collect(),
                    ))
                })
                .collect(),
        ))
    })
}

fn list_list_bool_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(prop::collection::vec(any::<bool>(), 0..5), 0..5).prop_map(|rows| {
        Value::List(Arc::new(
            rows.into_iter()
                .map(|r| Value::List(Arc::new(r.into_iter().map(Value::Bool).collect())))
                .collect(),
        ))
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn diff_int(val in list_list_int_strat()) {
        let report = assert_all_backends_bit_equal(SRC_INT, args(val));
        prop_assert!(report.cranelift_compared, "cranelift must compile the shape");
    }

    #[test]
    fn diff_float(val in list_list_float_strat()) {
        let report = assert_all_backends_bit_equal(SRC_FLOAT, args(val));
        prop_assert!(report.cranelift_compared, "cranelift must compile the shape");
    }

    #[test]
    fn diff_bool(val in list_list_bool_strat()) {
        let report = assert_all_backends_bit_equal(SRC_BOOL, args(val));
        prop_assert!(report.cranelift_compared, "cranelift must compile the shape");
    }
}

// ---- loud-cap guards: unsupported shapes decline, never miscompile ---

/// The shapes S1 does NOT lift must still make the cranelift backend
/// **decline** the `#main` shape (a setup error). In production
/// `Backend::Auto` falls back to the tree-walk oracle; here we assert the
/// decline is loud (an `Err`), so a silent miscompile can never sneak in.
#[test]
fn unsupported_return_shapes_fail_loudly_not_silently() {
    let cap_cases = [
        // Inner pointer-array element (String) — in-place reader can't
        // decode a per-element pointer array yet.
        "#main(List<List<String>> xss) -> List<List<String>>\nxss",
        // Parameter-*field* List<List<Int>> — the field load re-encodes
        // into the materialised inner form, which would mis-decode; stays
        // a loud cap until proven bit-equal.
        "#schema W { List<List<Int>> rows: * }\n#main(W w) -> List<List<Int>>\nw.rows",
    ];
    for src in cap_cases {
        match new_evaluator(src, Backend::CraneliftAot) {
            Err(BackendError::CraneliftAot(_)) => { /* loud decline — correct */ }
            Err(other) => panic!("expected a CraneliftAot decline for `{src}`, got {other}"),
            Ok(_) => panic!(
                "cranelift unexpectedly accepted an unsupported in-place return shape: `{src}` — \
                 a silent-miscompile path may have opened"
            ),
        }
    }
}
