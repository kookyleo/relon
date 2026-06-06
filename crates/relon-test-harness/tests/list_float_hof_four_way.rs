//! Wave R3b — four-way differential for the typed `List<Float>` higher-order
//! ops (map / filter / reduce) and the element-type-changing numeric `map`
//! shapes (Int -> Float, Float -> Int).
//!
//! Each case runs tree-walk (golden oracle) vs cranelift-AOT vs LLVM-AOT
//! (under the `llvm-aot` feature) through [`assert_all_backends_bit_equal`],
//! which compares the returned `Value` deep / bit-for-bit. The bundled
//! `list_float_*` bodies share the `List<Int>` record layout (8-byte slots)
//! and dispatch the closure per element via `Op::CallClosure`, so every
//! backend applies the same per-element transform in source order — matching
//! the tree-walk `_list_map` / `_list_filter` / `_list_reduce`.
//!
//! Float arithmetic inside the closures is IEEE-754 (an `f64.add` / compare,
//! no overflow trap), exercised here with a NaN element (ordering: any
//! comparison with NaN is false, so a NaN never survives `x > k` and is never
//! the running max) and signed-zero (`-0.0` vs `0.0` compare equal but keep
//! their bit pattern through map / filter).
//!
//! `List<String>` HOFs (and any map whose closure returns a String) are NOT
//! covered: they stay capped because a `List<String>` result needs a runtime
//! pointer-array builder satisfying the arena-relative handle-slot block
//! invariant, which is not yet a proven four-way substrate.

use std::collections::HashMap;

use relon_evaluator::Value;
use relon_test_harness::assert_all_backends_bit_equal;

fn no_args() -> HashMap<String, Value> {
    HashMap::new()
}

/// Bind each `names[i]` to the scalar `Float(vs[i])`.
fn flatten(names: &[&str], vs: &[f64]) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    for (n, v) in names.iter().zip(vs.iter()) {
        m.insert(n.to_string(), Value::Float((*v).into()));
    }
    m
}

/// Assert every available AOT backend actually compiled the shape (a decline
/// would silently degrade the test to tree-walk-only).
fn assert_compiled(source: &str, args: HashMap<String, Value>) {
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
}

// ---- homogeneous Float map / filter / reduce ------------------------------

#[test]
fn float_map_scale() {
    assert_compiled(
        "#main() -> List<Float>\n[1.0, 2.0, 3.0].map((Float x) => x * 2.0)",
        no_args(),
    );
}

#[test]
fn float_map_reciprocal() {
    assert_compiled(
        "#main() -> List<Float>\n[1.0, 2.0, 4.0].map((Float x) => 1.0 / x)",
        no_args(),
    );
}

#[test]
fn float_filter_threshold() {
    assert_compiled(
        "#main() -> List<Float>\n[0.5, 1.5, 2.5, 0.9].filter((Float x) => x > 1.0)",
        no_args(),
    );
}

#[test]
fn float_filter_keep_none() {
    assert_compiled(
        "#main() -> List<Float>\n[1.0, 2.0].filter((Float x) => x > 100.0)",
        no_args(),
    );
}

#[test]
fn float_reduce_sum() {
    assert_compiled(
        "#main() -> Float\n\
         _list_reduce([1.0, 2.0, 3.0, 4.0], 0.0, (Float a, Float x) => a + x)",
        no_args(),
    );
}

#[test]
fn float_reduce_max() {
    assert_compiled(
        "#main() -> Float\n\
         _list_reduce([3.0, 1.0, 4.0, 1.5, 9.0, 2.0], 0.0, (Float a, Float x) => x > a ? x : a)",
        no_args(),
    );
}

#[test]
fn float_reduce_method_form() {
    assert_compiled(
        "#main() -> Float\n\
         [1.0, 2.0, 3.0, 4.0].reduce(0.0, (Float a, Float x) => a + x)",
        no_args(),
    );
}

// ---- element-type-changing numeric map ------------------------------------

#[test]
fn int_to_float_map() {
    assert_compiled(
        "#main() -> List<Float>\n_list_map([1, 2, 3], (Int x) => x * 2.0)",
        no_args(),
    );
}

#[test]
fn float_to_int_map() {
    assert_compiled(
        "#main() -> List<Int>\n_list_map([1.5, 2.7, 3.2], (Float x) => x > 2.0 ? 1 : 0)",
        no_args(),
    );
}

// ---- IEEE-754 edges: NaN ordering + signed zero ---------------------------
//
// The source builds the float list from `#main` params so a NaN element can be
// constructed programmatically (JSON has no NaN literal). The list literal of
// param refs lowers to the Float-literal materialiser, then filter / reduce
// run over it.

#[test]
fn nan_filter_ordering() {
    // NaN never satisfies `x > 1.0` (every NaN comparison is false), so it is
    // dropped on all backends; the finite elements survive identically.
    let src = "#main(Float a, Float b, Float c, Float d) -> List<Float>\n\
               [a, b, c, d].filter((Float x) => x > 1.0)";
    assert_compiled(
        src,
        flatten(&["a", "b", "c", "d"], &[2.0, f64::NAN, 0.5, 3.0]),
    );
}

#[test]
fn nan_reduce_max_ordering() {
    // Running max via `x > a ? x : a`. A NaN element is never `> a`, so it is
    // skipped and the max stays finite — identical across backends.
    let src = "#main(Float a, Float b, Float c) -> Float\n\
               _list_reduce([a, b, c], 0.0, (Float acc, Float x) => x > acc ? x : acc)";
    assert_compiled(src, flatten(&["a", "b", "c"], &[3.0, f64::NAN, 5.0]));
}

#[test]
fn signed_zero_map_preserved() {
    // `-0.0` and `0.0` compare equal but carry distinct bit patterns; the map
    // identity must preserve each backend's bytes bit-for-bit.
    let src = "#main(Float a, Float b) -> List<Float>\n\
               [a, b].map((Float x) => x * 1.0)";
    assert_compiled(src, flatten(&["a", "b"], &[-0.0, 0.0]));
}
