//! Smoke check for the W26 / W27 workload sources (panel expansion
//! 2026-05-29, Tier 4 Phase 4 — Group C strings / formatting + Group
//! E non-list stdlib coverage).
//!
//! The bench helpers `w26_relon_src()` / `w26_lua_src()` /
//! `w26_expected()` and the W27 trio live in
//! `crates/relon-bench/benches/cmp_lua.rs` (the Criterion `[[bench]]`
//! binary), so this smoke binary inlines the source strings rather
//! than reaching across the binary-vs-binary boundary. The constants
//! and helpers here are KEPT IN SYNC with the bench file at the const
//! sites — any drift surfaces as a test mismatch at smoke time.
//!
//! Run via:
//!
//! ```sh
//! cargo run -p relon-bench --bin w26_w27_smoke
//! ```
//!
//! W26 returns scalar Int (`#main -> Int`); W27 returns scalar Int
//! (`#main -> Int`). Both checked exact-equal against analytic
//! constants computed on the fly from the bench scales.

use relon::JitEvaluator;
use relon_eval_api::{Evaluator as _, Value};
use std::collections::HashMap;

// Kept in sync with crates/relon-bench/benches/cmp_lua.rs.
const W26_N: i64 = 1_000;
const W27_N: i64 = 10_000;

fn w26_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Int\n\
     range(n).reduce(0, (acc, i) => acc + _len(f\"item ${i} of ${n}\"))"
}

fn w27_src() -> &'static str {
    "#unstrict\n\
     #import dict from \"std/dict\"\n\
     #main(Int n) -> Int\n\
     range(n).reduce(0, (acc, _) => acc + _len(dict.keys({ a: 1, b: 2, c: 3 })))"
}

fn run_int(label: &str, src: &str, n: i64) -> i64 {
    let jit = JitEvaluator::new(src)
        .unwrap_or_else(|e| panic!("{label}: setup failed:\n{src}\nerr: {e}"));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    let v = jit
        .run_main(args)
        .unwrap_or_else(|e| panic!("{label}: run_main failed: {e}"));
    match v {
        Value::Int(n) => n,
        Value::Dict(d) => match d.map.get("result") {
            Some(Value::Int(n)) => *n,
            other => panic!("{label}: dict.result is not Int: {other:?}"),
        },
        other => panic!("{label}: non-Int return: {other:?}"),
    }
}

fn decimal_len(n: i64) -> u32 {
    if n == 0 {
        1
    } else {
        (n as f64).log10().floor() as u32 + 1
    }
}

fn w26_expected() -> i64 {
    // Per-iter byte count = 5 ("item ") + len(str(i)) + 4 (" of ")
    //                     + len(str(n))
    let n = W26_N;
    let n_len = decimal_len(n) as i64;
    let mut total: i64 = 0;
    for i in 0..n {
        total += 5 + decimal_len(i) as i64 + 4 + n_len;
    }
    total
}

fn w27_expected() -> i64 {
    // dict.keys({a:1,b:2,c:3}) returns a List<String> of length 3;
    // `_len(...)` returns 3; reduce over n iters → n * 3.
    W27_N * 3
}

fn main() {
    let v26 = run_int("W26", w26_src(), W26_N);
    let e26 = w26_expected();
    println!(
        "W26_fstring_interp: got={v26} expected={e26} ok={}",
        v26 == e26
    );
    assert_eq!(v26, e26, "W26 mismatch");

    let v27 = run_int("W27", w27_src(), W27_N);
    let e27 = w27_expected();
    println!(
        "W27_stdlib_dict:    got={v27} expected={e27} ok={}",
        v27 == e27
    );
    assert_eq!(v27, e27, "W27 mismatch");

    println!("\nALL W26 / W27 smoke checks passed.");
}
