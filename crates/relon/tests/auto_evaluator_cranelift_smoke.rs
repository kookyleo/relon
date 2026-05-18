//! AutoEvaluator + cranelift-AOT integration smoke.
//!
//! Verifies that `Backend::Auto` produces a working evaluator even
//! when the `cranelift-aot` feature is on alongside `wasm-aot`. The
//! cranelift path always falls back to wasm-AOT for v5-beta-1 sources
//! (the production parse + lower pipeline emits buffer-protocol IR
//! that cranelift hasn't lowered yet), so we exercise the wasm-AOT
//! fallback continues to work transparently.

#![cfg(all(feature = "wasm-aot", feature = "cranelift-aot"))]

use std::collections::HashMap;

use relon::{new_evaluator, Backend};
use relon_eval_api::Value;

#[test]
fn auto_backend_falls_through_to_wasm_aot_for_real_source() {
    // Production Relon source: the cranelift backend rejects this
    // shape (the lowered IR uses buffer-protocol ops it doesn't yet
    // cover) so the auto-tier wrapper must fall through to wasm-AOT
    // and complete `run_main` successfully.
    let src = "#main(Int n) -> Int\nn + 1";
    let evaluator = new_evaluator(src, Backend::Auto).expect("Auto backend");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(41));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}

#[test]
fn cranelift_backend_explicit_select_surfaces_error_for_unsupported_source() {
    // Backend::CraneliftAot bypasses the auto-tier fallback. The
    // production source path goes through the buffer-protocol IR
    // ops the cranelift backend doesn't cover yet, so this must
    // surface a typed BackendError.
    let src = "#main(Int n) -> Int\nn + 1";
    let result = new_evaluator(src, Backend::CraneliftAot);
    assert!(result.is_err(), "cranelift backend should reject buffer IR");
    let err = result.err().unwrap();
    let msg = format!("{err}");
    assert!(
        msg.contains("cranelift") || msg.contains("unsupported"),
        "{msg}"
    );
}

#[test]
fn wasm_aot_backend_still_works_when_cranelift_feature_is_on() {
    let src = "#main(Int n) -> Int\nn * 2";
    let evaluator = new_evaluator(src, Backend::WasmAot).expect("WasmAot backend");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(21));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}
