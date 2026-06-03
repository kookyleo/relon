//! Quick smoke check for W16/W17/W18 workload sources (panel
//! expansion 2026-05-28, Tier 2 industry-standard).
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
//! cargo run -p relon-bench --bin w16_w17_w18_smoke
//! ```

use relon::AutoEvaluator;
use relon_eval_api::{Evaluator as _, Value};
use std::collections::HashMap;

const W16_N: i64 = 1_000;
const W17_N: i64 = 100;
const W18_N: i64 = 10_000;

fn w16_src() -> &'static str {
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

fn w17_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Int\n\
     range(n).reduce(0, (acc, i) => acc + bs(0, n, (i * 31) % n))\n\
     where {\n\
       bs(lo, hi, t): hi - lo <= 1 ? lo : (\n\
         (lo + hi) / 2 <= t\n\
           ? bs((lo + hi) / 2, hi, t)\n\
           : bs(lo, (lo + hi) / 2, t)\n\
       )\n\
     }"
}

fn w18_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Int\n\
     _len(_list_filter(range(2, n), (k) => is_prime(k, 2)))\n\
     where {\n\
       is_prime(k, d): d * d > k ? true : (k % d == 0 ? false : is_prime(k, d + 1))\n\
     }"
}

fn run(label: &str, src: &str, n: i64) -> i64 {
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

fn w16_expected() -> i64 {
    let mut acc: i64 = 0;
    for i in 0..W16_N {
        acc += (i.wrapping_mul(1103515245).wrapping_add(12345)) % 2048;
    }
    acc
}

fn w17_expected() -> i64 {
    let mut acc: i64 = 0;
    for i in 0..W17_N {
        acc += (i.wrapping_mul(31)) % W17_N;
    }
    acc
}

fn w18_expected() -> i64 {
    fn is_prime(k: i64) -> bool {
        let mut d: i64 = 2;
        while d.saturating_mul(d) <= k {
            if k % d == 0 {
                return false;
            }
            d += 1;
        }
        true
    }
    let mut count: i64 = 0;
    let mut k: i64 = 2;
    while k < W18_N {
        if is_prime(k) {
            count += 1;
        }
        k += 1;
    }
    count
}

fn main() {
    let v16 = run("W16", w16_src(), W16_N);
    let e16 = w16_expected();
    println!("W16_quicksort: got={v16} expected={e16} ok={}", v16 == e16);
    assert_eq!(v16, e16, "W16 mismatch");

    let v17 = run("W17", w17_src(), W17_N);
    let e17 = w17_expected();
    println!(
        "W17_binary_search: got={v17} expected={e17} ok={}",
        v17 == e17
    );
    assert_eq!(v17, e17, "W17 mismatch");

    let v18 = run("W18", w18_src(), W18_N);
    let e18 = w18_expected();
    println!(
        "W18_prime_count_trial_div: got={v18} expected={e18} ok={}",
        v18 == e18
    );
    assert_eq!(v18, e18, "W18 mismatch");

    println!("\nALL W16/W17/W18 smoke checks passed.");
}
