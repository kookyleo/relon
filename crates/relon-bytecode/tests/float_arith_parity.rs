//! Float (F64) arithmetic parity between the bytecode VM and the
//! tree-walker.
//!
//! Regression guard for ENG-FLOAT-CL (bytecode half): `ConstF64` used
//! to be constant-folded away at compile time, so any Float program
//! whose arithmetic depended on a runtime parameter was rejected by
//! `BytecodeEvaluator::from_source` (`UnsupportedOp("... Op::ConstF64")`).
//! The compiler now emits a real `BcOp::ConstF64` push that rides the
//! same u64 operand-stack lane (f64 bits via `to_bits`/`from_bits`) the
//! existing `AddF64` / `SubF64` / `MulF64` / `DivF64` arms consume.
//!
//! Each case runs a runtime-data-dependent Float program through both
//! backends and asserts the bytecode result matches the tree-walker
//! reference to a tight tolerance (the two backends evaluate the same
//! IEEE-754 ops, so the match is expected to be bit-exact — the
//! tolerance only guards against accidental ULP-level divergence).

use std::collections::HashMap;
use std::sync::Arc;

use ordered_float::OrderedFloat;
use relon_bytecode::BytecodeEvaluator;
use relon_eval_api::{Context, Evaluator, Value};
use relon_evaluator::TreeWalkEvaluator;

/// Build the tree-walking reference backend from source. Mirrors the
/// `build_tree_walk_evaluator_from_parsed` path the top-level `relon`
/// crate uses (parse -> analyze -> Context -> prepare).
fn tree_walk(source: &str) -> TreeWalkEvaluator {
    let node = relon_parser::parse_document(source).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    TreeWalkEvaluator::new(Arc::new(ctx))
}

/// Pull the f64 payload out of a `Value::Float`, panicking with a clear
/// message on any other shape.
fn as_f64(v: &Value, who: &str) -> f64 {
    match v {
        Value::Float(f) => f.into_inner(),
        other => panic!("{who} did not return a Float, got {other:?}"),
    }
}

/// Run `source` with `args` through both the tree-walker and the
/// bytecode VM and assert the Float results agree to a tight tolerance.
fn assert_float_parity(source: &str, args: HashMap<String, Value>) {
    let tw = tree_walk(source);
    let tw_v = Evaluator::run_main(&tw, args.clone()).expect("tree-walk run_main");

    let bc = BytecodeEvaluator::from_source(source)
        .expect("bytecode compile (ConstF64 must no longer be folded)");
    let bc_v = bc.run_main(args).expect("bytecode run_main");

    let tw_f = as_f64(&tw_v, "tree-walk");
    let bc_f = as_f64(&bc_v, "bytecode");

    // The two backends run the same IEEE-754 ops, so they should agree
    // bit-for-bit; the tolerance is a tight backstop, not a fudge.
    let tol = 1e-12 * (1.0 + tw_f.abs());
    assert!(
        (tw_f - bc_f).abs() <= tol,
        "Float parity drift for `{source}`: tree-walk={tw_f}, bytecode={bc_f}, |diff|={}",
        (tw_f - bc_f).abs()
    );
}

fn farg(pairs: &[(&str, f64)]) -> HashMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), Value::Float(OrderedFloat(*v))))
        .collect()
}

/// 4-term dot product of runtime Float inputs against constant weights.
/// Exercises `MulF64` + `AddF64` over `ConstF64`-pushed literals and
/// `LocalGet`-pushed runtime params.
#[test]
fn dot_product_matches_tree_walker() {
    let src = "#main(Float a, Float b, Float c, Float d) -> Float\n\
               a * 1.5 + b * 2.5 + c * 3.5 + d * 4.5";
    for args in [
        farg(&[("a", 0.5), ("b", 1.25), ("c", 2.0), ("d", 3.5)]),
        farg(&[("a", -4.0), ("b", 0.0), ("c", 7.75), ("d", -1.25)]),
        farg(&[("a", 1e6), ("b", 1e-6), ("c", 5.0625), ("d", 2.5)]),
    ] {
        assert_float_parity(src, args);
    }
}

/// Running f64 accumulation mixing add / sub / mul / div over runtime
/// Float data. Exercises every non-mod F64 arith arm in one expression.
#[test]
fn running_sum_mixed_ops_matches_tree_walker() {
    let src = "#main(Float a, Float b, Float c, Float d) -> Float\n\
               (a + b + c + d) * (a - b + c - d) / (a + 1.0)";
    for args in [
        farg(&[("a", 0.5), ("b", 1.25), ("c", 2.0), ("d", 3.5)]),
        farg(&[("a", 9.0), ("b", -2.5), ("c", 0.125), ("d", 4.0)]),
    ] {
        assert_float_parity(src, args);
    }
}

/// Nested Float expression with a runtime-dependent denominator —
/// guards the `DivF64` arm on a value produced by upstream `ConstF64` +
/// arith rather than a bare literal.
#[test]
fn nested_float_division_matches_tree_walker() {
    let src = "#main(Float x, Float y) -> Float\n\
               ((x + y) * (x - y)) / (x * x + 1.0)";
    for args in [
        farg(&[("x", 3.25), ("y", 1.5)]),
        farg(&[("x", -8.0), ("y", 2.5)]),
        farg(&[("x", 0.0), ("y", 0.0)]),
    ] {
        assert_float_parity(src, args);
    }
}
