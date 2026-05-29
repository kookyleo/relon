//! ENG-FLOAT-CL correctness: compile a `Float` arithmetic `#main`
//! through the cranelift backend and assert the JIT result matches the
//! tree-walking interpreter bit-for-bit (within a tight tolerance).
//!
//! Before this lane the cranelift op visitor rejected `Op::Add` /
//! `Op::Sub` / `Op::Mul` / `Op::Div` on `IrType::F64` (and `ConstF64`),
//! so any Float-arithmetic `#main` failed to compile native code. These
//! tests drive the full `from_source` pipeline (parse -> analyze ->
//! lower -> cranelift -> JIT) so the new `fadd` / `fsub` / `fmul` /
//! `fdiv` / `f64const` emission is exercised end-to-end, and use the
//! `relon-evaluator` tree-walker as the differential oracle.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_eval_api::{Evaluator, RuntimeError, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// Build a tree-walking oracle for `src`, mirroring the
/// `relon-bench` consistency-test harness shape.
fn build_tree_walker(src: &str) -> (TreeWalkEvaluator, Arc<Scope>) {
    let node = parse_document(src)
        .unwrap_or_else(|e| panic!("parse failed for source:\n{src}\nerror: {e:?}"));
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    (
        TreeWalkEvaluator::new(Arc::new(ctx)),
        Arc::new(Scope::default()),
    )
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Float(f) => f.into_inner(),
        Value::Int(n) => *n as f64,
        other => panic!("expected Float result, got {other:?}"),
    }
}

/// Compile `src` through cranelift, run it and the tree-walker with the
/// same `(x, y)` Float args, and assert both backends agree to within a
/// tight tolerance. Returns the cranelift result so callers can also
/// pin the exact expected value.
fn assert_cranelift_matches_walker(src: &str, x: f64, y: f64) -> f64 {
    let aot = AotEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("cranelift compile failed for:\n{src}\nerror: {e:?}"));

    let mut aot_args = HashMap::new();
    aot_args.insert("x".to_string(), Value::Float(x.into()));
    aot_args.insert("y".to_string(), Value::Float(y.into()));
    let aot_result = aot.run_main(aot_args).expect("cranelift run_main");

    let (walker, scope) = build_tree_walker(src);
    let mut walk_args = HashMap::new();
    walk_args.insert("x".to_string(), Value::Float(x.into()));
    walk_args.insert("y".to_string(), Value::Float(y.into()));
    let walk_result = walker
        .run_main(&scope, walk_args)
        .expect("tree-walker run_main");

    let aot_f = as_f64(&aot_result);
    let walk_f = as_f64(&walk_result);
    assert!(
        (aot_f - walk_f).abs() <= 1e-12 * walk_f.abs().max(1.0),
        "cranelift Float result diverged from tree-walker for:\n{src}\n\
         x={x}, y={y}: cranelift={aot_f}, tree-walker={walk_f}",
    );
    aot_f
}

#[test]
fn float_add_matches_tree_walker() {
    let src = "#main(Float x, Float y) -> Float\nx + y";
    let r = assert_cranelift_matches_walker(src, 40.5, 1.5);
    assert!((r - 42.0).abs() <= 1e-12, "got {r}");
}

#[test]
fn float_sub_matches_tree_walker() {
    let src = "#main(Float x, Float y) -> Float\nx - y";
    let r = assert_cranelift_matches_walker(src, 3.25, 10.75);
    assert!((r - (-7.5)).abs() <= 1e-12, "got {r}");
}

#[test]
fn float_mul_matches_tree_walker() {
    let src = "#main(Float x, Float y) -> Float\nx * y";
    let r = assert_cranelift_matches_walker(src, 6.5, 7.0);
    assert!((r - 45.5).abs() <= 1e-12, "got {r}");
}

#[test]
fn float_div_matches_tree_walker() {
    let src = "#main(Float x, Float y) -> Float\nx / y";
    let r = assert_cranelift_matches_walker(src, 22.0, 7.0);
    assert!((r - (22.0_f64 / 7.0)).abs() <= 1e-12, "got {r}");
}

#[test]
fn float_mixed_expression_matches_tree_walker() {
    // Exercises a chain of fadd / fsub / fmul / fdiv plus an f64const
    // literal in one body, so the const-emission path and all four
    // arithmetic ops are stressed together.
    let src = "#main(Float x, Float y) -> Float\nx * 2.5 + y / 4.0 - 1.0";
    let r = assert_cranelift_matches_walker(src, 8.0, 12.0);
    assert!(
        (r - (8.0 * 2.5 + 12.0 / 4.0 - 1.0)).abs() <= 1e-12,
        "got {r}"
    );
}

