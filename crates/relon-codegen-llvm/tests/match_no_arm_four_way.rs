//! No-match `match` trap parity across backends.
//!
//! A strict-mode `match` that falls through every arm with no `_`
//! catch-all and no arm that matches at runtime must trap with
//! `RuntimeError::TypeMismatch { expected: "a matching arm", .. }` — the
//! exact shape the tree-walk oracle raises in `Expr::Match`. Wave Part 2
//! lowers a `TrapKind::NoMatch` that surfaces that same typed error on
//! every compiled backend instead of leaving the construct capped.
//!
//! This test pins the trap four-way:
//!
//! * **tree-walk** — the oracle (`Expr::Match` no-match arm).
//! * **cranelift-native** — `TrapKind::NoMatch` → sandbox
//!   `TrapKind::NoMatch` → `to_runtime_error` → `TypeMismatch`.
//! * **llvm-native** — `Op::Trap { NoMatch }` records `NativeTrap::NoMatch`
//!   in `state.trap_code` + returns the negative sentinel; the host lifts
//!   it via `NativeTrap::runtime_error_from_code` → `TypeMismatch`.
//! * **wasm32** — shares the cranelift trap path verbatim: the wasm32
//!   cranelift target lowers the identical `cond_trap` → `raise_trap`
//!   host-helper epilogue, so the recorded trap code (`7` = `NoMatch`)
//!   decodes to the same `TypeMismatch` through `TrapKind::from_code`.
//!   The wasm leg is proven structurally (same codegen path, same trap
//!   code) rather than re-driven through wasmtime here; the cranelift
//!   leg below exercises the byte-for-byte code that the wasm32 target
//!   emits.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, RuntimeError, Value};

/// `String` scrutinee; the only arm `Int` provably never matches and
/// there is no `_` catch-all, so the match falls through every arm.
const SRC: &str = "#main(String s) -> String\ns match { Int: \"int\" }";

fn args() -> HashMap<String, Value> {
    HashMap::from([("s".to_string(), Value::String("hi".into()))])
}

/// Run the tree-walk oracle, returning its (trapping) result.
fn tree_walk_run() -> Result<Value, RuntimeError> {
    use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
    let node = relon_parser::parse_document(SRC).expect("parse no-match src");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope = Arc::new(Scope::default());
    walker.run_main(&scope, args())
}

/// The trap each backend must surface: `expected` is byte-identical to
/// the oracle; the harness's `trap_equivalent` keys on the discriminant.
fn assert_no_match_type_mismatch(backend: &str, err: &RuntimeError) {
    match err {
        RuntimeError::TypeMismatch { expected, .. } => {
            assert_eq!(
                expected, "a matching arm",
                "{backend}: no-match trap must carry the oracle's `expected` string, got {expected:?}"
            );
        }
        other => panic!("{backend}: expected RuntimeError::TypeMismatch, got {other:?}"),
    }
}

#[test]
fn tree_walk_no_arm_traps_type_mismatch() {
    let err = tree_walk_run().expect_err("tree-walk: no matching arm must trap");
    assert_no_match_type_mismatch("tree-walk", &err);
}

#[test]
fn cranelift_no_arm_traps_type_mismatch() {
    let ev = AotEvaluator::from_source(SRC).expect("cranelift compiles");
    let err = ev
        .run_main(args())
        .expect_err("cranelift: no matching arm must trap");
    assert_no_match_type_mismatch("cranelift", &err);
}

#[test]
fn llvm_no_arm_traps_type_mismatch() {
    let ev = LlvmAotEvaluator::from_source(SRC).expect("llvm compiles");
    let err = ev
        .run_main(args())
        .expect_err("llvm: no matching arm must trap");
    assert_no_match_type_mismatch("llvm", &err);
}

/// All three host backends agree: same `TypeMismatch` discriminant and
/// the same `expected` string. (Ranges differ by design — the compiled
/// backends only know the entry's `#main` range.)
#[test]
fn no_arm_trap_agrees_three_way() {
    let tw = tree_walk_run().expect_err("tree-walk traps");
    let cl = AotEvaluator::from_source(SRC)
        .expect("cranelift compiles")
        .run_main(args())
        .expect_err("cranelift traps");
    let llvm = LlvmAotEvaluator::from_source(SRC)
        .expect("llvm compiles")
        .run_main(args())
        .expect_err("llvm traps");

    for (name, err) in [("tree-walk", &tw), ("cranelift", &cl), ("llvm", &llvm)] {
        assert_no_match_type_mismatch(name, err);
    }
}
