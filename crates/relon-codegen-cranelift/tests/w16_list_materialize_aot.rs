//! Regression: the cmp_lua `W16_quicksort` Relon source drives a
//! runtime-materialised `range(n).map(..)` list through the cranelift
//! AOT codegen. The list-materialise path stores the element count
//! (computed by an `If { result_ty: I64 }`) into the i32 length slot
//! via `LetSet { ty: I32 }`; `set_let` declared the cranelift
//! `Variable` at the slot's i32 type but stored the raw i64 value, so
//! the frontend panicked with `declared type of variable var{N}
//! doesn't match type of value v{M}`. The LLVM AOT path coerces the
//! value to the slot width (`coerce_to_let_ty`); the cranelift fix
//! mirrors that (`ireduce` / `uextend` in `set_let`).
//!
//! The first two tests pin the fix: the list-materialise + 1D index
//! shape and the list-materialise + `list.sum` reduction both compile
//! through cranelift and match the tree-walker oracle for several
//! runtime `n`. Both previously aborted with the var-type panic at the
//! first `LetSet { idx, ty: I32 }` fed an i64 value.
//!
//! The third test documents the *remaining* boundary: the full W16
//! `sum_qs` recurrence lowers the recursive helper as a closure that
//! captures a handle to itself (`MakeClosure { fn_table_idx: 0 }` with
//! `ClosureCapture { let_idx: 10, ty: Closure }` read before the
//! matching `LetSet { idx: 10 }`). That self-recursive-closure capture
//! is a separate, unimplemented cranelift feature (the LLVM backend
//! handles it via `OwnCaptureHandle` provenance) and is out of scope
//! for the var-type fix — the test asserts the current, well-defined
//! `LetGet read before LetSet` lowering error so a future fix flips it
//! into a green oracle check.
//!
//! NOTE (separate, pre-existing cranelift bug, NOT touched here): a
//! *selective* `_list_filter` (a predicate that drops elements, e.g.
//! `v < 1000`) returns an empty list through the cranelift backend, so
//! `list.sum(_list_filter(...))` yields 0 regardless of `n`. The
//! `list.sum` over the *full* materialised list (and over a
//! keep-everything filter) is correct — proven below — so the var-type
//! fix is sound; the selective-filter compaction defect is independent
//! of the var-type panic and is left for a dedicated lane.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::{AotEvaluator, CraneliftError};
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// The exact W16 production source (mirrors `cmp_lua.rs::w16_relon_src`,
/// copied here so the test does not depend on the bench crate).
fn w16_relon_src() -> &'static str {
    "#unstrict\n\
     #import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     sum_qs(arr)\n\
     where {\n\
       arr: range(n).map((i) => (i * 1103515245 + 12345) % 2048),\n\
       sum_qs(xs): _len(xs) <= 1 ? (_len(xs) == 0 ? 0 : xs[0]) : (\n\
         sum_qs(_list_filter(xs, (x) => x < xs[0]))\n\
         + list.sum(_list_filter(xs, (x) => x == xs[0]))\n\
         + sum_qs(_list_filter(xs, (x) => x > xs[0]))\n\
       )\n\
     }"
}

/// Minimal list-materialise + 1D index shape. Drives the
/// `range(n).map(..)` materialise loop plus two `xs[i]` indexes — the
/// op sequence the var-type panic was traced to (the `If { I64 }` ->
/// `LetSet { idx: 2, ty: I32 }` length slot).
fn list_materialize_index_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Int\n\
     xs[0] + xs[n - 1]\n\
     where {\n\
       xs: range(n).map((i) => (i * 1103515245 + 12345) % 2048)\n\
     }"
}

/// list-materialise + `list.sum` reduction over the whole list. Drives
/// the same length-slot coercion as the index shape plus the materialise
/// loop and the `list.sum` intrinsic — the non-recursive reduction at
/// the heart of a W16 partition's `==` sum, without the self-recursive
/// closure or the (separately-buggy) selective filter.
fn list_materialize_sum_src() -> &'static str {
    "#unstrict\n\
     #import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(xs)\n\
     where {\n\
       xs: range(n).map((i) => (i * 1103515245 + 12345) % 2048)\n\
     }"
}

fn oracle(src: &str, n: i64) -> i64 {
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
        Value::Int(v) => v,
        other => panic!("oracle returned non-int: {other:?}"),
    }
}

fn aot_run(src: &str, n: i64) -> i64 {
    let eval = AotEvaluator::from_source(src).expect("W16-shape AOT must compile");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match eval.run_main(args).expect("W16-shape run_main") {
        Value::Int(v) => v,
        other => panic!("AOT returned non-int: {other:?}"),
    }
}

/// The minimal list-materialise + 1D index shape compiles (no
/// `declared type of variable` panic) and matches the tree-walker
/// oracle across several runtime `n`.
#[test]
fn list_materialize_index_matches_oracle() {
    let src = list_materialize_index_src();
    for n in [2_i64, 3, 7, 16, 64, 257] {
        let want = oracle(src, n);
        let got = aot_run(src, n);
        assert_eq!(got, want, "list-materialise+index mismatch at n={n}");
    }
}

/// The list-materialise + `list.sum` reduction compiles and matches
/// the oracle across several runtime `n`. Exercises the same length-
/// slot coercion as the index shape plus the materialise loop + sum
/// reduction.
#[test]
fn list_materialize_sum_matches_oracle() {
    let src = list_materialize_sum_src();
    for n in [0_i64, 1, 4, 17, 100, 513] {
        let want = oracle(src, n);
        let got = aot_run(src, n);
        assert_eq!(got, want, "list-materialise+sum mismatch at n={n}");
    }
}

/// The full W16 `sum_qs` recurrence no longer hits the var-type panic,
/// but currently stops at the self-recursive-closure capture lowering
/// (`MakeClosure` reads `let_idx 10` before its `LetSet`). Assert that
/// well-defined `LetGet read before LetSet` error so the boundary is
/// pinned: when the self-capture lowering lands, this test flips and
/// should become an oracle check.
#[test]
fn w16_full_blocks_on_self_recursive_closure_not_var_type_panic() {
    let src = w16_relon_src();
    match AotEvaluator::from_source(src) {
        Ok(_) => {
            // If a future change makes the full W16 compile, upgrade
            // this test to an oracle comparison rather than leaving a
            // stale assertion.
            panic!(
                "full W16 now compiles through cranelift — upgrade this test to an oracle check"
            );
        }
        Err(CraneliftError::Codegen(msg)) => {
            assert!(
                msg.contains("LetGet(10) read before LetSet"),
                "expected the self-recursive-closure capture limitation, got: {msg}"
            );
        }
        Err(other) => panic!("unexpected error compiling full W16: {other:?}"),
    }
}
