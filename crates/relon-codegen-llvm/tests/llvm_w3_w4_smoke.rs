//! Phase E.1 end-to-end smoke: `.relon` source → LLVM AOT → run_main
//! → typed return for the cmp_lua W3 / W4 / W4_long workloads. Each
//! test cross-checks against the canonical tree-walker output so any
//! miscompile shows up as a value mismatch rather than a silent
//! regression.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// W3 — String fold: `range(n).map(_ => "a").reduce("", (acc, s) => acc + s)`.
const W3_SRC: &str = "#unstrict\n\
                      #import list from \"std/list\"\n\
                      #main(Int n) -> String\n\
                      range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";

/// W4 — String contains: count strings in a generated list that contain "x".
const W4_SRC: &str = "#import list from \"std/list\"\n\
                      #main(Int n) -> Int\n\
                      range(n).map((i) => \"axb\").filter((s) => s.contains(\"x\")).len()";

fn run_int(src: &str, n: i64) -> Value {
    let ev = LlvmAotEvaluator::from_source(src).expect("LLVM AOT from_source");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    ev.run_main(args).expect("LLVM run_main")
}

#[test]
fn w3_string_concat_zero() {
    assert_eq!(run_int(W3_SRC, 0), Value::String("".into()));
}

#[test]
fn w3_string_concat_one() {
    assert_eq!(run_int(W3_SRC, 1), Value::String("a".into()));
}

#[test]
fn w3_string_concat_ten() {
    assert_eq!(run_int(W3_SRC, 10), Value::String("aaaaaaaaaa".into()));
}

#[test]
fn w3_string_concat_thousand() {
    let expected = "a".repeat(1000);
    assert_eq!(run_int(W3_SRC, 1000), Value::String(expected.into()));
}

#[test]
fn w4_string_contains_zero() {
    assert_eq!(run_int(W4_SRC, 0), Value::Int(0));
}

#[test]
fn w4_string_contains_one() {
    assert_eq!(run_int(W4_SRC, 1), Value::Int(1));
}

#[test]
fn w4_string_contains_ten() {
    assert_eq!(run_int(W4_SRC, 10), Value::Int(10));
}

#[test]
fn w4_string_contains_thousand() {
    assert_eq!(run_int(W4_SRC, 1000), Value::Int(1000));
}

/// Phase H guard: the W4 module must lower `s.contains("x")` away from
/// the bundled naive O(s_len * p_len) stdlib body. Phase F.1 routed all
/// `contains` calls through the host shim `relon_llvm_str_contains_arena`;
/// Phase H further specialises the const-single-byte-needle case (the
/// W4 / W4_long hot loop) to an inline byte-scan loop that LLVM 18's
/// loop vectoriser lowers to SSE2 `pcmpeqb` + `pmovmskb`. Either shape
/// is acceptable — the only regression we guard against is the bundled
/// stdlib body reappearing (which would re-open the pre-F.1 gap vs
/// LuaJIT). Asserting on the IR dump keeps the regression observable
/// without depending on a wall-clock measurement.
#[test]
fn w4_ir_dumps_str_contains_fast_path() {
    let ev =
        relon_codegen_llvm::LlvmAotEvaluator::from_source(W4_SRC).expect("LLVM AOT from_source");
    let dump = ev.emit_ir_dump();
    // Phase H fast-path marker: the const-single-byte-needle path
    // declares + calls libc `@memchr` directly. Phase F.1 extern
    // marker: the shim declaration / call sites mention
    // `relon_llvm_str_contains_arena`. The W4 source must contain
    // at least one of them — the bundled stdlib body produces neither,
    // so its accidental re-inlining would trip this assert.
    let has_libc_memchr = dump.contains("@memchr");
    let has_extern_shim = dump.contains("relon_llvm_str_contains_arena");
    assert!(
        has_libc_memchr || has_extern_shim,
        "W4 IR must use either the Phase H libc `@memchr` fast path \
         or the Phase F.1 host shim (`relon_llvm_str_contains_arena`); \
         neither appeared. Dump:\n{dump}"
    );
    // W4's needle is the single-byte literal `"x"` — the Phase H const-
    // needle path should fire. If the libc-memchr path doesn't appear
    // we fell back to the shim, which means the peek-state plumbing
    // regressed.
    assert!(
        has_libc_memchr,
        "W4 source has a compile-time single-byte `\"x\"` needle — \
         Phase H should lower it through libc `@memchr`, not the \
         extern shim. Dump:\n{dump}"
    );
}