// ---------------------------------------------------------------------
// CL-FLOAT-FIX: edges where the cranelift F64 lowering used to diverge
// from the tree-walker oracle.
//
// (1) Float div-by-zero. The oracle
//     (`relon-evaluator::arithmetic::eval_numeric_division`) checks
//     `right.as_f64() == 0.0` *before* the Int/Float split and raises
//     `DivisionByZero`, so cranelift must trap rather than emit IEEE
//     ±inf. Unlike the LLVM AOT backend (whose `llvm.trap` aborts the
//     process), the cranelift trap routes through `raise_trap` and
//     surfaces as a catchable `RuntimeError::DivisionByZero`, so we can
//     assert both sides return an error on the same operand.
//
// (2) Float `==` / `!=` NaN semantics. `relon` compares `Value::Float`
//     through `OrderedFloat`, where `NaN == NaN` is **true** and
//     `NaN != NaN` is **false** — the opposite of raw IEEE. Cranelift
//     must OR a both-NaN test into the equality path. Ordering
//     comparisons stay ordered (NaN -> false).
// ---------------------------------------------------------------------

/// Compile `src` (returning Bool) through cranelift and read the result
/// as an i64 (Bool encodes 0/1).
fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Bool(b) => i64::from(*b),
        Value::Int(n) => *n,
        other => panic!("expected Bool/Int result, got {other:?}"),
    }
}

/// Run a Bool-returning `src` on cranelift and the tree-walker with the
/// same `(x, y)` Float args and assert they agree. Returns the shared
/// result.
fn assert_cranelift_bool_matches_walker(src: &str, x: f64, y: f64) -> i64 {
    let aot = AotEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("cranelift compile failed for:\n{src}\nerror: {e:?}"));
    let mut aot_args = HashMap::new();
    aot_args.insert("x".to_string(), Value::Float(x.into()));
    aot_args.insert("y".to_string(), Value::Float(y.into()));
    let aot_result = aot.run_main(aot_args).expect("cranelift run_main");

    let (walker, scope) = build_tree_walker(src);
    let mut walk_args = HashMap::new();
    walk_args.insert("x".to_string(), Value::Float(x.into()));
    walk_args.insert("y".to_string(), Value::Float(y.into()));
    let walk_result = walker
        .run_main(&scope, walk_args)
        .expect("tree-walker run_main");

    let aot_b = as_i64(&aot_result);
    let walk_b = as_i64(&walk_result);
    assert_eq!(
        aot_b, walk_b,
        "cranelift Bool result diverged from tree-walker for:\n{src}\nx={x}, y={y}",
    );
    aot_b
}

/// Float divide-by-zero must trap with `DivisionByZero` on cranelift,
/// matching the oracle's runtime error — NOT emit IEEE ±inf. The
/// divisor is a runtime arg so the guard cannot be const-folded away.
#[test]
fn float_div_by_zero_traps_like_oracle() {
    let src = "#main(Float x, Float y) -> Float\nx / y";

    // Oracle: a Float divide-by-zero is a runtime error.
    let (walker, scope) = build_tree_walker(src);
    let mut walk_args = HashMap::new();
    walk_args.insert("x".to_string(), Value::Float(1.0.into()));
    walk_args.insert("y".to_string(), Value::Float(0.0.into()));
    let oracle = walker.run_main(&scope, walk_args);
    assert!(
        matches!(oracle, Err(RuntimeError::DivisionByZero(_))),
        "tree-walker should reject Float div-by-zero, got {oracle:?}"
    );

    // Cranelift: the F64 div lowering emits an OEQ-zero guard that traps
    // on the same divisor, surfacing the typed DivisionByZero error.
    let aot = AotEvaluator::from_source(src).expect("cranelift compile");
    let mut aot_args = HashMap::new();
    aot_args.insert("x".to_string(), Value::Float(1.0.into()));
    aot_args.insert("y".to_string(), Value::Float(0.0.into()));
    let err = aot
        .run_main(aot_args)
        .expect_err("Float div-by-zero must trap");
    assert!(
        matches!(err, RuntimeError::DivisionByZero(_)),
        "expected DivisionByZero from cranelift Float div, got {err:?}"
    );
}

