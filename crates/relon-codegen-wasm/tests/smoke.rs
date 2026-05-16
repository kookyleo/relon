//! Phase 1.alpha smoke tests.
//!
//! Two integration tests prove the codegen toolchain is wired end to
//! end before Phase 1.beta replaces the hardcoded body with real IR
//! lowering:
//!
//! 1. `hardcoded_double_runs_in_wasmtime` — load the emitted module
//!    into the reference wasmtime engine and round-trip `run_main(5)`,
//!    expecting `10`. Covers encoder output + runtime instantiation.
//! 2. `compile_output_validates_as_wasm` — independently validate the
//!    bytes with `wasmparser` and confirm a `run_main` export shows
//!    up. Covers spec-level conformance without needing an engine.

use relon_codegen_wasm::compile_hardcoded_double;
use wasmparser::{Parser, Payload, Validator};
use wasmtime::{Engine, Instance, Module, Store};

#[test]
fn hardcoded_double_runs_in_wasmtime() {
    let bytes = compile_hardcoded_double();

    let engine = Engine::default();
    let module = Module::new(&engine, &bytes).expect("wasmtime should load smoke module");

    // Smoke generator emits no imports, so an empty imports slice is
    // sufficient for instantiation.
    let mut store: Store<()> = Store::new(&engine, ());
    let instance =
        Instance::new(&mut store, &module, &[]).expect("smoke module should instantiate");

    let run_main = instance
        .get_typed_func::<i32, i32>(&mut store, "run_main")
        .expect("run_main export should be typed (i32) -> i32");

    let out = run_main
        .call(&mut store, 5)
        .expect("run_main(5) should execute without trapping");
    assert_eq!(out, 10, "hardcoded body should compute x * 2");
}

#[test]
fn compile_output_validates_as_wasm() {
    let bytes = compile_hardcoded_double();

    // Step 1: spec validator must accept the module.
    Validator::new()
        .validate_all(&bytes)
        .expect("emitted module must pass wasmparser validation");

    // Step 2: walk the section stream and confirm `run_main` is an
    // exported function. We do not assume a specific function index
    // here — only that the export exists with the right name and
    // kind.
    let mut saw_run_main_export = false;
    for payload in Parser::new(0).parse_all(&bytes) {
        let payload = payload.expect("payload should parse");
        if let Payload::ExportSection(reader) = payload {
            for export in reader {
                let export = export.expect("export entry should parse");
                if export.name == "run_main"
                    && matches!(export.kind, wasmparser::ExternalKind::Func)
                {
                    saw_run_main_export = true;
                }
            }
        }
    }
    assert!(
        saw_run_main_export,
        "module must export `run_main` as a function"
    );
}
