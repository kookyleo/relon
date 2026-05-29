//! AOT-4 (W16 slice): end-to-end proof that the W16 quicksort-sum
//! kernel's core shape — materialise a runtime `range(...)` into a
//! `List<Int>`, index it (`xs[0]`), partition it via `_list_filter`,
//! and recurse on the filtered sub-lists — compiles and runs through
//! the real LLVM-18 AOT pipeline and agrees with the tree-walker
//! oracle (`relon-evaluator`, the source of truth).
//!
//! The W16 production source (`crates/relon-bench/benches/cmp_lua.rs`'s
//! `w16_relon_src`, reference only — the bench file is orchestrator-
//! owned) is the recursive sum-via-partition recurrence
//!
//! ```text
//! sum_qs(xs): _len(xs) <= 1 ? (_len(xs) == 0 ? 0 : xs[0]) : (
//!   sum_qs(_list_filter(xs, (x) => x < xs[0]))
//!   + list.sum(_list_filter(xs, (x) => x == xs[0]))
//!   + sum_qs(_list_filter(xs, (x) => x > xs[0])))
//! ```
//!
//! Pre-AOT-4 this rejected at IR lowering: a where-bound `range(...)`
//! never materialised (the eliding range peepholes only cover the
//! fusable `.sum` / `.len` / `.reduce` terminals); the `xs[i]` index
//! arm rejected a `List<Int>` receiver; and `_len(xs)` /
//! `_list_filter(xs, f)` / `list.sum(xs)` over a materialised handle
//! had no lowering.
//!
//! AOT-4 (this slice) adds, in `relon-ir` lowering:
//!   * where-bound `range(a, b)` -> `List<Int>` materialisation
//!     (reuses the W18 `emit_range_materialize`);
//!   * 1D `xs[i]` index on a `List<Int>` receiver — inline payload
//!     addressing (`payload = (base + 11) & -8`, `addr = payload +
//!     i*8`, `Op::LoadI64AtAbsolute { offset: 0 }`), no bounds branch
//!     (the kernel guards `_len(xs) <= 1` before `xs[0]`), NOT
//!     `Op::ListGetByIntIdx`;
//!   * general `_len(xs)` / `_list_filter(xs, f)` / `list.sum(xs)` over
//!     an arbitrary `List<Int>` handle (`Op::ReadStringLen` /
//!     `Op::Call(list_int_filter)` / `Op::Call(list_int_sum)`);
//!   * `List<Int>` recursive-helper param-type inference so an
//!     unannotated `sum_qs(xs)` whose body uses `xs` as a list takes a
//!     `List<Int>` handle and the recursive list arg type-checks;
//!   * a global closure-table `fn_table_idx` fix so a predicate lambda
//!     built *inside* the recursive helper dispatches to itself, not to
//!     the helper.
//!
//! ## Shape coverage / known limit
//! The full 3-partition production source (`<`, `==`, `>` in one frame)
//! inlines three closure-taking stdlib bodies (`list_int_filter` /
//! `list_int_sum`) into a single lambda body; the LLVM emitter's
//! stdlib-inline frame currently faults at runtime with three or more
//! such inlines per lambda (two work) — a pre-existing emitter
//! limitation independent of this lowering slice. This test therefore
//! proves the genuine **2-partition** quicksort-sum recurrence
//!
//! ```text
//! qs(xs): _len(xs) <= 1 ? (_len(xs) == 0 ? 0 : xs[0]) : (
//!   qs(_list_filter(xs, (x) => x < xs[0]))
//!   + xs[0]
//!   + qs(_list_filter(xs, (x) => x > xs[0])))
//! ```
//!
//! which exercises every AOT-4 capability above (materialise + index +
//! filter + recursion-on-filtered-sub-list, two filters per frame). The
//! pivot `xs[0]` is summed in place (the `== pivot` run is a single
//! element on a distinct-element input). The result is the multiset sum
//! — sort-invariant — so it equals `sum(0..n)` for `arr = range(0, n)`,
//! but the per-call work is the data-dependent partition recursion, not
//! a closed-form fold (no algorithm substitution).
//!
//! Runtime `n` is supplied at call time, so the optimiser cannot
//! const-fold the result; the assertion is against the tree-walker
//! oracle, not a hand-written closed form.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator as _, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// 2-partition quicksort-sum kernel. `arr` is a where-bound materialised
/// `range(0, n)`; `qs` recurses on the `<`/`>` filtered sub-lists and
/// adds the pivot `xs[0]` in place. Verbatim shape coverage of the W16
/// production recurrence minus the `== pivot` `list.sum` middle term
/// (which is the single pivot element on a distinct-element input, so
/// `xs[0]` is the same value).
const W16_QS_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     qs(arr)\n\
     where {\n\
       arr: range(0, n),\n\
       qs(xs): _len(xs) <= 1 ? (_len(xs) == 0 ? 0 : xs[0]) : (\n\
         qs(_list_filter(xs, (x) => x < xs[0]))\n\
         + xs[0]\n\
         + qs(_list_filter(xs, (x) => x > xs[0]))\n\
       )\n\
     }";

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

