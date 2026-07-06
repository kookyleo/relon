//! AOT-3: end-to-end smoke for a W17-shaped where-bound recursive
//! helper routed through the real LLVM-18 AOT pipeline.
//!
//! The cmp_lua W17 source declares `bs` (binary search) as a recursive
//! helper bound in a `where { ... }` clause and called from a `reduce`
//! fold:
//!
//! ```text
//! #unstrict
//! #main(Int n) -> Int
//! range(n).reduce(0, (acc, i) => acc + bs(0, n, (i * 31) % n))
//! where {
//!   bs(lo, hi, t): hi - lo <= 1 ? lo : (
//!     (lo + hi) / 2 <= t
//!       ? bs((lo + hi) / 2, hi, t)
//!       : bs(lo, (lo + hi) / 2, t)
//!   )
//! }
//! ```
//!
//! Pre-AOT-3 this rejected at IR lowering with `ClosureAcrossBoundary`
//! ("closure used in a non-higher-order position") — the `bs` closure
//! binding in the `where` clause hit the bare `Expr::Closure` arm of
//! `lower_expr`. AOT-3 lifts the where-bound recursive helper to a
//! top-level let-bound closure, exactly the way the W7 anon-Dict-return
//! path lifts its `#internal fib: (Int k) -> Int => ...` field.
//!
//! W17 is PURE recursion over an arithmetic index range (no list
//! materialisation), so the lowering change alone lands it. This test
//! asserts the JIT result agrees with the tree-walker-style oracle for
//! several runtime inputs the optimizer cannot const-fold (the `#main`
//! argument `n` is supplied at call time).

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// Reference oracle: the SAME bisection algorithm the Relon source
/// runs, expressed in plain Rust. Bisects `[lo, hi)` for `t`; the base
/// case (`hi - lo <= 1`) returns `lo`. The outer fold sums `bs(0, n,
/// (i * 31) % n)` over `i in [0, n)`.
fn bs_oracle(lo: i64, hi: i64, t: i64) -> i64 {
    if hi - lo <= 1 {
        return lo;
    }
    let mid = (lo + hi) / 2;
    if mid <= t {
        bs_oracle(mid, hi, t)
    } else {
        bs_oracle(lo, mid, t)
    }
}

fn w17_oracle(n: i64) -> i64 {
    let mut acc = 0i64;
    for i in 0..n {
        acc += bs_oracle(0, n, (i.wrapping_mul(31)) % n);
    }
    acc
}

fn extract_int(v: Value) -> i64 {
    match v {
        Value::Int(i) => i,
        other => panic!("W17 return expected Int, got {other:?}"),
    }
}

const W17_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     range(n).reduce(0, (acc, i) => acc + bs(0, n, (i * 31) % n))\n\
     where {\n\
       bs(lo, hi, t): hi - lo <= 1 ? lo : (\n\
         (lo + hi) / 2 <= t\n\
           ? bs((lo + hi) / 2, hi, t)\n\
           : bs(lo, (lo + hi) / 2, t)\n\
       )\n\
     }";

#[test]
fn w17_where_bound_recursive_helper_lowers_and_evaluates() {
    let ev = LlvmAotEvaluator::from_source(W17_SRC)
        .expect("W17 where-bound recursive helper compiles via LLVM AOT");
    // Runtime inputs (supplied at call time -> not const-foldable). The
    // `(i * 31) % n` scrambling defeats any closed-form fold the
    // optimizer might otherwise recognise for `range(n)`.
    for n in [1i64, 2, 3, 5, 8, 16, 50, 100] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let got = extract_int(ev.run_main(args).expect("run_main"));
        let want = w17_oracle(n);
        assert_eq!(
            got, want,
            "W17 binary-search LLVM AOT result mismatches tree-walker oracle for n={n}"
        );
    }
}
