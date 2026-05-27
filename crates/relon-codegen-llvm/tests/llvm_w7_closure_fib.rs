//! Phase F.W7: end-to-end smoke for the W7 production-source recursive
//! `fib` closure routed through `LlvmAotEvaluator::from_source`.
//!
//! The cmp_lua W7 source declares `fib` as a `#internal` dict-field
//! closure that recurses on itself:
//!
//! ```text
//! #main(Int n) -> Dict
//! {
//!   #internal
//!   fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),
//!   result: fib(n)
//! }
//! ```
//!
//! Pre-Phase-F.W7 the LLVM emitter rejected this shape at the
//! `Op::MakeClosure` / `Op::CallClosure` arm of `lower_op` — the W7
//! row stayed `n/a` in the cmp_lua panel. This test asserts the
//! end-to-end pipeline (parse + analyzer-non-strict + IR lowering +
//! anon-Dict-return plan + MakeClosure-with-self-capture + indirect
//! `CallClosure` dispatch) is wired and the JIT result agrees with
//! the analytic `fib` oracle.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

fn fib_oracle(k: i64) -> i64 {
    if k < 2 {
        k
    } else {
        fib_oracle(k - 1) + fib_oracle(k - 2)
    }
}

fn extract_result(v: Value) -> i64 {
    match v {
        Value::Dict(d) => match d.map.get("result") {
            Some(Value::Int(i)) => *i,
            other => panic!("W7 return field `result` expected Int, got {other:?}"),
        },
        other => panic!("W7 return expected Dict, got {other:?}"),
    }
}

#[test]
fn w7_production_source_lowers_and_evaluates() {
    let src = "#main(Int n) -> Dict\n\
               {\n\
                 #internal\n\
                 fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                 result: fib(n)\n\
               }";
    let ev = LlvmAotEvaluator::from_source(src).expect("W7 source compiles via LLVM AOT");
    // fib(13) = 233 mirrors the cmp_lua bench's canonical W7 value.
    for n in [0i64, 1, 2, 5, 10, 13, 15, 20] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let got = extract_result(ev.run_main(args).expect("run_main"));
        let want = fib_oracle(n);
        assert_eq!(
            got, want,
            "W7 fib({n}) LLVM AOT result mismatches tree-walker oracle"
        );
    }
}

/// Phase D.2: the W7 `#main(Int n) -> Dict { result: Int }` shape now
/// qualifies for the typed-i64 fast-path entry. Hits
/// [`LlvmAotEvaluator::run_main_legacy_i64_fast`] directly so the test
/// fails if the fast emitter ever drops back to the buffer path on this
/// shape — the cmp_lua `relon_llvm_aot_fast` row depends on this.
#[test]
fn w7_production_source_has_fast_path() {
    let src = "#main(Int n) -> Dict\n\
               {\n\
                 #internal\n\
                 fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                 result: fib(n)\n\
               }";
    let ev = LlvmAotEvaluator::from_source(src).expect("W7 source compiles via LLVM AOT");
    assert!(
        ev.has_fast_path(),
        "Phase D.2: W7 anon-Dict-return should qualify for the typed-i64 fast path; \
         either `build_fast_path_profile` regressed or `emit_fast_entry` failed (check \
         the IR dump for the fast-emit diagnostic)"
    );
    for n in [0i64, 1, 2, 5, 10, 13, 15, 20] {
        let got = ev
            .run_main_legacy_i64_fast(&[n])
            .expect("W7 fast entry dispatch");
        let want = fib_oracle(n);
        assert_eq!(
            got, want,
            "W7 fast-path fib({n}) result mismatches tree-walker oracle"
        );
    }
}

/// Phase D.2: the fast-path `run_main` re-wraps the i64 result into a
/// `Value::Dict { result: Int }` matching the buffer-protocol decoder's
/// shape. The cmp_lua `relon_llvm_aot_fast` row uses
/// `run_main_legacy_i64_fast` directly (i64 -> i64), but production
/// hosts go through `run_main` and rely on the wrapped value lining up
/// with the schema's declared return shape.
#[test]
fn w7_fast_path_run_main_returns_dict() {
    let src = "#main(Int n) -> Dict\n\
               {\n\
                 #internal\n\
                 fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                 result: fib(n)\n\
               }";
    let ev = LlvmAotEvaluator::from_source(src).expect("W7 source compiles via LLVM AOT");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(13));
    let v = ev.run_main(args).expect("run_main");
    let got = extract_result(v);
    assert_eq!(got, fib_oracle(13));
}