fn extract_int(v: Value) -> i64 {
    match v {
        Value::Int(i) => i,
        other => panic!("W16 return expected Int, got {other:?}"),
    }
}

/// Run `src` on the tree-walker oracle (`relon-evaluator`) with a single
/// runtime `Int n` argument — the SOURCE OF TRUTH the JIT must match.
fn oracle_n(src: &str, n: i64) -> i64 {
    let (walker, scope) = build_tree_walker(src);
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    extract_int(
        walker
            .run_main(&scope, args)
            .unwrap_or_else(|e| panic!("tree-walker run_main failed for n={n}: {e:?}")),
    )
}

/// Run `src` on the LLVM-18 AOT JIT with a single runtime `Int n`.
fn llvm_n(ev: &LlvmAotEvaluator, n: i64) -> i64 {
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    extract_int(
        ev.run_main(args)
            .unwrap_or_else(|e| panic!("LLVM run_main failed for n={n}: {e:?}")),
    )
}

#[test]
fn w16_quicksort_2partition_matches_tree_walker_oracle() {
    let ev = LlvmAotEvaluator::from_source(W16_QS_SRC).expect(
        "W16 2-partition quicksort kernel compiles via LLVM AOT \
         (range materialise + xs[0] index + filter + recursion on filtered sub-list)",
    );

    // Runtime n supplied at call time -> not const-foldable. n=0 hits
    // the empty-list base case; n=1 the singleton base case; the larger
    // inputs drive the materialise + index + filter + sub-list recursion
    // at increasing depth.
    //
    // n is capped at 12 here because the *oracle* (the tree-walker) is
    // the stack-bound side: `arr = range(0, n)` is ascending, so the
    // pivot is always the minimum and the `> pivot` partition is the
    // whole tail — an O(n)-deep recursion. The tree-walker clones its
    // scope per frame, which overflows the small cargo-test worker-
    // thread stack past n ~ 16. The LLVM AOT side recurses far more
    // cheaply (and the 1 MiB per-dispatch bump arena holds the per-frame
    // sub-lists well past n=12); n=12 keeps BOTH sides comfortable while
    // still driving real multi-level partition recursion. The bench
    // (`cmp_lua.rs` W16) runs n=1000 on a PRNG-shuffled input where the
    // expected depth is O(log n) ~ 10 — not reproduced here because the
    // tree-walker oracle path is the limiter, not the engine.
    for n in [0i64, 1, 2, 3, 5, 10, 12] {
        let got = llvm_n(&ev, n);
        let want = oracle_n(W16_QS_SRC, n);
        assert_eq!(
            got, want,
            "W16 2-partition quicksort LLVM AOT result mismatches tree-walker oracle for n={n}"
        );
    }
}

/// Narrower pin: a where-bound materialised `range` indexed at a runtime
/// position, isolating the 1D `xs[i]` index path from the recursion /
/// filter composition so a regression in either localises cleanly. The
/// `_len <= 1` guard keeps the index in-bounds (matching the kernel's
/// no-bounds-branch posture).
const INDEX_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     (_len(arr) <= 2 ? 0 : arr[2]) where { arr: range(0, n) }";

#[test]
fn list_int_index_matches_oracle() {
    let ev = LlvmAotEvaluator::from_source(INDEX_SRC).expect("range-materialise + index compiles");
    for n in [0i64, 1, 2, 3, 7, 20] {
        let got = llvm_n(&ev, n);
        let want = oracle_n(INDEX_SRC, n);
        assert_eq!(got, want, "list index LLVM AOT mismatches oracle for n={n}");
    }
}

/// Narrower pin: a recursive `List<Int>` helper that peels the head and
/// recurses on the strictly-greater filtered sub-list. Isolates the
/// filter + recursion-on-filtered-sub-list composition (one filter per
/// frame) from the 2-partition kernel above.
const FILTER_REC_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     sum_lt(arr) where { arr: range(0, n), \
     sum_lt(xs): _len(xs) == 0 ? 0 : (xs[0] + sum_lt(_list_filter(xs, (x) => x > xs[0]))) }";

#[test]
fn filter_recursion_matches_oracle() {
    let ev = LlvmAotEvaluator::from_source(FILTER_REC_SRC)
        .expect("filter + recursion-on-filtered-sub-list compiles");
    // n capped at 12 for the same tree-walker-oracle stack reason as the
    // 2-partition kernel: `sum_lt` peels the head and recurses on the
    // `> head` tail, an O(n)-deep recursion on the ascending `range`
    // input, and the oracle clones its scope per frame.
    for n in [0i64, 1, 2, 5, 10, 12] {
        let got = llvm_n(&ev, n);
        let want = oracle_n(FILTER_REC_SRC, n);
        assert_eq!(
            got, want,
            "filter-recursion LLVM AOT mismatches oracle for n={n}"
        );
    }
}
