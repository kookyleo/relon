//! Four-way differential for the std/list stable sort family
//! (`_list_sort_by` / `_list_sort_by_desc`).
//!
//! Each case runs tree-walk (golden oracle) vs cranelift-AOT vs LLVM-AOT
//! (under the `llvm-aot` feature) through [`assert_all_backends_bit_equal`],
//! which compares the returned `Value` deep / bit-for-bit (and via a JSON
//! projection that keeps NaN / signed-zero bits distinct). The bundled
//! `list_sort_by_*` bodies reproduce the oracle
//! (`relon_evaluator::stdlib::{compare_sort_keys, compare_sort_floats,
//! list_sort_by_impl}`) exactly: a stable bottom-up merge sort of the
//! decorated key, ties broken by original index, the element moved as a
//! raw 8-byte value.
//!
//! `_list_sort_by` / `_list_sort_by_desc` is the intrinsic every surface
//! wrapper (`sort` / `sort_by` / `sort_desc` / `sort_by_desc`) desugars
//! to — it is the four-way-provable surface. The module-qualified sugar
//! (`list.sort(xs)`) caps to tree-walk on the pre-existing
//! module-namespace limitation shared by every `std/list` member call
//! (`list.sum`, `list.map`, ...), and there is no UFCS `xs.sort()`
//! method surface in the oracle, so neither is covered here.
//!
//! Covered four-way: `Int` and `Float` element lists, identity keys and
//! Int / Float key closures (all four element x key combinations), both
//! directions. `List<String>` / `List<Bool>` elements and String-return
//! keys stay capped (see `list_sort_cap_smoke` below and the ledger).

use std::collections::HashMap;

use relon_evaluator::Value;
use relon_test_harness::{assert_all_backends_bit_equal, AllBackendsReport};

fn no_args() -> HashMap<String, Value> {
    HashMap::new()
}

/// Assert every available AOT backend actually compiled the shape (a
/// decline would silently degrade the test to tree-walk-only) and return
/// the agreed value for an additional expected-output assertion.
fn assert_compiled(source: &str, args: HashMap<String, Value>) -> AllBackendsReport {
    let report = assert_all_backends_bit_equal(source, args);
    assert!(
        report.cranelift_compared,
        "cranelift must compile {source:?}; skipped: {:?}",
        report.cranelift_skip_reason
    );
    #[cfg(feature = "llvm-aot")]
    assert!(
        report.llvm_compared,
        "llvm must compile {source:?}; skipped: {:?}",
        report.llvm_skip_reason
    );
    report
}

fn ints(vs: &[i64]) -> Value {
    Value::list(vs.iter().copied().map(Value::Int).collect())
}

fn floats(vs: &[f64]) -> Value {
    Value::list(vs.iter().copied().map(|f| Value::Float(f.into())).collect())
}

// ---- Int elements, identity key -------------------------------------------

#[test]
fn int_sort_asc() {
    let r = assert_compiled(
        "#main() -> List<Int>\n_list_sort_by([3, 1, 2], (Int x) => x)",
        no_args(),
    );
    assert_eq!(r.value, ints(&[1, 2, 3]));
}

#[test]
fn int_sort_desc() {
    let r = assert_compiled(
        "#main() -> List<Int>\n_list_sort_by_desc([1, 3, 2], (Int x) => x)",
        no_args(),
    );
    assert_eq!(r.value, ints(&[3, 2, 1]));
}

#[test]
fn int_sort_empty() {
    // An empty list literal has no inferable element type; drive it as a
    // typed argument instead. Exercises the `n == 0` fast path (zero key
    // calls, zero merge passes).
    let mut args = HashMap::new();
    args.insert("xs".to_string(), ints(&[]));
    let r = assert_compiled(
        "#main(List<Int> xs) -> List<Int>\n_list_sort_by(xs, (Int x) => x)",
        args,
    );
    assert_eq!(r.value, ints(&[]));
}

