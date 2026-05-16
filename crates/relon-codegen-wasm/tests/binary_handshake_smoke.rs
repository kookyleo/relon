//! Phase 2.b binary-handshake integration tests.
//!
//! Drives `#main` source through parser + analyzer + relon-ir +
//! relon-codegen-wasm, then executes the resulting wasm module under
//! wasmtime by:
//!
//! 1. Allocating in_buf / out_buf positions in the module's linear
//!    memory (the codegen emits a single 1-page memory exported as
//!    `"memory"`).
//! 2. Writing the input record bytes via the
//!    `relon-eval-api::buffer::BufferBuilder`.
//! 3. Calling `run_main(in_ptr, in_len, out_ptr, out_cap)` and
//!    reading the result record back via `BufferReader`.
//!
//! Coverage:
//!   * Single-param Int → Int                (`int_unary_doubles`).
//!   * Two-param Int + Int → Int             (`int_add_two_params`).
//!   * Schema-hash mismatch refuse-to-load   (`schema_drift_refused`).
//!   * out_cap-too-small traps               (`out_cap_too_small_traps`).
//!   * in_len-too-small traps                (`in_len_too_small_traps`).
//!
//! The `if`-expression Bool test from the Phase 2.b spec is deferred
//! to Phase 2.c — codegen has no branch lowering yet, so a body of
//! `if flag { v } else { -v }` is rejected at the lowering stage.
//! Once branch codegen lands the test will be reinstated as part of
//! Phase 3.

use relon_codegen_wasm::{compile_lowered_entry, AbiError, LoadError, WasmModule};
use relon_eval_api::buffer::{BufferBuilder, BufferReader};
use relon_eval_api::layout::SchemaLayout;
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_ir::lower_workspace_single;
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

/// Compile + lower `src` end-to-end and return the wasm bytes plus
/// the canonical schemas the codegen passed through. Mirrors the
/// helper in `abi_smoke.rs` but lives here so the integration test
/// stays self-contained.
fn compile(src: &str) -> (Vec<u8>, Schema, Schema) {
    let ast = relon_parser::parse_document(src).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    assert!(
        !analyzed.has_errors(),
        "analyzer reported errors: {:?}",
        analyzed.diagnostics
    );
    let ir = lower_workspace_single(&analyzed, &ast).expect("lower");
    let bytes = compile_lowered_entry(&ir).expect("compile");
    (bytes, ir.main_schema, ir.return_schema)
}

/// One assembled wasmtime session — engine + store + instance +
/// memory + the typed `run_main` view — held together so each test
/// can drive an entire roundtrip without re-stating the setup.
struct WasmSession {
    store: Store<()>,
    memory: Memory,
    run_main: TypedFunc<(i32, i32, i32, i32), i32>,
}

impl WasmSession {
    /// Instantiate `bytes` and grab the `run_main` typed view + the
    /// exported linear memory.
    fn new(bytes: &[u8]) -> Self {
        let engine = Engine::default();
        let module = Module::new(&engine, bytes).expect("module load");
        let mut store: Store<()> = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("memory export");
        let run_main = instance
            .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "run_main")
            .expect("run_main typed view");
        Self {
            store,
            memory,
            run_main,
        }
    }

    /// Copy `bytes` into the wasm linear memory at `offset`.
    fn write(&mut self, offset: usize, bytes: &[u8]) {
        self.memory
            .write(&mut self.store, offset, bytes)
            .expect("memory write");
    }

    /// Read `len` bytes back out of the wasm linear memory at `offset`.
    fn read(&mut self, offset: usize, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        self.memory
            .read(&mut self.store, offset, &mut out)
            .expect("memory read");
        out
    }

    /// Convenience for the canonical `run_main(in_ptr, in_len,
    /// out_ptr, out_cap)` call shape.
    fn call(&mut self, in_ptr: i32, in_len: i32, out_ptr: i32, out_cap: i32) -> i32 {
        self.run_main
            .call(&mut self.store, (in_ptr, in_len, out_ptr, out_cap))
            .expect("run_main call should not trap")
    }

    /// Same as [`Self::call`] but expects the wasm to trap.
    fn call_expect_trap(&mut self, in_ptr: i32, in_len: i32, out_ptr: i32, out_cap: i32) {
        let res = self
            .run_main
            .call(&mut self.store, (in_ptr, in_len, out_ptr, out_cap));
        assert!(
            res.is_err(),
            "run_main was expected to trap, got Ok({:?})",
            res.unwrap()
        );
    }
}

