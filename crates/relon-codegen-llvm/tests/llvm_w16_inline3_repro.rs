//! EMIT-INLINE repro: >= 3 closure-taking stdlib bodies inlined into a
//! single lambda body through the LLVM AOT emitter.
//!
//! Bisected from the W16 3-partition quicksort source: the where-helper
//! shape `_len(_list_filter(xs, p1)) + _len(_list_filter(xs, p2)) +
//! _len(_list_filter(xs, p3))` SIGSEGV'd through LLVM AOT, while the
//! 2-filter version returned correct values. `list.sum` had the same
//! effect (also a closure-taking inlined stdlib body).
//!
//! ## Root cause
//!
//! Each `_list_filter` inline frame emits an `Op::CallClosure` that
//! dispatches the predicate through a `switch` over the module's closure
//! table. Three filter frames bind three predicate closures; together
//! with the `sum_qs` / `count` where-helper (itself a lambda, called via
//! `CallClosure` from the entry) the module hosts >= 4 closures, so the
//! `switch` lowers to a *jump table* (a `.rodata` block of code pointers
//! indexed by `fn_table_idx`). The LLVM AOT engine JITs list-handling
//! modules under the **Small** code model, which addresses that jump
//! table through a 32-bit *absolute* reference (`jmp *table(,%idx,8)`).
//! The MCJIT memory manager `mmap`'d the table at a 64-bit address well
//! above 4 GiB, so the truncated 32-bit base pointed at unmapped memory
//! and the indirect jump SIGSEGV'd. A module with <= 3 closures stays a
//! compare-chain (no jump table), which is exactly why the 2-partition
//! kernel survived and the 3-partition / 3-filter kernels faulted.
//!
//! The fix allocates every MCJIT code / data section in the low 2 GiB
//! (`MAP_32BIT`) so the Small model's 32-bit absolute jump-table
//! reference resolves; see `mcjit_mm::ContiguousCodeMemoryManager`.
//!
//! This module reproduces the bug and proves the fix against the
//! tree-walker oracle (`relon-evaluator`, the source of truth).

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator as _, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

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
        other => panic!("return expected Int, got {other:?}"),
    }
}

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

fn llvm_n(ev: &LlvmAotEvaluator, n: i64) -> i64 {
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    extract_int(
        ev.run_main(args)
            .unwrap_or_else(|e| panic!("LLVM run_main failed for n={n}: {e:?}")),
    )
}

/// Two sequential closure-taking stdlib inline frames in one lambda
/// body (the working baseline). Counts how many elements are `< arr[0]`
/// plus how many are `> arr[0]`.
const TWO_FILTER_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     count2(arr)\n\
     where {\n\
       arr: range(0, n),\n\
       count2(xs): _len(xs) == 0 ? 0 : (\n\
         _len(_list_filter(xs, (x) => x < xs[0]))\n\
         + _len(_list_filter(xs, (x) => x > xs[0]))\n\
       )\n\
     }";

/// Three sequential closure-taking stdlib inline frames in one lambda
/// body (the repro). `< arr[0]`, `== arr[0]`, `> arr[0]` filtered then
/// counted. NO recursion — the crash reproduces at n=2.
const THREE_FILTER_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     count3(arr)\n\
     where {\n\
       arr: range(0, n),\n\
       count3(xs): _len(xs) == 0 ? 0 : (\n\
         _len(_list_filter(xs, (x) => x < xs[0]))\n\
         + _len(_list_filter(xs, (x) => x == xs[0]))\n\
         + _len(_list_filter(xs, (x) => x > xs[0]))\n\
       )\n\
     }";

/// Mixed shape closer to W16: two filters plus a `list.sum` over the
/// `== pivot` partition — three closure-taking inline frames, the last
/// being `list_int_sum` rather than a third filter+len.
const FILTER_SUM_SRC: &str = "#unstrict\n\
     #import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     mix(arr)\n\
     where {\n\
       arr: range(0, n),\n\
       mix(xs): _len(xs) == 0 ? 0 : (\n\
         _len(_list_filter(xs, (x) => x < xs[0]))\n\
         + list.sum(_list_filter(xs, (x) => x == xs[0]))\n\
         + _len(_list_filter(xs, (x) => x > xs[0]))\n\
       )\n\
     }";

#[test]
fn two_filter_inline_matches_oracle() {
    let ev =
        LlvmAotEvaluator::from_source(TWO_FILTER_SRC).expect("2-filter inline kernel compiles");
    for n in [0i64, 1, 2, 3, 5, 10, 20] {
        let got = llvm_n(&ev, n);
        let want = oracle_n(TWO_FILTER_SRC, n);
        assert_eq!(
            got, want,
            "2-filter inline LLVM AOT mismatches oracle n={n}"
        );
    }
}

