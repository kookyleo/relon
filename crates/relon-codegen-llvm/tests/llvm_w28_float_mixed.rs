//! #359: W28_float_mixed_ops AOT oracle — the production source mixes
//! Int and Float arithmetic (`acc + i / 3.0 + i % 7`), which the IR
//! lowering now compiles by inserting `Op::ConvertI64ToF64` promotions
//! that mirror the tree-walker's `NumericValue::as_f64()`.
//!
//! The reduce body lowers to:
//!   * `i / 3.0` — I64 / F64 → promote `i` (sitofp), F64 div.
//!   * `acc + (i/3.0)` — F64 + F64 → F64 add.
//!   * `i % 7` — I64 % I64 → I64 mod (no promotion).
//!   * `(...) + (i%7)` — F64 + I64 → promote the mod-result, F64 add.
//!
//! This pins the LLVM AOT output BIT-IDENTICAL (`f64::to_bits`) to the
//! `TreeWalkEvaluator` on the SAME source — no algorithm substitution,
//! no tolerance fudge. NaN payloads and signed-zero divergence would
//! surface as a bit mismatch.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// Exact W28 production source (kept in sync with
/// `relon-bench::benches::cmp_lua::w28_relon_src`).
const W28_SRC: &str = "#unstrict\n\
                       #main(Int n) -> Float\n\
                       range(n).reduce(0.0, (acc, i) => acc + i / 3.0 + i % 7)";

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Float(f) => f.into_inner(),
        Value::Int(n) => *n as f64,
        other => panic!("expected Float result, got {other:?}"),
    }
}

fn oracle(n: i64) -> f64 {
    let node = parse_document(W28_SRC).expect("parse W28");
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

fn aot(n: i64) -> f64 {
    let ev = LlvmAotEvaluator::from_source(W28_SRC)
        .unwrap_or_else(|e| panic!("LLVM AOT from_source failed: {e:?}"));
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    as_f64(&ev.run_main(args).expect("LLVM run_main"))
}

#[test]
fn w28_mixed_int_float_aot_bit_identical_to_oracle() {
    // n=0 (empty fold → seed 0.0), n=1 (single iter, i=0), and larger
    // values that exercise the i/3.0 rounding + the i%7 wrap.
    for &n in &[0i64, 1, 2, 7, 10, 100, 1000] {
        let got = aot(n);
        let want = oracle(n);
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "W28 AOT diverged from tree-walker at n={n}: \
             aot_bits={:#018x} ({got}) oracle_bits={:#018x} ({want})",
            got.to_bits(),
            want.to_bits(),
        );
    }
}
