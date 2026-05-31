//! #362: Int→Float promotion extended to the `%` (Mod) operator — AOT
//! oracle, bit-identical to the tree-walker.
//!
//! The tree-walker (`relon-evaluator::arithmetic::eval_numeric_division`)
//! promotes BOTH operands of a non-`Int`/`Int` `%` to `f64` and computes
//! `a.as_f64() % b.as_f64()` — Rust's `f64 %` = `fmod`: truncated
//! remainder, sign of the dividend. The IR lowering now mirrors this by
//! inserting `Op::ConvertI64ToF64` on the `Int` operand of a mixed
//! `Int`/`Float` `%` and emitting `Op::Mod(F64)`, which the LLVM AOT
//! lowers to `frem` (= `fmod`).
//!
//! These tests pin the LLVM AOT output BIT-IDENTICAL (`f64::to_bits`) to
//! the `TreeWalkEvaluator` on the SAME source across several `n`,
//! including negative dividends (sign-of-dividend semantics) and a
//! divisor that does not divide evenly (non-trivial remainder). No
//! algorithm substitution, no tolerance fudge — a NaN payload or a
//! signed-zero divergence would surface as a bit mismatch.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// Mixed `Int % Float`: `i` (Int) `% 2.5` (Float) → promote `i`, F64 mod.
/// `2.5` does not divide evenly, so the remainder cycles 0.0, 1.0, 2.0,
/// 0.5, 1.5, 0.0, ... exercising `fmod`'s non-trivial residues.
const MIXED_MOD_SRC: &str = "#unstrict\n\
                             #main(Int n) -> Float\n\
                             range(n).reduce(0.0, (acc, i) => acc + i % 2.5)";

/// `Float % Float`: `(i / 2.0)` (mixed → F64) `% 3.0` (F64) → F64 mod.
const FLOAT_MOD_SRC: &str = "#unstrict\n\
                             #main(Int n) -> Float\n\
                             range(n).reduce(0.0, (acc, i) => acc + (i / 2.0) % 3.0)";

/// Negative dividend: `(0.0 - i)` (mixed → negative F64) `% 2.5` (F64).
/// `fmod` takes the sign of the dividend, so the residues here are
/// non-positive — a bit mismatch would expose a wrong sign convention.
const NEG_MOD_SRC: &str = "#unstrict\n\
                           #main(Int n) -> Float\n\
                           range(n).reduce(0.0, (acc, i) => acc + (0.0 - i) % 2.5)";

/// Runtime divisor: `10.0 % (i + 1.0)` — the divisor `(i + 1.0)` is a
/// loop-carried F64 value, so `-O3` cannot prove it non-zero and the
/// divisor-zero trap guard survives into the optimised IR. `i >= 0`, so
/// `(i + 1.0) >= 1.0` and the guard never actually fires (the oracle
/// stays divergence-free), which keeps this usable as an oracle case too.
const RUNTIME_DIVISOR_SRC: &str = "#unstrict\n\
                                   #main(Int n) -> Float\n\
                                   range(n).reduce(0.0, (acc, i) => acc + 10.0 % (i + 1.0))";

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Float(f) => f.into_inner(),
        Value::Int(n) => *n as f64,
        other => panic!("expected Float result, got {other:?}"),
    }
}

fn oracle(src: &str, n: i64) -> f64 {
    let node = parse_document(src).expect("parse oracle src");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope = Arc::new(Scope::default());
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    as_f64(&walker.run_main(&scope, args).expect("tree-walker run_main"))
}

fn aot(src: &str, n: i64) -> f64 {
    let ev = LlvmAotEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("LLVM AOT from_source failed: {e:?}"));
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    as_f64(&ev.run_main(args).expect("LLVM run_main"))
}

fn assert_bit_identical(label: &str, src: &str) {
    // n=0 (empty fold → seed 0.0), n=1 (single iter, i=0), and larger
    // values that walk the full remainder cycle.
    for &n in &[0i64, 1, 2, 3, 5, 7, 10, 100, 1000] {
        let got = aot(src, n);
        let want = oracle(src, n);
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "{label} AOT diverged from tree-walker at n={n}: \
             aot_bits={:#018x} ({got}) oracle_bits={:#018x} ({want})",
            got.to_bits(),
            want.to_bits(),
        );
    }
}

#[test]
fn mixed_int_float_mod_aot_bit_identical_to_oracle() {
    assert_bit_identical("Int%Float", MIXED_MOD_SRC);
}

#[test]
fn float_float_mod_aot_bit_identical_to_oracle() {
    assert_bit_identical("Float%Float", FLOAT_MOD_SRC);
}

#[test]
fn negative_dividend_mod_aot_bit_identical_to_oracle() {
    assert_bit_identical("(-)%Float", NEG_MOD_SRC);
}

#[test]
fn runtime_divisor_mod_aot_bit_identical_to_oracle() {
    assert_bit_identical("F64%runtime", RUNTIME_DIVISOR_SRC);
}

/// Defence in depth: the `Op::Mod(F64)` path carries the same
/// divisor-zero trap guard as `Op::Div(F64)`. The tree-walker raises
/// `DivisionByZero` for `x % 0.0` — `eval_numeric_division` checks
/// `right.as_f64() == 0.0` *before* the `%` runs (for both `/` and
/// `%`), so the oracle never yields a `fmod`-style NaN. The AOT must
/// trap on the same operands.
///
/// We compile `RUNTIME_DIVISOR_SRC`, whose divisor `(i + 1.0)` is a
/// loop-carried F64 value — a constant divisor would let `-O3` prove it
/// non-zero and prune the guard. We assert the trap skeleton + `frem`
/// survive into the optimised IR; we do not run a live trap (it lowers
/// to `ud2`/SIGILL, same as the I64 path in `llvm_divmod_trap.rs`).
#[test]
fn f64_mod_emits_div_by_zero_trap_guard() {
    let ev = LlvmAotEvaluator::from_source(RUNTIME_DIVISOR_SRC)
        .unwrap_or_else(|e| panic!("LLVM AOT from_source failed: {e:?}"));
    let dump = ev.emit_ir_dump();
    assert!(
        dump.contains("llvm.trap"),
        "IR dump missing llvm.trap guard for F64 Mod:\n{dump}"
    );
    // `frem` is the canonical float-remainder instruction emitted for
    // `Op::Mod(F64)`; with a runtime divisor it survives -O3.
    assert!(
        dump.contains("frem"),
        "IR dump missing frem for F64 Mod:\n{dump}"
    );
}