#[test]
fn three_filter_inline_matches_oracle() {
    let ev =
        LlvmAotEvaluator::from_source(THREE_FILTER_SRC).expect("3-filter inline kernel compiles");
    // Runtime n at call time -> not const-foldable. n=2 is the minimal
    // crash repro; larger n exercise the same 3-inline frame setup with
    // more loop trips.
    for n in [0i64, 1, 2, 3, 5, 10, 20, 50] {
        let got = llvm_n(&ev, n);
        let want = oracle_n(THREE_FILTER_SRC, n);
        assert_eq!(
            got, want,
            "3-filter inline LLVM AOT mismatches oracle n={n}"
        );
    }
}

#[test]
fn filter_sum_inline_matches_oracle() {
    let ev =
        LlvmAotEvaluator::from_source(FILTER_SUM_SRC).expect("filter+sum 3-inline kernel compiles");
    for n in [0i64, 1, 2, 3, 5, 10, 20, 50] {
        let got = llvm_n(&ev, n);
        let want = oracle_n(FILTER_SUM_SRC, n);
        assert_eq!(
            got, want,
            "filter+sum 3-inline LLVM AOT mismatches oracle n={n}"
        );
    }
}

/// The full 3-partition W16 production recurrence: partition around the
/// head pivot, recurse on the `<` / `>` sub-lists, and sum the `==`
/// partition via `list.sum` — three closure-taking stdlib inline frames
/// (`_list_filter` x3, one feeding `list.sum`) per `sum_qs` call.
///
/// This replicates the SHAPE of `crates/relon-bench/benches/cmp_lua.rs`'s
/// `w16_relon_src` (reference only — the bench file is orchestrator-
/// owned, not edited here) minus the `range(n).map(...)` PRNG generator,
/// which is substituted with `range(0, n)` so the test stays a pure
/// `relon-ir` list-materialise + 3-partition recursion exercise. The sum
/// is sort-invariant (multiset sum), but the per-call work is the real
/// data-dependent 3-way partition recursion, not a closed-form fold.
///
/// Pre-fix this SIGSEGV'd (the `<`/`==`/`>` predicates plus the
/// `sum_qs` where-helper = 4 closures, so the `_list_filter`-internal
/// `CallClosure` lowered to a >= 4-entry jump table that MCJIT's Small
/// code model addressed past the 32-bit boundary).
const W16_QS_3PARTITION_SRC: &str = "#unstrict\n\
     #import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     sum_qs(arr)\n\
     where {\n\
       arr: range(0, n),\n\
       sum_qs(xs): _len(xs) <= 1 ? (_len(xs) == 0 ? 0 : xs[0]) : (\n\
         sum_qs(_list_filter(xs, (x) => x < xs[0]))\n\
         + list.sum(_list_filter(xs, (x) => x == xs[0]))\n\
         + sum_qs(_list_filter(xs, (x) => x > xs[0]))\n\
       )\n\
     }";

/// Run `src` on the tree-walker oracle on a 64 MiB-stack thread. The
/// W16 recurrence is O(n)-deep on the ascending `range(0, n)` input
/// (the pivot is always the minimum, so the `> pivot` partition is the
/// whole tail) and the tree-walker clones its scope per frame — the
/// default cargo-test worker stack overflows past n ~ 16. The engine
/// side recurses far more cheaply; spawning the oracle on a big stack
/// lets us cross-check at larger n without the oracle being the limiter.
fn oracle_n_bigstack(src: &'static str, n: i64) -> i64 {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || oracle_n(src, n))
        .expect("spawn big-stack oracle thread")
        .join()
        .expect("big-stack oracle thread panicked")
}

#[test]
fn w16_quicksort_3partition_matches_tree_walker_oracle() {
    let ev = LlvmAotEvaluator::from_source(W16_QS_3PARTITION_SRC).expect(
        "W16 3-partition quicksort kernel compiles via LLVM AOT \
         (range materialise + xs[0] index + 3x filter + list.sum + recursion)",
    );
    // Runtime n supplied at call time -> not const-foldable. n=0 hits the
    // empty base case, n=1 the singleton base case; the larger inputs
    // drive the materialise + index + 3-way partition recursion at
    // increasing depth (the exact shape that faulted pre-fix). n=80 keeps
    // the oracle's 64 MiB-stack ascending-range recursion comfortable.
    for n in [0i64, 1, 2, 3, 5, 10, 16, 40, 80] {
        let got = llvm_n(&ev, n);
        let want = oracle_n_bigstack(W16_QS_3PARTITION_SRC, n);
        assert_eq!(
            got, want,
            "W16 3-partition quicksort LLVM AOT mismatches tree-walker oracle for n={n}"
        );
    }
}
