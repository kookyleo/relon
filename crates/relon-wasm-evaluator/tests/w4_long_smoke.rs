//! W4_long smoke: the 256-byte-haystack variant of W4.
//!
//! Source matches `W4_LONG_LLVM_SRC` in
//! `crates/relon-bench/benches/cmp_lua.rs` byte-identical: same chain
//! shape as W4 (`range(n).map(...).filter(s => s.contains("x")).len()`)
//! with the haystack swapped for a 256-byte literal whose terminal
//! byte is 'x'. Per-iter the host shim walks the full 256 bytes
//! before reporting hit, exercising the long-haystack path the
//! LLVM-side `W4_long_haystack` row measures.
//!
//! Honesty (design §7):
//!
//! 1. Same algorithm? — yes, the loop calls `__relon_str_contains`
//!    once per iter on the same 256-byte haystack + 1-byte needle
//!    the source declares. No closed-form `count = n` substitution.
//! 2. Same code path? — yes, same WASM lowering as W4 (variant
//!    `W4StringContains { long: true }`), only the haystack data
//!    segment differs.
//! 3. Same I/O shape? — `#main(Int n) -> Int`, returns
//!    `Value::Int(matched_count)`. Every iter matches (haystack
//!    ends with 'x'), so the result is `n`.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

// 256-byte haystack ending in 'x', byte-identical to `W4_LONG_HAYSTACK`
// in cmp_lua.rs / `W4_HAYSTACK_LONG` in relon-codegen-wasm.
const W4_LONG_SRC: &str = concat!(
    "#import list from \"std/list\"\n",
    "#main(Int n) -> Int\n",
    "range(n)\n",
    "  .map((i) => \"",
    "loremipsumdolorsitametconsecteturadipiscingelitseddoeiusmodtemporincididuntutlaboreetdoloremagnaaliquautenimadminimveniamquisnostrudezercitationullamcolaborisnisiutaliquipezeacommodoconsequatduisauteiruredolorinreprehenderitinvoluptatevelitessecillumaaaaax",
    "\")\n",
    "  .filter((s) => s.contains(\"x\"))\n",
    "  .len()",
);

#[test]
fn w4_long_handles_zero_n() {
    let ev = WasmEvaluator::new(W4_LONG_SRC).expect("WasmEvaluator::new(W4_long)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(0));
    let out = ev.run_main(args).expect("run_main(W4_long, n=0)");
    assert_eq!(out, Value::Int(0));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w4_long_matches_tree_walker_small() {
    let ev = WasmEvaluator::new(W4_LONG_SRC).expect("WasmEvaluator::new(W4_long)");
    for &n in &[1i64, 5, 32] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev.run_main(args).expect("run_main(W4_long)");
        assert_eq!(out, Value::Int(n), "W4_long result mismatch at n={n}");
    }
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w4_long_matches_tree_walker_at_bench_n() {
    // cmp_lua drives W4_long at TREE_WALK_N = 10_000.
    const TREE_WALK_N: i64 = 10_000;
    let ev = WasmEvaluator::new(W4_LONG_SRC).expect("WasmEvaluator::new(W4_long)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(TREE_WALK_N));
    let out = ev.run_main(args).expect("run_main(W4_long, n=10000)");
    assert_eq!(out, Value::Int(TREE_WALK_N));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w4_long_fast_path_available() {
    let ev = WasmEvaluator::new(W4_LONG_SRC).expect("WasmEvaluator::new(W4_long)");
    assert!(
        ev.has_fast_path(),
        "W4_long must expose fast-path entry (scalar Int return)"
    );
    let fast_out = ev
        .run_main_legacy_i64_fast(&[7])
        .expect("W4_long fast path run");
    assert_eq!(fast_out, 7);
}