/// Conventional layout for the integration tests: in_buf at byte 0,
/// out_buf at byte 256. Plenty of slack so the host doesn't have to
/// worry about allocator behaviour for Phase 2.b scalars.
const IN_PTR: i32 = 0;
const OUT_PTR: i32 = 256;

#[test]
fn int_unary_doubles() {
    let (wasm, main_schema, return_schema) = compile("#main(Int x) -> Int\nx * 2");

    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    // Host fills the in_buf via the typesafe builder so a stray
    // field reorder shows up as a TypeMismatch rather than as
    // mysterious wasm output.
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 5).expect("write x");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bytes_written = session.call(
        IN_PTR,
        in_bytes.len() as i32,
        OUT_PTR,
        return_layout.root_size as i32,
    );
    assert_eq!(bytes_written as usize, return_layout.root_size);

    let out_bytes = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, &out_bytes).expect("reader");
    assert_eq!(reader.read_int("value").expect("read value"), 10);
}

#[test]
fn int_add_two_params() {
    let (wasm, main_schema, return_schema) = compile("#main(Int a, Int b) -> Int\na + b");

    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("a", 3).expect("write a");
    builder.write_int("b", 7).expect("write b");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bytes_written = session.call(
        IN_PTR,
        in_bytes.len() as i32,
        OUT_PTR,
        return_layout.root_size as i32,
    );
    assert_eq!(bytes_written as usize, return_layout.root_size);

    let out_bytes = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, &out_bytes).expect("reader");
    assert_eq!(reader.read_int("value").expect("read value"), 10);
}

#[test]
fn float_param_and_return_roundtrip() {
    // Phase 2.b's spec covers Int / Float / Bool / Null layouts. The
    // arithmetic ops support both numeric flavours; this test pins
    // the Float path through the same handshake.
    let (wasm, main_schema, return_schema) = compile("#main(Float a, Float b) -> Float\na * b");

    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_float("a", 1.5).expect("write a");
    builder.write_float("b", 4.0).expect("write b");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bytes_written = session.call(
        IN_PTR,
        in_bytes.len() as i32,
        OUT_PTR,
        return_layout.root_size as i32,
    );
    assert_eq!(bytes_written as usize, return_layout.root_size);

    let out_bytes = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, &out_bytes).expect("reader");
    assert!((reader.read_float("value").expect("read value") - 6.0).abs() < f64::EPSILON);
}

#[test]
fn schema_drift_refused_at_load_time() {
    let (wasm, _orig_main, return_schema) = compile("#main(Int a, Int b) -> Int\na + b");

    // Host claims the schema is `(b, a)` instead of `(a, b)` — same
    // field set, different declaration order. Canonical hashing is
    // order-sensitive, so the loader must refuse the module.
    let drifted_main = Schema {
        name: "MainParams".to_string(),
        generics: vec![],
        fields: vec![
            Field {
                name: "b".to_string(),
                ty: TypeRepr::Int,
                default: None,
            },
            Field {
                name: "a".to_string(),
                ty: TypeRepr::Int,
                default: None,
            },
        ],
    };

    let err = WasmModule::from_bytes_with_schema(wasm, &drifted_main, &return_schema)
        .expect_err("drifted main schema must refuse");
    assert!(
        matches!(err, LoadError::Abi(AbiError::SchemaDrift { which: "main" })),
        "expected SchemaDrift on main, got {err:?}"
    );
}

#[test]
fn out_cap_too_small_traps() {
    let (wasm, main_schema, return_schema) = compile("#main(Int x) -> Int\nx * 2");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 5).expect("write x");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    // Pass an out_cap one byte short — the prologue guard must trap
    // (Phase 2.b uses `unreachable`; the Phase 7 translate_trap pass
    // will turn this into OutBufTooSmall, but for now the host sees
    // it as a generic wasmtime trap).
    let short_cap = (return_layout.root_size as i32) - 1;
    session.call_expect_trap(IN_PTR, in_bytes.len() as i32, OUT_PTR, short_cap);
}

#[test]
fn in_len_too_small_traps() {
    let (wasm, main_schema, return_schema) = compile("#main(Int x) -> Int\nx * 2");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 5).expect("write x");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    // in_len shorter than the canonical main_root_size — first
    // prologue guard must trap.
    let short_len = (in_bytes.len() as i32) - 1;
    session.call_expect_trap(IN_PTR, short_len, OUT_PTR, return_layout.root_size as i32);
}