#[test]
fn int_sort_single() {
    let r = assert_compiled(
        "#main() -> List<Int>\n_list_sort_by([42], (Int x) => x)",
        no_args(),
    );
    assert_eq!(r.value, ints(&[42]));
}

#[test]
fn int_sort_already_sorted() {
    let r = assert_compiled(
        "#main() -> List<Int>\n_list_sort_by([1, 2, 3, 4, 5], (Int x) => x)",
        no_args(),
    );
    assert_eq!(r.value, ints(&[1, 2, 3, 4, 5]));
}

#[test]
fn int_sort_reverse_sorted() {
    let r = assert_compiled(
        "#main() -> List<Int>\n_list_sort_by([5, 4, 3, 2, 1], (Int x) => x)",
        no_args(),
    );
    assert_eq!(r.value, ints(&[1, 2, 3, 4, 5]));
}

#[test]
fn int_sort_tie_heavy_identity() {
    // Equal ints are indistinguishable, but a longer power-of-two-
    // straddling run exercises every merge width; the sorted output must
    // be exact.
    let r = assert_compiled(
        "#main() -> List<Int>\n_list_sort_by([3, 1, 2, 1, 3, 1, 2, 2, 3], (Int x) => x)",
        no_args(),
    );
    assert_eq!(r.value, ints(&[1, 1, 1, 2, 2, 2, 3, 3, 3]));
}

// ---- Int elements, Int key (stability + desc-vs-reverse) ------------------

#[test]
fn int_sort_by_key_asc_stable() {
    // Keys tie within each decade bucket (10/11 -> 1, 20/21 -> 2); the
    // tie order must stay input order (10 before 11, 20 before 21).
    let r = assert_compiled(
        "#main() -> List<Int>\n_list_sort_by([20, 10, 21, 11], (Int x) => x / 10)",
        no_args(),
    );
    assert_eq!(r.value, ints(&[10, 11, 20, 21]));
}

#[test]
fn int_sort_by_key_desc_is_not_reverse() {
    // Descending swaps the bucket order but keeps ties in input order:
    // [20, 21, 10, 11]. reverse(ascending) would instead be
    // [21, 20, 11, 10], so this pins `sort_desc != reverse(sort)`.
    let r = assert_compiled(
        "#main() -> List<Int>\n_list_sort_by_desc([20, 10, 21, 11], (Int x) => x / 10)",
        no_args(),
    );
    assert_eq!(r.value, ints(&[20, 21, 10, 11]));
}

// ---- Int elements, Float key ----------------------------------------------

#[test]
fn int_sort_by_float_key_asc() {
    // Key `x * 1.0` widens Int -> Float; ordering matches the Int order.
    let r = assert_compiled(
        "#main() -> List<Int>\n_list_sort_by([3, 1, 2], (Int x) => x * 1.0)",
        no_args(),
    );
    assert_eq!(r.value, ints(&[1, 2, 3]));
}

#[test]
fn int_sort_by_float_key_desc() {
    let r = assert_compiled(
        "#main() -> List<Int>\n_list_sort_by_desc([3, 1, 2], (Int x) => x * 1.0)",
        no_args(),
    );
    assert_eq!(r.value, ints(&[3, 2, 1]));
}

// ---- Float elements, identity key -----------------------------------------

#[test]
fn float_sort_asc() {
    let r = assert_compiled(
        "#main() -> List<Float>\n_list_sort_by([3.5, 1.5, 2.5], (Float x) => x)",
        no_args(),
    );
    assert_eq!(r.value, floats(&[1.5, 2.5, 3.5]));
}

#[test]
fn float_sort_desc() {
    let r = assert_compiled(
        "#main() -> List<Float>\n_list_sort_by_desc([1.5, 3.5, 2.5], (Float x) => x)",
        no_args(),
    );
    assert_eq!(r.value, floats(&[3.5, 2.5, 1.5]));
}

