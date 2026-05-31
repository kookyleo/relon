//! Regression: the cmp_lua `W20_n_body_softened` Relon source — a
//! softened 4-body 1D Verlet step — must compile through the cranelift
//! AOT backend and produce bit-identical (`f64::to_bits`) results to the
//! tree-walker oracle for several runtime `n`.
//!
//! W20 exercises native-`F64` cranelift codegen end to end: the
//! `pair_force`/`accel` closures return native `F64` (via a
//! `i==j ? 0.0 : <f64 expr>` ternary, an `Op::If { result_ty: F64 }`),
//! `step` materialises a computed 8-element `List<Float>` literal
//! (`AllocScratchDyn` + i32 header + `StoreF64AtAbsolute` per element),
//! `final_state: range(n).reduce(init, (s,_)=>step(s))` carries a
//! `ListFloat` (i32 arena handle) accumulator across the reduce loop
//! (an `Op::If`/`Op::Loop` joining a `ListFloat` handle), and `#main`
//! reads `final_state[k]` as native `F64` (`LoadF64AtAbsolute`) into a
//! weighted sum.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// The exact W20 production source (mirrors `cmp_lua.rs::w20_relon_src`,
/// copied here so the test does not depend on the bench crate).
fn w20_relon_src() -> &'static str {
    "#unstrict\n\
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
     }"
}

fn oracle(src: &str, n: i64) -> f64 {
    let node = parse_document(src).expect("oracle parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope: Arc<Scope> = Arc::new(Scope::default());
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match walker.run_main(&scope, args).expect("oracle run_main") {
        Value::Float(v) => v.into_inner(),
        other => panic!("oracle returned non-float: {other:?}"),
    }
}

fn aot_run(src: &str, n: i64) -> f64 {
    let eval = AotEvaluator::from_source(src).expect("W20 AOT must compile");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match eval.run_main(args).expect("W20 run_main") {
        Value::Float(v) => v.into_inner(),
        other => panic!("AOT returned non-float: {other:?}"),
    }
}

/// W20 compiles through cranelift and is bit-identical (`f64::to_bits`)
/// to the tree-walker oracle across several runtime `n` — including
/// `n=0` (the bare `init` checksum, the reduce never runs), `n=1`
/// (one step), and larger counts. Both the oracle and the AOT runtime
/// recurse a little (the closures call each other), so the comparison
/// runs on a wide-stack thread mirroring the W16/W19 tests.
#[test]
fn w20_n_body_matches_oracle_bit_exact() {
    let src = w20_relon_src();
    // Compile once up front so a compile failure surfaces directly here
    // (not buried inside the worker thread's panic).
    AotEvaluator::from_source(src).expect("W20 must compile through cranelift");

    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(|| {
            let src = w20_relon_src();
            let mut seen: Vec<(i64, u64)> = Vec::new();
            for n in [0_i64, 1, 2, 5, 10, 100, 1000] {
                let want = oracle(src, n);
                let got = aot_run(src, n);
                assert_eq!(
                    got.to_bits(),
                    want.to_bits(),
                    "W20 oracle bit mismatch at n={n}: got {got} ({:#018x}) want {want} ({:#018x})",
                    got.to_bits(),
                    want.to_bits(),
                );
                seen.push((n, got.to_bits()));
            }
            // Guard against a degenerate "reduce never ran" miscompile that
            // would collapse every `n` to the bare `init` checksum: the
            // per-step kernel does real work, so the checksum must vary
            // with `n` (and differ from the `n=0` `init`-only baseline).
            let baseline = seen[0].1;
            assert!(
                seen.iter().any(|&(_, bits)| bits != baseline),
                "W20 checksum is constant across all n — the reduce loop did not run"
            );
        })
        .expect("spawn wide-stack worker");
    handle.join().expect("W20 oracle worker panicked");
}
