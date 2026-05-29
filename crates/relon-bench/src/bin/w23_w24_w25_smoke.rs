//! Quick smoke check for the Tier 4 Phase 2 workload sources
//! (Group B container construction sugar — W23 / W24 / W25).
//!
//! W25 lands in its follow-up commit; this binary covers W23 + W24
//! today and grows the W25 entry once the workload commits.
//!
//! The bench helpers `wN_relon_src()` / `wN_lua_src()` / `wN_expected()`
//! live in `crates/relon-bench/benches/cmp_lua.rs` (the Criterion
//! `[[bench]]` binary), so this smoke binary inlines the source strings
//! rather than reaching across the binary-vs-binary boundary. The
//! constants here are KEPT IN SYNC with the bench file at the const
//! sites — any drift surfaces as a test mismatch at smoke time.
//!
//! Run via:
//!
//! ```sh
//! cargo run -p relon-bench --bin w23_w24_w25_smoke
//! ```
//!
//! All Phase 2 workloads return Int directly (`#main -> Int`); exact-
//! equal check against the analytic constant.

use relon::JitEvaluator;
use relon_eval_api::{Evaluator as _, Value};
use std::collections::HashMap;

const W23_N: i64 = 10_000;
const W24_N: i64 = 10_000;

fn w23_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Int\n\
     range(n).reduce(0, (acc, _) =>\n\
       acc + _len({ ...base, e: 5 }))\n\
     where {\n\
       base: { a: 1, b: 2, c: 3, d: 4 }\n\
     }"
}

fn w24_src() -> &'static str {
    "#unstrict\n\
     #import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum([x * 2 for x in range(n) if x % 3 == 0])"
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
        other => panic!("{label}: non-Int return: {other:?}"),
    }
}

fn w23_expected() -> i64 {
    W23_N * 5
}

fn w24_expected() -> i64 {
    let n = W24_N;
    let count = (n + 2) / 3;
    let last_kept = (count - 1) * 3;
    let sum_kept = last_kept * count / 2;
    2 * sum_kept
}

fn main() {
    let v23 = run_int("W23", w23_src(), W23_N);
    let e23 = w23_expected();
    println!(
        "W23_dict_spread: got={v23} expected={e23} ok={}",
        v23 == e23
    );
    assert_eq!(v23, e23, "W23 mismatch");

    let v24 = run_int("W24", w24_src(), W24_N);
    let e24 = w24_expected();
    println!(
        "W24_list_comprehension: got={v24} expected={e24} ok={}",
        v24 == e24
    );
    assert_eq!(v24, e24, "W24 mismatch");

    println!("\nALL Tier 4 Phase 2 smoke checks passed.");
}
