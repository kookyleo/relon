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
use relon_eval_api::{Evaluator, Value};
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
