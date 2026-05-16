//! Phase 1.beta end-to-end tests.
//!
//! Drive a Relon source through parser + analyzer + relon-ir +
//! relon-codegen-wasm and execute the resulting wasm module in
//! wasmtime. Validates the locked design decisions:
//!
//! 1. `Int` lowers to `i64`, scalar parameters arrive as wasm
//!    function args (binary handshake will replace this in Phase 2
//!    once layouts grow past scalars).
//! 2. `#main` exports as `run_main`.
//! 3. Lowering rejects out-of-scope expressions / signatures with
//!    structured `LoweringError` variants — no panics.

use relon_codegen_wasm::compile_module;
use relon_ir::{lower_workspace_single, LoweringError};
use wasmtime::{Engine, Instance, Module, Store};

/// Helper: source -> wasm bytes. Single-file, single-module shape
/// (analyzer's `analyze()` legacy entry, paired with the IR's
/// `lower_workspace_single` convenience). Once Phase 4 lands a real
/// `WorkspaceTree`-driven path the smoke test switches to
/// `lower_workspace`.
fn compile_source(src: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let ast = relon_parser::parse_document(src)?;
    let analyzed = relon_analyzer::analyze(&ast);
    if analyzed.has_errors() {
        return Err(format!("analyzer reported errors: {:?}", analyzed.diagnostics).into());
    }
    let ir = lower_workspace_single(&analyzed, &ast)?;
    Ok(compile_module(&ir)?)
}

#[test]
fn double_int_lowers_and_runs() {
    let source = "#main(Int x) -> Int\nx * 2";
    let wasm_bytes = compile_source(source).expect("compile end-to-end");

    let engine = Engine::default();
    let module = Module::new(&engine, &wasm_bytes).expect("wasmtime should load module");
    let mut store: Store<()> = Store::new(&engine, ());
    let instance =
        Instance::new(&mut store, &module, &[]).expect("module should instantiate cleanly");
    let run_main = instance
        .get_typed_func::<i64, i64>(&mut store, "run_main")
        .expect("run_main should be exported as (i64) -> i64");
    let result = run_main
        .call(&mut store, 5)
        .expect("run_main(5) should execute without trapping");
    assert_eq!(result, 10, "x * 2 with x=5 should compute 10, got {result}");
}

#[test]
fn add_and_mul_lower_with_correct_precedence() {
    // (2 + 3) * 4 - operator precedence puts `+` inside `*`, so the
    // lowered op stream is `[const 2, const 3, add, const 4, mul]`.
    // Engine-level execution is the canonical check that the lowered
    // stack ordering matches the source intent.
    let source = "#main(Int x) -> Int\nx + 3 * 4";
    let wasm_bytes = compile_source(source).expect("compile end-to-end");

    let engine = Engine::default();
    let module = Module::new(&engine, &wasm_bytes).expect("load");
    let mut store: Store<()> = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
    let run_main = instance
        .get_typed_func::<i64, i64>(&mut store, "run_main")
        .expect("typed func");
    let result = run_main.call(&mut store, 10).expect("call");
    // 10 + (3 * 4) = 22
    assert_eq!(result, 22, "expected 22, got {result}");
}

#[test]
fn missing_main_reports_error() {
    let source = "{ val: 1 }";
    let ast = relon_parser::parse_document(source).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    let err = lower_workspace_single(&analyzed, &ast).expect_err("lowering should reject");
    assert!(
        matches!(err, LoweringError::MissingMain { .. }),
        "expected MissingMain, got {err:?}"
    );
}

#[test]
fn unsupported_type_in_main_reports_error() {
    let source = "#main(String s) -> String\ns";
    let ast = relon_parser::parse_document(source).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    // Analyzer may surface diagnostics on `String` for an entry's
    // return shape; we still drive the lowering path because the
    // user-visible error we care about here is the IR's rejection.
    let err = lower_workspace_single(&analyzed, &ast).expect_err("lowering should reject");
    assert!(
        matches!(err, LoweringError::UnsupportedTypeInMain { .. }),
        "expected UnsupportedTypeInMain, got {err:?}"
    );
}
