//! review-improvement-160 bytecode M3 phase 2: cover the
//! `list.sum(range(...))` IR-level peephole desugar that unblocks the
//! cmp_lua W1 bytecode + cranelift rows.
//!
//! Today `list` is a Relon-source module alias (`#import list from
//! "std/list"`) the tree-walker resolves dynamically via the host
//! module loader.  The IR pipeline has no notion of "user-imported
//! namespace", so without the peephole the receiver always falls
//! through as `UnresolvedVariable`.  Combined with `range` being a
//! tree-walker-only host fn (no IR stdlib slot), the canonical
//! hot-loop shape `list.sum(range(n))` is unreachable to the bytecode
//! + cranelift backends.  These tests pin the desugar so future
//!   refactors don't regress the lift.

use relon_bytecode::BytecodeEvaluator;
use relon_eval_api::value::Value;
use relon_eval_api::Evaluator;
use std::collections::HashMap;

fn build_eval(src: &str) -> BytecodeEvaluator {
    BytecodeEvaluator::from_source(src).expect("bytecode build should succeed post-desugar")
}

fn run_int(ev: &BytecodeEvaluator, n: i64) -> i64 {
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match ev.run_main(args).expect("run_main") {
        Value::Int(v) => v,
        other => panic!("unexpected result shape: {other:?}"),
    }
}

#[test]
fn list_sum_range_one_arg_zero_to_ten() {
    // sum(0..10) = 0+1+...+9 = 45.
    let ev = build_eval("#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))");
    assert_eq!(run_int(&ev, 10), 45);
}

#[test]
fn list_sum_range_one_arg_large() {
    // sum(0..10_000) = 10_000 * 9_999 / 2 = 49_995_000.
    // Mirrors the cmp_lua W1 workload exactly.
    let ev = build_eval("#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))");
    let n: i64 = 10_000;
    assert_eq!(run_int(&ev, n), n * (n - 1) / 2);
}

#[test]
fn list_sum_range_empty_returns_zero() {
    // Empty range: `range(0)` produces `[]`; the tree-walker returns 0.
    // The desugar's loop must take the `start >= end` branch on the
    // first iteration and never push anything onto `acc`.
    let ev = build_eval("#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))");
    assert_eq!(run_int(&ev, 0), 0);
}

#[test]
fn list_sum_range_two_arg_eval() {
    // sum(3..7) = 3+4+5+6 = 18.  Exercises the 2-arg `range(start, end)`
    // overload — the desugar lowers `start` instead of pushing a
    // ConstI64(0).
    let ev =
        build_eval("#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(3, n))");
    assert_eq!(run_int(&ev, 7), 18);
}

#[test]
fn list_sum_range_two_arg_inverted_returns_zero() {
    // `range(7, 3)` is empty (start >= end); behaviour must match
    // tree-walker (`Value::list((7..3).map(...)) == empty list`).
    let ev =
        build_eval("#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(7, n))");
    assert_eq!(run_int(&ev, 3), 0);
}

#[test]
fn list_sum_range_matches_gauss_formula_at_several_n() {
    // Sum identity is the differential anchor: the desugar must agree
    // with the closed form `n*(n-1)/2` for `range(n)` at every point
    // we check.  A drift here means the loop emitted by the desugar
    // miscounts iterations or off-by-ones the bounds — the same
    // shape the tree-walker validates against host semantics via the
    // corpus differential harness (see relon-test-harness).
    let ev = build_eval("#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))");
    for n in [0_i64, 1, 5, 100, 1_000, 10_000] {
        assert_eq!(
            run_int(&ev, n),
            n * (n - 1) / 2,
            "list.sum(range({n})) must equal n*(n-1)/2",
        );
    }
}
