//! S1/S2 differential gate for the in-place region-walk return ABI.
//!
//! Asserts that a `#main(List<List<scalar>> xss) -> List<List<scalar>> =
//! xss` identity return produces **bit-identical** output on the
//! cranelift-AOT backend, the llvm-AOT backend (S2 — gated behind the
//! `llvm-aot` feature), and the tree-walk golden oracle. Both AOT
//! backends decode the value in place at its source region through the
//! one shared host pipeline (`relon_eval_api::inplace_return`): negative
//! sentinel `-(root_abs+1)` → region-select → verifier → decode.
//!
//! Three layers:
//!  1. Hand-written edge cases (empty outer, empty rows, single element,
//!     many rows, `i64::MIN`/`MAX`, signed-zero / extreme floats, bools).
//!  2. A proptest generator feeding random `List<List<scalar>>` values
//!     through the same differential — the auto-shrinking "shapes you
//!     didn't think of" net.
//!  3. Loud-cap guards: `List<List<String>>` / `List<Schema>` /
//!     parameter-*field* `List<List<scalar>>` returns must still decline
//!     on **both** AOT backends (they fall back to tree-walk in
//!     production), never silently mis-decode.

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

/// Run the differential and assert the AOT backends actually compiled
/// the shape (an in-place return must NOT silently fall back to a skip
/// here — the whole point of S1/S2 is that the backends express it).
/// cranelift is always asserted; llvm is asserted only when the
/// `llvm-aot` feature is on (otherwise its leg is a recorded skip and the
/// default no-LLVM workspace build stays green).
fn assert_cranelift_inplace(src: &str, v: Value) {
    let report = assert_all_backends_bit_equal(src, args(v));
    assert!(
        report.cranelift_compared,
        "cranelift must compile the in-place List<List<scalar>> return; skipped: {:?}",
        report.cranelift_skip_reason
    );
    #[cfg(feature = "llvm-aot")]
    assert!(
        report.llvm_compared,
        "llvm must compile the in-place List<List<scalar>> return; skipped: {:?}",
        report.llvm_skip_reason
    );
}

// ---- F4: parameter-FIELD List<List<Int>> return (`w.rows`) -----------

const SRC_W_ROWS: &str = "#schema W { rows: List<List<Int>>, n: Int }\n\
     #main(W w) -> List<List<Int>>\nw.rows";

fn w_rows(rows: &[&[i64]], n: i64) -> HashMap<String, Value> {
    let map = std::collections::BTreeMap::from([
        (
            relon_eval_api::smol_str::SmolStr::from("rows"),
            int_rows(rows),
        ),
        (relon_eval_api::smol_str::SmolStr::from("n"), Value::Int(n)),
    ]);
    let w = Value::branded_dict(map, Some("W".into()));
    let mut m = HashMap::new();
    m.insert("w".to_string(), w);
    m
}

#[test]
fn param_field_rows_list_list_int() {
    let report = assert_all_backends_bit_equal(
        SRC_W_ROWS,
        w_rows(&[&[1, 2, 3], &[], &[i64::MIN, i64::MAX, -1]], 5),
    );
    assert!(report.cranelift_compared, "cranelift must compile w.rows");
    #[cfg(feature = "llvm-aot")]
    assert!(report.llvm_compared, "llvm must compile w.rows");
}

#[test]
fn param_field_rows_empty() {
    let report = assert_all_backends_bit_equal(SRC_W_ROWS, w_rows(&[], 0));
    assert!(report.cranelift_compared, "cranelift must compile w.rows");
    #[cfg(feature = "llvm-aot")]
    assert!(report.llvm_compared, "llvm must compile w.rows");
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
        #[cfg(feature = "llvm-aot")]
        prop_assert!(report.llvm_compared, "llvm must compile the shape");
    }

    #[test]
    fn diff_float(val in list_list_float_strat()) {
        let report = assert_all_backends_bit_equal(SRC_FLOAT, args(val));
        prop_assert!(report.cranelift_compared, "cranelift must compile the shape");
        #[cfg(feature = "llvm-aot")]
        prop_assert!(report.llvm_compared, "llvm must compile the shape");
    }

    #[test]
    fn diff_bool(val in list_list_bool_strat()) {
        let report = assert_all_backends_bit_equal(SRC_BOOL, args(val));
        prop_assert!(report.cranelift_compared, "cranelift must compile the shape");
        #[cfg(feature = "llvm-aot")]
        prop_assert!(report.llvm_compared, "llvm must compile the shape");
    }
}

// ---- loud-cap guards: unsupported shapes decline, never miscompile ---

/// The shapes S1/S2 do NOT lift must still make **both** AOT backends
/// **decline** the `#main` shape (a setup error). In production
/// `Backend::Auto` falls back to the tree-walk oracle; here we assert the
/// decline is loud (an `Err`), so a silent miscompile can never sneak in
/// on either backend.
#[test]
fn unsupported_return_shapes_fail_loudly_not_silently() {
    let cap_cases = [
        // Inner pointer-array element (String) — in-place reader can't
        // decode a per-element pointer array yet (F5).
        "#main(List<List<String>> xss) -> List<List<String>>\nxss",
        // Parameter-*field* List<List<String>> — a double pointer array
        // field is still out of scope (F5).
        "#schema W { rows: List<List<String>>, n: Int }\n\
         #main(W w) -> List<List<String>>\nw.rows",
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
        #[cfg(feature = "llvm-aot")]
        match new_evaluator(src, Backend::LlvmAot) {
            Err(BackendError::LlvmAot(_)) => { /* loud decline — correct */ }
            Err(other) => panic!("expected an LlvmAot decline for `{src}`, got {other}"),
            Ok(_) => panic!(
                "llvm unexpectedly accepted an unsupported in-place return shape: `{src}` — \
                 a silent-miscompile path may have opened"
            ),
        }
    }
}
