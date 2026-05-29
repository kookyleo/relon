//! AOT-4 (W18 slice): end-to-end smoke for the W18 prime-count kernel
//! routed through the real LLVM-18 AOT pipeline.
//!
//! The cmp_lua W18 source counts primes in `[2, n)` via trial division.
//! `range(2, n)` is the candidate stream, `_list_filter` keeps the
//! survivors that pass a where-bound recursive `is_prime(k, d)`
//! predicate, and `_len` returns the survivor count:
//!
//! ```text
//! #unstrict
//! #main(Int n) -> Int
//! _len(_list_filter(range(2, n), (k) => is_prime(k, 2)))
//! where {
//!   is_prime(k, d): d * d > k ? true : (k % d == 0 ? false : is_prime(k, d + 1))
//! }
//! ```
//!
//! Pre-AOT-4 this rejected at IR lowering: `range(2, n)` consumed by
//! `_list_filter` never materialised (the eliding range peepholes only
//! cover the fusable `.sum` / `.len` / `.reduce` terminals), and `_len`
//! / `_list_filter` are tree-walker host intrinsics with no bundled IR
//! stdlib slot.
//!
//! AOT-4 adds a range-materialise lowering: `range(2, n)` is built into
//! a `List<Int>` scratch record (`AllocScratchDyn` + `[len][pad][i64
//! ...]` fill loop), handed to the bundled `list_int_filter` body via a
//! real `Op::Call`, and the survivor count is read with
//! `Op::ReadStringLen`. The where-bound recursive `is_prime` lifts via
//! AOT-3 and returns `Bool` (inferred from its ternary body).
//!
//! This test asserts the JIT result agrees with a plain-Rust
//! tree-walker-style oracle for several runtime `n` the optimizer
//! cannot const-fold (the `#main` argument `n` is supplied at call
//! time). `pi(n)` is not a closed-form polynomial in `n`, so even an
//! aggressive optimiser cannot reduce the survivor count to a literal.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// Reference oracle: the SAME trial-division primality test the Relon
/// source runs, expressed in plain Rust. Counts primes in `[2, n)`.
fn is_prime_oracle(k: i64) -> bool {
    let mut d: i64 = 2;
    while d.saturating_mul(d) <= k {
        if k % d == 0 {
            return false;
        }
        d += 1;
    }
    true
}

fn w18_oracle(n: i64) -> i64 {
    let mut count: i64 = 0;
    let mut k: i64 = 2;
    while k < n {
        if is_prime_oracle(k) {
            count += 1;
        }
        k += 1;
    }
    count
}

fn extract_int(v: Value) -> i64 {
    match v {
        Value::Int(i) => i,
        other => panic!("W18 return expected Int, got {other:?}"),
    }
}

/// Verbatim copy of `crates/relon-bench/benches/cmp_lua.rs`'s
/// `w18_relon_src()` production shape (reference only — the bench file
/// is orchestrator-owned and not edited here).
const W18_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     _len(_list_filter(range(2, n), (k) => is_prime(k, 2)))\n\
     where {\n\
       is_prime(k, d): d * d > k ? true : (k % d == 0 ? false : is_prime(k, d + 1))\n\
     }";

#[test]
fn w18_prime_count_materialize_filter_length_matches_oracle() {
    let ev = LlvmAotEvaluator::from_source(W18_SRC)
        .expect("W18 prime-count source compiles via LLVM AOT (range materialise + filter + len)");
    // Runtime inputs supplied at call time -> not const-foldable. n=2
    // exercises the empty-list edge (`range(2, 2)` -> `[]` -> 0
    // primes); the larger inputs cover the materialise + filter +
    // recursion path with non-trivial survivor subsets.
    for n in [2i64, 3, 4, 5, 10, 30, 50, 100] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let got = extract_int(ev.run_main(args).expect("run_main"));
        let want = w18_oracle(n);
        assert_eq!(
            got, want,
            "W18 prime-count LLVM AOT result mismatches tree-walker oracle for n={n}"
        );
    }
}

/// Narrower slice: materialise + filter + length WITHOUT the where-bound
/// recursive helper. Pins the AOT-4 range-materialise path independent
/// of the AOT-3 recursive-lift composition, so a regression in either
/// layer localises cleanly. The predicate `k % 2 == 0` keeps the even
/// candidates in `[0, n)`.
const EVEN_COUNT_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     _len(_list_filter(range(0, n), (k) => k % 2 == 0))";

fn even_count_oracle(n: i64) -> i64 {
    (0..n).filter(|k| k % 2 == 0).count() as i64
}

#[test]
fn even_count_materialize_filter_length_matches_oracle() {
    let ev = LlvmAotEvaluator::from_source(EVEN_COUNT_SRC)
        .expect("materialise + filter + len (no recursion) compiles via LLVM AOT");
    for n in [0i64, 1, 2, 3, 8, 17, 64] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let got = extract_int(ev.run_main(args).expect("run_main"));
        let want = even_count_oracle(n);
        assert_eq!(
            got, want,
            "even-count LLVM AOT result mismatches oracle for n={n}"
        );
    }
}

/// CODEGEN-QUALITY mechanistic proof: the W18 per-element closure
/// dispatch is devirtualised. The `_list_filter` predicate is a literal
/// `MakeClosure` with a statically-known `fn_table_idx`, and the
/// predicate's `is_prime` call is a capture of another known closure, so
/// both `Op::CallClosure`s route through `emit_call_closure_direct`
/// instead of the runtime `switch i32 %cc_fn_idx`. The post-O3 IR must
/// therefore contain NO closure-dispatch switch in the hot loop — LLVM
/// inlines the direct calls, leaving straight-line primality math.
///
/// This pins the mechanism the perf work targets: pre-change the hot
/// loop did TWO `switch i32 %cc_fn_idx` dispatches per element (filter →
/// predicate, then predicate → is_prime) before any arithmetic.
#[test]
fn w18_per_element_closure_dispatch_is_devirtualized() {
    let ev = LlvmAotEvaluator::from_source(W18_SRC).expect("W18 compiles via LLVM AOT");
    let dump = ev.emit_ir_dump();
    // The closure-dispatch slow path names its loaded selector
    // `cc_fn_idx` and the dispatch `switch i32 %cc_fn_idx`. Devirtualised
    // calls instead emit `ccd_call` (direct) with no selector load.
    assert!(
        !dump.contains("cc_fn_idx"),
        "W18 post-O3 IR still loads a closure `cc_fn_idx` selector — \
         the per-element dispatch did NOT devirtualise:\n{dump}"
    );
    assert!(
        !dump.contains("switch i32"),
        "W18 post-O3 IR still contains a `switch i32` closure dispatch — \
         devirtualisation did not fire:\n{dump}"
    );
}

/// Mechanistic proof for the no-capture predicate (the even-count
/// filter `(k) => k % 2 == 0`): a literal `MakeClosure` passed straight
/// into `_list_filter` must still devirtualise even though the predicate
/// captures nothing. Guards the inline-frame-param provenance hop.
#[test]
fn even_count_predicate_dispatch_is_devirtualized() {
    let ev = LlvmAotEvaluator::from_source(EVEN_COUNT_SRC).expect("even-count compiles");
    let dump = ev.emit_ir_dump();
    assert!(
        !dump.contains("cc_fn_idx"),
        "even-count post-O3 IR still loads a closure `cc_fn_idx` selector — \
         the filter predicate dispatch did NOT devirtualise:\n{dump}"
    );
}
