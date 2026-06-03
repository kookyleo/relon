//! Quick smoke check for W28 / W30 workload sources (panel expansion
//! 2026-05-29, Tier 4 Phase 3 — Group D numeric + Group F strict).
//!
//! W29_null_coalesce is omitted: the `??` null-coalesce operator is
//! NOT implemented in the parser today (verified via the bench-side
//! probe — every form of `?? default` returns `parse error: expected
//! expression`). Per the Tier 4 Phase 3 spec, the workload is marked
//! a TRUE BLOCKER until the parser / IR / evaluator grow the operator.
//! The blocker is reported in the Phase 3 hand-off so a follow-up
//! phase can pick it up after the operator lands.
//!
//! The bench helpers `wN_relon_src()` / `wN_lua_src()` /
//! `wN_expected()` live in `crates/relon-bench/benches/cmp_lua.rs`
//! (the Criterion `[[bench]]` binary, not the library), so this
//! smoke binary inlines the source strings rather than reaching
//! across the binary-vs-binary boundary. The constants here are
//! KEPT IN SYNC with the bench file at the const sites — any drift
//! between the two surfaces a test mismatch at smoke time.
//!
//! Run via:
//!
//! ```sh
//! cargo run -p relon-bench --bin w28_w29_w30_smoke
//! ```
//!
//! W28 returns Float (Int+Float mixed reduce; absolute-tolerance
//! check because the tree-walker's evaluation order may differ from
//! `rustc`'s by ~1 ULP per iter). W30 returns Int (closed-form
//! `n*(n+1)/2`, exact-equal check).

use relon::AutoEvaluator;
use relon_eval_api::{Evaluator as _, Value};
use std::collections::HashMap;

const W28_N: i64 = 10_000;
const W30_N: i64 = 10_000;
/// W28 absolute tolerance — see `cmp_lua.rs::W28_FLOAT_TOL`.
const W28_FLOAT_TOL: f64 = 1.0e-6;

fn w28_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Float\n\
     range(n).reduce(0.0, (acc, i) => acc + i / 3.0 + i % 7)"
}

fn w30_src() -> &'static str {
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((Int i) => i + 1))"
}

fn run_int(label: &str, src: &str, n: i64) -> i64 {
    let jit = AutoEvaluator::new(src)
        .unwrap_or_else(|e| panic!("{label}: setup failed:\n{src}\nerr: {e}"));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match jit
        .run_main(args)
        .unwrap_or_else(|e| panic!("{label}: run_main failed: {e}"))
    {
        Value::Int(v) => v,
        other => panic!("{label}: non-Int return: {other:?}"),
    }
}

fn run_float(label: &str, src: &str, n: i64) -> f64 {
    let jit = AutoEvaluator::new(src)
        .unwrap_or_else(|e| panic!("{label}: setup failed:\n{src}\nerr: {e}"));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match jit
        .run_main(args)
        .unwrap_or_else(|e| panic!("{label}: run_main failed: {e}"))
    {
        Value::Float(f) => f.into_inner(),
        other => panic!("{label}: non-Float return: {other:?}"),
    }
}

fn w28_expected() -> f64 {
    // Iterative reference matching the bench source's per-iter shape.
    let n: i64 = W28_N;
    let mut acc = 0.0_f64;
    let mut i: i64 = 0;
    while i < n {
        acc = acc + (i as f64) / 3.0 + ((i % 7) as f64);
        i += 1;
    }
    acc
}

fn w30_expected() -> i64 {
    let n: i64 = W30_N;
    n * (n + 1) / 2
}

fn main() {
    let v28 = run_float("W28", w28_src(), W28_N);
    let e28 = w28_expected();
    let abs = (v28 - e28).abs();
    println!(
        "W28_float_mixed_ops:      got={v28} expected={e28} abs_err={abs:.3e} tol={W28_FLOAT_TOL:.3e} \
         ok={}",
        abs < W28_FLOAT_TOL
    );
    assert!(
        abs < W28_FLOAT_TOL,
        "W28 mismatch: {v28} vs expected {e28} (abs_err {abs:.3e} >= tol {W28_FLOAT_TOL:.3e})"
    );

    let v30 = run_int("W30", w30_src(), W30_N);
    let e30 = w30_expected();
    println!(
        "W30_strict_mode_baseline: got={v30} expected={e30} ok={}",
        v30 == e30
    );
    assert_eq!(v30, e30, "W30 mismatch");

    println!(
        "\nNOTE: W29_null_coalesce is TRUE BLOCKER (?? operator not implemented \
         in parser / IR / evaluator). No smoke entry."
    );
    println!("\nALL W28/W30 smoke checks passed.");
}