/// The guard fires on `-0.0` too (OEQ matches both signed zeros) but not
/// on a normal divisor — the non-zero path still computes the IEEE
/// quotient and agrees with the oracle.
#[test]
fn float_div_neg_zero_traps_but_normal_divisor_ok() {
    let src = "#main(Float x, Float y) -> Float\nx / y";

    let aot = AotEvaluator::from_source(src).expect("cranelift compile");
    let mut neg_zero_args = HashMap::new();
    neg_zero_args.insert("x".to_string(), Value::Float(3.0.into()));
    neg_zero_args.insert("y".to_string(), Value::Float((-0.0_f64).into()));
    let err = aot
        .run_main(neg_zero_args)
        .expect_err("Float div by -0.0 must trap");
    assert!(
        matches!(err, RuntimeError::DivisionByZero(_)),
        "expected DivisionByZero for -0.0 divisor, got {err:?}"
    );

    // Normal divisor: still the IEEE quotient, agreeing with the oracle.
    let r = assert_cranelift_matches_walker(src, 22.0, 7.0);
    assert!((r - (22.0_f64 / 7.0)).abs() <= 1e-12, "got {r}");
}

/// `NaN == NaN` must be **true** and `NaN != NaN` must be **false**,
/// matching `OrderedFloat` equality the tree-walker uses (the opposite
/// of raw IEEE). NaN is fed as a runtime arg.
#[test]
fn float_nan_eq_matches_ordered_float() {
    let nan = f64::NAN;

    let eq = "#main(Float x, Float y) -> Bool\nx == y";
    assert_eq!(
        assert_cranelift_bool_matches_walker(eq, nan, nan),
        1,
        "NaN == NaN must be true (OrderedFloat equality)"
    );

    let ne = "#main(Float x, Float y) -> Bool\nx != y";
    assert_eq!(
        assert_cranelift_bool_matches_walker(ne, nan, nan),
        0,
        "NaN != NaN must be false (OrderedFloat equality)"
    );

    // A NaN is never equal to a non-NaN value (one operand NaN -> the
    // both-NaN OR term is false, OEQ is false).
    assert_eq!(
        assert_cranelift_bool_matches_walker(eq, nan, 1.0),
        0,
        "NaN == 1.0 must be false"
    );
    assert_eq!(
        assert_cranelift_bool_matches_walker(ne, nan, 1.0),
        1,
        "NaN != 1.0 must be true"
    );
}

/// Finite-operand `==` / `!=` are unaffected by the NaN fix (the
/// both-NaN OR term is false for finite operands), including the
/// `+0.0 == -0.0` IEEE case.
#[test]
fn float_finite_eq_unchanged() {
    let eq = "#main(Float x, Float y) -> Bool\nx == y";
    assert_eq!(assert_cranelift_bool_matches_walker(eq, 2.5, 2.5), 1);
    assert_eq!(assert_cranelift_bool_matches_walker(eq, 2.5, 2.6), 0);
    // OrderedFloat / IEEE OEQ both say +0.0 == -0.0.
    assert_eq!(assert_cranelift_bool_matches_walker(eq, 0.0, -0.0), 1);

    let ne = "#main(Float x, Float y) -> Bool\nx != y";
    assert_eq!(assert_cranelift_bool_matches_walker(ne, 2.5, 2.6), 1);
    assert_eq!(assert_cranelift_bool_matches_walker(ne, 2.5, 2.5), 0);
}

/// Ordering comparisons stay ordered: a NaN operand compares `false`
/// for `<` / `<=` / `>` / `>=`, matching the oracle's native `f64`
/// ordering (this guards against the eq fix accidentally leaking into
/// the ordering arms).
#[test]
fn float_nan_ordering_stays_ordered() {
    let nan = f64::NAN;

    let lt = "#main(Float x, Float y) -> Bool\nx < y";
    assert_eq!(assert_cranelift_bool_matches_walker(lt, nan, 1.0), 0);
    assert_eq!(assert_cranelift_bool_matches_walker(lt, 1.0, nan), 0);

    let le = "#main(Float x, Float y) -> Bool\nx <= y";
    assert_eq!(assert_cranelift_bool_matches_walker(le, nan, 1.0), 0);

    let gt = "#main(Float x, Float y) -> Bool\nx > y";
    assert_eq!(assert_cranelift_bool_matches_walker(gt, nan, 1.0), 0);

    let ge = "#main(Float x, Float y) -> Bool\nx >= y";
    assert_eq!(assert_cranelift_bool_matches_walker(ge, nan, 1.0), 0);

    // Sanity: ordering on finite operands still works.
    assert_eq!(assert_cranelift_bool_matches_walker(lt, 1.0, 2.0), 1);
}