// The signed-zero / NaN / infinity edge inputs are driven as arguments:
// an in-source `-0.0` / `sqrt(-1.0)` / `1.0 / 0.0` either fails to lower
// as a list literal (a computed element caps the materialiser — same as
// `_list_map`) or traps (`Float` division by zero is a trap in relon),
// so passing the exact bits as a `List<Float>` argument is both cleaner
// and a stronger test of the raw element bytes.

fn float_arg_list(name: &str, vs: &[f64]) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert(name.to_string(), floats(vs));
    m
}

const SORT_F: &str = "#main(List<Float> xs) -> List<Float>\n_list_sort_by(xs, (Float x) => x)";
const SORT_F_DESC: &str =
    "#main(List<Float> xs) -> List<Float>\n_list_sort_by_desc(xs, (Float x) => x)";

#[test]
fn float_sort_signed_zero_stable_asc() {
    // +0.0 and -0.0 compare equal, so a stable asc sort keeps input
    // order and preserves each element's sign bit (bit-for-bit compare).
    let r = assert_compiled(SORT_F, float_arg_list("xs", &[0.0, -0.0]));
    assert_eq!(r.value, floats(&[0.0, -0.0]));
    if let Value::List(items) = &r.value {
        if let (Value::Float(a), Value::Float(b)) = (&items[0], &items[1]) {
            assert_eq!(a.0.to_bits(), 0.0f64.to_bits(), "first must stay +0.0");
            assert_eq!(b.0.to_bits(), (-0.0f64).to_bits(), "second must stay -0.0");
        } else {
            panic!("expected two floats");
        }
    } else {
        panic!("expected a list");
    }
}

#[test]
fn float_sort_signed_zero_stable_desc() {
    // Descending: +0.0 >= -0.0 (ordered) is true, so the left/lower-index
    // element still wins the tie — input order preserved, not reversed.
    let r = assert_compiled(SORT_F_DESC, float_arg_list("xs", &[0.0, -0.0]));
    assert_eq!(r.value, floats(&[0.0, -0.0]));
}

#[test]
fn float_sort_nan_sorts_last_asc() {
    // NaN is the single greatest class; asc places both NaNs after every
    // finite value, keeping their input order.
    let r = assert_compiled(
        SORT_F,
        float_arg_list("xs", &[2.0, f64::NAN, 1.0, f64::NAN]),
    );
    if let Value::List(items) = &r.value {
        assert_eq!(items.len(), 4);
        assert_eq!(items[0], Value::Float(1.0.into()));
        assert_eq!(items[1], Value::Float(2.0.into()));
        assert!(matches!(&items[2], Value::Float(f) if f.0.is_nan()));
        assert!(matches!(&items[3], Value::Float(f) if f.0.is_nan()));
    } else {
        panic!("expected a list");
    }
}

#[test]
fn float_sort_nan_sorts_first_desc() {
    // Descending: NaN is greatest, so both NaNs lead, then the finite
    // values in descending order.
    let r = assert_compiled(
        SORT_F_DESC,
        float_arg_list("xs", &[2.0, f64::NAN, 1.0, f64::NAN]),
    );
    if let Value::List(items) = &r.value {
        assert_eq!(items.len(), 4);
        assert!(matches!(&items[0], Value::Float(f) if f.0.is_nan()));
        assert!(matches!(&items[1], Value::Float(f) if f.0.is_nan()));
        assert_eq!(items[2], Value::Float(2.0.into()));
        assert_eq!(items[3], Value::Float(1.0.into()));
    } else {
        panic!("expected a list");
    }
}

