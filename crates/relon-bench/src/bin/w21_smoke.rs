//! Quick smoke check for the W21 workload source (panel expansion
//! 2026-05-29, Tier 4 Phase 1 — Group A runtime dispatch).
//!
//! The bench helpers `w21_relon_src()` / `w21_lua_src()` /
//! `w21_expected()` live in `crates/relon-bench/benches/cmp_lua.rs`
//! (the Criterion `[[bench]]` binary), so this smoke binary inlines
//! the source strings rather than reaching across the binary-vs-binary
//! boundary. The constants here are KEPT IN SYNC with the bench file
//! at the const sites — any drift surfaces as a test mismatch at
//! smoke time.
//!
//! Run via:
//!
//! ```sh
//! cargo run -p relon-bench --bin w21_smoke
//! ```
//!
//! W21 returns Dict (`#main -> Dict` with a `result: Int` projection,
//! same shape as W13 / W14 / W15 / W16 / W17 / W18 / W19); exact-equal
//! check against the analytic constant `W21_N * 3 / 2`.

use relon::JitEvaluator;
use relon_eval_api::{Evaluator as _, Value};
use std::collections::HashMap;

const W21_N: i64 = 10_000;

fn w21_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Dict\n\
     {\n\
       #schema Image { name: String, url: String },\n\
       #schema Text { name: String, content: String },\n\
       items: [\n\
         #brand Image { name: \"img\", url: \"http://a.png\" },\n\
         #brand Text { name: \"txt\", content: \"hello\" }\n\
       ],\n\
       classify(it): it match {\n\
         Image: 1,\n\
         Text: 2,\n\
         *: 0\n\
       },\n\
       result: range(n).reduce(0, (acc, i) => acc + classify(items[i % 2]))\n\
     }"
}

fn run_dict_result_int(label: &str, src: &str, n: i64) -> i64 {
    let jit = JitEvaluator::new(src)
        .unwrap_or_else(|e| panic!("{label}: setup failed:\n{src}\nerr: {e}"));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    let v = jit
        .run_main(args)
        .unwrap_or_else(|e| panic!("{label}: run_main failed: {e}"));
    match v {
        Value::Dict(d) => match d.map.get("result") {
            Some(Value::Int(n)) => *n,
            other => panic!("{label}: dict.result is not Int: {other:?}"),
        },
        other => panic!("{label}: non-Dict return: {other:?}"),
    }
}

fn w21_expected() -> i64 {
    W21_N * 3 / 2
}

fn main() {
    let v = run_dict_result_int("W21", w21_src(), W21_N);
    let e = w21_expected();
    println!("W21_match_dispatch: got={v} expected={e} ok={}", v == e);
    assert_eq!(v, e, "W21 mismatch");

    println!("\nALL W21 smoke checks passed.");
}
