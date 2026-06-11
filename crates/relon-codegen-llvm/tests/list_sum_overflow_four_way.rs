//! Checked `xs.sum()` overflow parity across backends.
//!
//! The method-form `xs.sum()` over a `List<Int>` is *checked*: the first
//! overflowing partial sum (in element order) traps with
//! `RuntimeError::NumericOverflow`, exactly like the `+` operator and the
//! `std/list` reduce-based `sum`. It used to be the language's only
//! silently-wrapping Int arithmetic (a §2.3 spec violation); this test
//! pins the fixed, uniform surface:
//!
//! * **tree-walk** — the native `ListSum` folds with `checked_add` and
//!   raises `RuntimeError::NumericOverflow` on the first overflow.
//! * **cranelift-native** — the bundled `list_int_sum` body's
//!   per-iteration guard emits `Op::Trap { NumericOverflow }` →
//!   sandbox `TrapKind::NumericOverflow` (= 6) → `to_runtime_error` →
//!   `NumericOverflow`.
//! * **llvm-native** — the same bundled body inlines through
//!   `emit_call_stdlib`; `Op::Trap { NumericOverflow }` records
//!   `NativeTrap::NumericOverflow` (= 6) in `state.trap_code` + returns
//!   the negative sentinel; the host lifts it via
//!   `NativeTrap::runtime_error_from_code` → `NumericOverflow`.
//! * **wasm32** — shares the LLVM emitter verbatim (same inlined body,
//!   same `state.trap_code` store, same sentinel); the recorded code
//!   (`6` = `NumericOverflow`) decodes to the same typed error. The wasm
//!   leg is proven structurally (identical codegen path + trap code)
//!   rather than re-driven through wasmtime here; the llvm-native leg
//!   below exercises the byte-for-byte IR that the wasm32 target emits.
//!
//! The guard fires *before* the add (`val >= 0 ? acc > MAX - val : acc <
//! MIN - val`), so the subsequent `Op::Add(I64)` never overflows on any
//! backend — checked (cranelift) and wrapping (LLVM) adds behave
//! identically past the guard, which is what makes the four legs
//! byte/trap-aligned.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, RuntimeError, Value};

/// Method-form sum over a `List<Int>` parameter.
const SRC: &str = "#main(List<Int> xs) -> Int\nxs.sum()";

fn list_args(xs: &[i64]) -> HashMap<String, Value> {
    HashMap::from([(
        "xs".to_string(),
        Value::List(Arc::new(xs.iter().copied().map(Value::Int).collect())),
    )])
}

/// `[MAX - 1, 1, 1]`: the *second* addition overflows — pins "first
/// overflowing partial sum in element order", not just "any overflow".
fn overflow_args() -> HashMap<String, Value> {
    list_args(&[i64::MAX - 1, 1, 1])
}

/// `[MIN + 1, -1, -1]`: negative-direction overflow takes the guard's
/// `val < 0` arm.
fn underflow_args() -> HashMap<String, Value> {
    list_args(&[i64::MIN + 1, -1, -1])
}

/// Normal sums (including a near-MAX non-overflowing one and a
/// cancellation across both signs) must keep returning values.
fn value_cases() -> Vec<(HashMap<String, Value>, i64)> {
    vec![
        (list_args(&[1, 2, 3, 4, 5]), 15),
        (list_args(&[i64::MAX - 1, 1]), i64::MAX),
        (list_args(&[i64::MIN + 1, -1]), i64::MIN),
        (list_args(&[i64::MAX, -1, 1]), i64::MAX),
        (list_args(&[]), 0),
    ]
}

/// Run the tree-walk oracle for `SRC` with `args`.
fn tree_walk_run(args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
    use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
    let node = relon_parser::parse_document(SRC).expect("parse sum src");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope = Arc::new(Scope::default());
    walker.run_main(&scope, args)
}

fn assert_numeric_overflow(backend: &str, err: &RuntimeError) {
    assert!(
        matches!(err, RuntimeError::NumericOverflow(_)),
        "{backend}: overflowing xs.sum() must trap NumericOverflow, got {err:?}"
    );
}

#[test]
fn tree_walk_sum_overflow_traps() {
    let err = tree_walk_run(overflow_args()).expect_err("tree-walk: overflow must trap");
    assert_numeric_overflow("tree-walk", &err);
    let err = tree_walk_run(underflow_args()).expect_err("tree-walk: underflow must trap");
    assert_numeric_overflow("tree-walk", &err);
}

#[test]
fn cranelift_sum_overflow_traps() {
    let ev = AotEvaluator::from_source(SRC).expect("cranelift compiles");
    let err = ev
        .run_main(overflow_args())
        .expect_err("cranelift: overflow must trap");
    assert_numeric_overflow("cranelift", &err);
    let err = ev
        .run_main(underflow_args())
        .expect_err("cranelift: underflow must trap");
    assert_numeric_overflow("cranelift", &err);
}

#[test]
fn llvm_sum_overflow_traps() {
    let ev = LlvmAotEvaluator::from_source(SRC).expect("llvm compiles");
    let err = ev
        .run_main(overflow_args())
        .expect_err("llvm: overflow must trap");
    assert_numeric_overflow("llvm", &err);
    let err = ev
        .run_main(underflow_args())
        .expect_err("llvm: underflow must trap");
    assert_numeric_overflow("llvm", &err);
}

/// Non-overflowing sums stay value-correct and bit-equal three-way —
/// the guard must not perturb the value path (boundary sums landing
/// exactly on `i64::MAX` / `i64::MIN` included).
#[test]
fn sum_values_agree_three_way() {
    let cl = AotEvaluator::from_source(SRC).expect("cranelift compiles");
    let llvm = LlvmAotEvaluator::from_source(SRC).expect("llvm compiles");
    for (args, want) in value_cases() {
        let tw = tree_walk_run(args.clone()).expect("tree-walk value");
        let cl_v = cl.run_main(args.clone()).expect("cranelift value");
        let llvm_v = llvm.run_main(args.clone()).expect("llvm value");
        for (name, got) in [("tree-walk", &tw), ("cranelift", &cl_v), ("llvm", &llvm_v)] {
            assert_eq!(
                got,
                &Value::Int(want),
                "{name}: xs.sum() value mismatch for {args:?}"
            );
        }
    }
}

/// All three host backends agree on the trap: same `NumericOverflow`
/// discriminant. (Ranges differ by design — the compiled backends only
/// know the entry's `#main` range.)
#[test]
fn sum_overflow_trap_agrees_three_way() {
    let cl = AotEvaluator::from_source(SRC).expect("cranelift compiles");
    let llvm = LlvmAotEvaluator::from_source(SRC).expect("llvm compiles");
    for args in [overflow_args(), underflow_args()] {
        let tw = tree_walk_run(args.clone()).expect_err("tree-walk traps");
        let cl_e = cl.run_main(args.clone()).expect_err("cranelift traps");
        let llvm_e = llvm.run_main(args.clone()).expect_err("llvm traps");
        for (name, err) in [("tree-walk", &tw), ("cranelift", &cl_e), ("llvm", &llvm_e)] {
            assert_numeric_overflow(name, err);
        }
    }
}