#[test]
fn float_sort_infinities() {
    // -inf < finite < +inf < NaN.
    let r = assert_compiled(
        SORT_F,
        float_arg_list("xs", &[f64::INFINITY, f64::NEG_INFINITY, 0.0, f64::NAN]),
    );
    if let Value::List(items) = &r.value {
        assert_eq!(items.len(), 4);
        assert!(matches!(&items[0], Value::Float(f) if f.0 == f64::NEG_INFINITY));
        assert_eq!(items[1], Value::Float(0.0.into()));
        assert!(matches!(&items[2], Value::Float(f) if f.0 == f64::INFINITY));
        assert!(matches!(&items[3], Value::Float(f) if f.0.is_nan()));
    } else {
        panic!("expected a list");
    }
}

#[test]
fn float_sort_nan_tie_heavy_mixed() {
    // A longer NaN-laced input straddling merge widths: finite values in
    // order, then every NaN, with the finite run stable.
    let r = assert_compiled(
        SORT_F,
        float_arg_list("xs", &[f64::NAN, 3.0, -1.0, f64::NAN, 2.0, -1.0, f64::NAN]),
    );
    if let Value::List(items) = &r.value {
        assert_eq!(items.len(), 7);
        assert_eq!(items[0], Value::Float((-1.0).into()));
        assert_eq!(items[1], Value::Float((-1.0).into()));
        assert_eq!(items[2], Value::Float(2.0.into()));
        assert_eq!(items[3], Value::Float(3.0.into()));
        assert!(items[4..]
            .iter()
            .all(|v| matches!(v, Value::Float(f) if f.0.is_nan())));
    } else {
        panic!("expected a list");
    }
}

// ---- Float elements, Int key ----------------------------------------------

#[test]
fn float_sort_by_int_key_stable() {
    // Key is a coarse Int bucket (`x > 2.0 ? 1 : 0`); elements sharing a
    // bucket must keep input order (stability observable through the
    // distinct float payloads).
    let r = assert_compiled(
        "#main() -> List<Float>\n_list_sort_by([3.5, 1.5, 2.5, 0.5], (Float x) => x > 2.0 ? 1 : 0)",
        no_args(),
    );
    // bucket 0: 1.5, 0.5 (input order); bucket 1: 3.5, 2.5 (input order).
    assert_eq!(r.value, floats(&[1.5, 0.5, 3.5, 2.5]));
}

#[test]
fn float_sort_by_int_key_desc() {
    let r = assert_compiled(
        "#main() -> List<Float>\n_list_sort_by_desc([3.5, 1.5, 2.5, 0.5], (Float x) => x > 2.0 ? 1 : 0)",
        no_args(),
    );
    // bucket 1 first (3.5, 2.5), then bucket 0 (1.5, 0.5) — ties in
    // input order, so NOT reverse(ascending).
    assert_eq!(r.value, floats(&[3.5, 2.5, 1.5, 0.5]));
}

// ---- Argument-driven input (not a constant-foldable literal) --------------

#[test]
fn int_sort_from_arg_list() {
    let mut args = HashMap::new();
    args.insert("xs".to_string(), ints(&[9, 3, 7, 3, 1, 7, 3]));
    let r = assert_compiled(
        "#main(List<Int> xs) -> List<Int>\n_list_sort_by(xs, (Int x) => x)",
        args,
    );
    assert_eq!(r.value, ints(&[1, 3, 3, 3, 7, 7, 9]));
}

// ---- Cap: String / Bool elements stay on the tree-walk fallback -----------

#[test]
fn string_sort_caps_to_tree_walk() {
    // A String-element sort is outside the four-way envelope: cranelift
    // must DECLINE the shape (loud cap), and tree-walk still returns the
    // correct sorted value. Never a silent miscompile.
    let report = assert_all_backends_bit_equal(
        "#main() -> List<String>\n_list_sort_by([\"b\", \"a\", \"c\"], (String x) => x)",
        no_args(),
    );
    assert!(
        !report.cranelift_compared,
        "String-element sort must cap (decline compilation), but cranelift compiled it"
    );
    assert_eq!(
        report.value,
        Value::list(vec![
            Value::String("a".into()),
            Value::String("b".into()),
            Value::String("c".into()),
        ])
    );
}
