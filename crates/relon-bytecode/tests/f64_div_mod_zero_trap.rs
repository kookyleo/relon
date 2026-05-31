//! #362 follow-up: the bytecode VM must trap `DivisionByZero` on an
//! f64 divide / mod by a zero divisor — parity with the tree-walker
//! (`arithmetic.rs`: `right.as_f64() == 0.0` traps before the divide,
//! for both `/` and `%`) and the LLVM AOT backend (OEQ-against-0.0
//! guard). Before this fix `BcOp::DivF64` / `BcOp::ModF64` computed the
//! raw IEEE result (inf / NaN), diverging from those two engines.
//!
//! Each case is pinned against the `TreeWalkEvaluator` oracle on the
//! SAME source: non-zero divisors must be bit-identical (`f64::to_bits`)
//! and a zero divisor (`+0.0` and `-0.0`) must raise the SAME
//! `RuntimeError::DivisionByZero` on both engines.

use std::collections::HashMap;
use std::sync::Arc;

use ordered_float::OrderedFloat;
use relon_bytecode::BytecodeEvaluator;
use relon_eval_api::{Evaluator, RuntimeError, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

const DIV: &str = "#main(Float x, Float y) -> Float\nx / y";
const MOD: &str = "#main(Float x, Float y) -> Float\nx % y";

fn args(x: f64, y: f64) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert("x".to_string(), Value::Float(OrderedFloat(x)));
    m.insert("y".to_string(), Value::Float(OrderedFloat(y)));
    m
}

fn bytecode(src: &str, x: f64, y: f64) -> Result<f64, RuntimeError> {
    let ev = BytecodeEvaluator::from_source(src).expect("bytecode compiles the f64 kernel");
    match ev.run_main(args(x, y)) {
        Ok(Value::Float(f)) => Ok(f.into_inner()),
        Ok(other) => panic!("bytecode returned non-float: {other:?}"),
        Err(e) => Err(e),
    }
}

fn oracle(src: &str, x: f64, y: f64) -> Result<f64, RuntimeError> {
    let node = parse_document(src).expect("oracle parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope: Arc<Scope> = Arc::new(Scope::default());
    match walker.run_main(&scope, args(x, y)) {
        Ok(Value::Float(f)) => Ok(f.into_inner()),
        Ok(other) => panic!("oracle returned non-float: {other:?}"),
        Err(e) => Err(e),
    }
}

#[test]
fn f64_div_matches_oracle_and_traps_on_zero() {
    // Non-zero divisor: bytecode is bit-identical to the tree-walker.
    for (x, y) in [(7.0, 2.0), (1.0, 3.0), (-5.5, 2.5), (9.0, -4.0)] {
        let bc = bytecode(DIV, x, y).expect("non-zero div runs");
        let tw = oracle(DIV, x, y).expect("oracle non-zero div runs");
        assert_eq!(bc.to_bits(), tw.to_bits(), "div {x}/{y}: bc={bc} tw={tw}");
    }
    // Zero divisor (both +0.0 and -0.0): both engines trap DivisionByZero.
    for y in [0.0_f64, -0.0_f64] {
        let bc = bytecode(DIV, 7.0, y).expect_err("bytecode div-by-zero must trap");
        let tw = oracle(DIV, 7.0, y).expect_err("oracle div-by-zero must trap");
        assert!(
            matches!(bc, RuntimeError::DivisionByZero(_)),
            "bytecode div by {y} should be DivisionByZero, got {bc:?}"
        );
        assert!(
            matches!(tw, RuntimeError::DivisionByZero(_)),
            "oracle div by {y} should be DivisionByZero, got {tw:?}"
        );
    }
}

#[test]
fn f64_mod_matches_oracle_and_traps_on_zero() {
    // Non-zero divisor: bytecode is bit-identical to the tree-walker
    // (fmod: truncated remainder, sign of the dividend).
    for (x, y) in [(7.0, 2.0), (7.5, 2.0), (-7.0, 3.0), (8.0, -3.0)] {
        let bc = bytecode(MOD, x, y).expect("non-zero mod runs");
        let tw = oracle(MOD, x, y).expect("oracle non-zero mod runs");
        assert_eq!(bc.to_bits(), tw.to_bits(), "mod {x}%{y}: bc={bc} tw={tw}");
    }
    // Zero divisor: both engines trap DivisionByZero (not NaN).
    for y in [0.0_f64, -0.0_f64] {
        let bc = bytecode(MOD, 7.0, y).expect_err("bytecode mod-by-zero must trap");
        let tw = oracle(MOD, 7.0, y).expect_err("oracle mod-by-zero must trap");
        assert!(
            matches!(bc, RuntimeError::DivisionByZero(_)),
            "bytecode mod by {y} should be DivisionByZero, got {bc:?}"
        );
        assert!(
            matches!(tw, RuntimeError::DivisionByZero(_)),
            "oracle mod by {y} should be DivisionByZero, got {tw:?}"
        );
    }
}
