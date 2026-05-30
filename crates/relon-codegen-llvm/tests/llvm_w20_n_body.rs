//! #359 Part B: W20_n_body_softened AOT oracle — a softened 4-body 1D
//! Verlet integrator whose reduce accumulator is a `List<Float>`.
//!
//! The kernel exercises the genuinely-hard envelope additions this lane
//! lands:
//!   * a `List<Float>` LITERAL materialised into a scratch arena (`init`
//!     + the per-step `step(s)` body that builds a fresh 8-element list);
//!   * a list-VALUED reduce accumulator — `range(n).reduce(init, (s, _)
//!     => step(s))` carries a `List<Float>` handle across `n` iterations,
//!     each reading the previous list `s[k]` and producing a new one;
//!   * `List<Float>` 1D index `s[k]` / `final_state[k]` returning `f64`;
//!   * where-bound closures `pair_force` / `accel` returning `Float` and
//!     `pair_force` taking an `F64` mass — the first closures with a
//!     Float in their signature (the helper/lambda ABI now rides `F64`
//!     as i64 bits, matching the operand stack).
//!
//! HONESTY: the AOT output is pinned BIT-IDENTICAL (`f64::to_bits`) to
//! the `TreeWalkEvaluator` on the SAME production source — no algorithm
//! substitution, no tolerance fudge. A 1-ULP divergence on any lane (or
//! a NaN-payload / signed-zero mismatch) surfaces as a bit mismatch.
//! `n = 0` must return the `init`-state weighted checksum unchanged
//! (the reduce with 0 iterations returns the seed list verbatim).

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// Exact W20 production source — kept verbatim in sync with
/// `relon-bench::benches::cmp_lua::w20_relon_src`.
const W20_SRC: &str = "#unstrict\n\
     #main(Int n) -> Float\n\
     final_state[0] * 1.0 + final_state[1] * 2.0 + final_state[2] * 3.0 + final_state[3] * 4.0\n\
       + final_state[4] * 5.0 + final_state[5] * 6.0 + final_state[6] * 7.0 + final_state[7] * 8.0\n\
     where {\n\
       dt: 0.01,\n\
       soft: 0.1,\n\
       m0: 1.0, m1: 2.0, m2: 0.5, m3: 3.0,\n\
       init: [0.0, 1.0, 2.5, 4.0, 0.1, 0.0, 0.0, 0.2],\n\
       pair_force(s, i, j, mj):\n\
         i == j ? 0.0 :\n\
           (s[j] - s[i]) * mj * (1.0 / (((s[j] - s[i]) * (s[j] - s[i]) + soft) * ((s[j] - s[i]) * (s[j] - s[i]) + soft))),\n\
       accel(s, i): pair_force(s, i, 0, m0) + pair_force(s, i, 1, m1) + pair_force(s, i, 2, m2) + pair_force(s, i, 3, m3),\n\
       step(s): [\n\
         s[0] + s[4] * dt,\n\
         s[1] + s[5] * dt,\n\
         s[2] + s[6] * dt,\n\
         s[3] + s[7] * dt,\n\
         s[4] + accel(s, 0) * dt,\n\
         s[5] + accel(s, 1) * dt,\n\
         s[6] + accel(s, 2) * dt,\n\
         s[7] + accel(s, 3) * dt\n\
       ],\n\
       final_state: range(n).reduce(init, (s, _step) => step(s))\n\
     }";

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Float(f) => f.into_inner(),
        Value::Int(n) => *n as f64,
        other => panic!("expected Float result, got {other:?}"),
    }
}

fn oracle(n: i64) -> f64 {
    let node = parse_document(W20_SRC).expect("parse W20");
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

fn aot(ev: &LlvmAotEvaluator, n: i64) -> f64 {
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    as_f64(&ev.run_main(args).expect("LLVM run_main"))
}

#[test]
fn w20_n_body_aot_bit_identical_to_oracle() {
    let ev = LlvmAotEvaluator::from_source(W20_SRC)
        .unwrap_or_else(|e| panic!("LLVM AOT from_source failed: {e:?}"));
    // n=0  -> empty fold: `final_state == init`, so the result is the
    //         init-state weighted checksum (the seed list unchanged).
    // n=1  -> a single Verlet step.
    // larger n exercises the per-iteration scratch materialise + the
    //         feedback-into-next-step shape that defeats any fold.
    for &n in &[0i64, 1, 2, 3, 5, 10, 50, 100, 500, 1000] {
        let got = aot(&ev, n);
        let want = oracle(n);
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "W20 AOT diverged from tree-walker at n={n}: \
             aot_bits={:#018x} ({got}) oracle_bits={:#018x} ({want})",
            got.to_bits(),
            want.to_bits(),
        );
    }
}

/// n=0 must return the init-state checksum: with 0 reduce iterations
/// the accumulator is `init` verbatim, so the weighted sum is
/// `0*1 + 1*2 + 2.5*3 + 4*4 + 0.1*5 + 0*6 + 0*7 + 0.2*8`.
#[test]
fn w20_n_zero_returns_init_checksum() {
    let ev = LlvmAotEvaluator::from_source(W20_SRC)
        .unwrap_or_else(|e| panic!("LLVM AOT from_source failed: {e:?}"));
    let expected: f64 = 0.0 * 1.0
        + 1.0 * 2.0
        + 2.5 * 3.0
        + 4.0 * 4.0
        + 0.1 * 5.0
        + 0.0 * 6.0
        + 0.0 * 7.0
        + 0.2 * 8.0;
    let got = aot(&ev, 0);
    assert_eq!(
        got.to_bits(),
        expected.to_bits(),
        "W20 n=0 init checksum mismatch: got {got} ({:#018x}), expected {expected} ({:#018x})",
        got.to_bits(),
        expected.to_bits(),
    );
    // And it must equal the oracle's n=0 too.
    assert_eq!(got.to_bits(), oracle(0).to_bits());
}
