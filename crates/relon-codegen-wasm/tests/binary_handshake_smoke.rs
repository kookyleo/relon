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
//!   * Single-param Int → Int                  (`int_unary_doubles`).
//!   * Two-param Int + Int → Int               (`int_add_two_params`).
//!   * Float param + return                    (`float_param_and_return_roundtrip`).
//!   * Schema-hash mismatch refuse-to-load     (`schema_drift_refused`).
//!   * out_cap-too-small traps                 (`out_cap_too_small_traps`).
//!   * in_len-too-small traps                  (`in_len_too_small_traps`).
//!   * Phase 2.c: ternary if returning Int     (`if_expression_returns_int`).
//!   * Phase 2.c: Int comparison returning Bool (`comparison_returns_bool`).
//!   * Phase 2.c: String field layout pass-through (`string_field_in_buf_loads_pointer`).
//!   * Phase 2.c: List<Int> field layout pass-through (`list_int_field_in_buf_loads_pointer`).
//!   * Phase 2.c: ternary branch type mismatch refused at lowering.
//!   * Phase 2.c: non-Bool condition refused at lowering.

use relon_codegen_wasm::{compile_lowered_entry, AbiError, LoadError, WasmModule};
use relon_eval_api::buffer::{BufferBuilder, BufferReader};
use relon_eval_api::layout::SchemaLayout;
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_ir::{lower_workspace_single, LoweringError};
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

/// Compile + lower `src` without panicking on lowering errors —
/// returns the lowering error so the caller can match on it. Mirrors
/// [`compile`] but without the `compile_lowered_entry` step.
fn lower_only(src: &str) -> Result<(), LoweringError> {
    let ast = relon_parser::parse_document(src).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    // Don't bail on analyzer diagnostics — the lowering layer is
    // what these tests probe. The analyzer might still surface type
    // errors before lowering refuses; we only assert on the lowering
    // result here.
    lower_workspace_single(&analyzed, &ast).map(|_| ())
}

#[test]
fn if_expression_returns_int() {
    // Phase 2.c: ternary lowers to `Op::If { result_ty: Int }`. The
    // Relon surface uses the ternary form (`cond ? then : else`),
    // since no `if {} else {}` block syntax exists yet — same IR
    // shape either way.
    let (wasm, main_schema, return_schema) =
        compile("#main(Bool flag, Int v) -> Int\nflag ? v : 0 - v");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    // flag = true → +v
    {
        let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
        builder.write_bool("flag", true).expect("write flag");
        builder.write_int("v", 42).expect("write v");
        let in_bytes = builder.finish();
        let mut session = WasmSession::new(&wasm);
        session.write(IN_PTR as usize, &in_bytes);
        let bw = session.call(
            IN_PTR,
            in_bytes.len() as i32,
            OUT_PTR,
            return_layout.root_size as i32,
        );
        assert_eq!(bw as usize, return_layout.root_size);
        let out = session.read(OUT_PTR as usize, return_layout.root_size);
        let reader =
            BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
        assert_eq!(reader.read_int("value").expect("read value"), 42);
    }
    // flag = false → 0 - v
    {
        let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
        builder.write_bool("flag", false).expect("write flag");
        builder.write_int("v", 42).expect("write v");
        let in_bytes = builder.finish();
        let mut session = WasmSession::new(&wasm);
        session.write(IN_PTR as usize, &in_bytes);
        let bw = session.call(
            IN_PTR,
            in_bytes.len() as i32,
            OUT_PTR,
            return_layout.root_size as i32,
        );
        assert_eq!(bw as usize, return_layout.root_size);
        let out = session.read(OUT_PTR as usize, return_layout.root_size);
        let reader =
            BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
        assert_eq!(reader.read_int("value").expect("read value"), -42);
    }
}

#[test]
fn comparison_returns_bool() {
    // Phase 2.c: `x > 0` lowers to `Op::Gt(I64)`. The wasm result
    // type is `i32` (Bool), so the trailing StoreField uses
    // `i32.store8` and the host reads a single byte back.
    let (wasm, main_schema, return_schema) = compile("#main(Int x) -> Bool\nx > 0");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let check = |x: i64, want: bool| {
        let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
        builder.write_int("x", x).expect("write x");
        let in_bytes = builder.finish();
        let mut session = WasmSession::new(&wasm);
        session.write(IN_PTR as usize, &in_bytes);
        let bw = session.call(
            IN_PTR,
            in_bytes.len() as i32,
            OUT_PTR,
            return_layout.root_size as i32,
        );
        assert_eq!(bw as usize, return_layout.root_size);
        let out = session.read(OUT_PTR as usize, return_layout.root_size);
        let reader =
            BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
        assert_eq!(reader.read_bool("value").expect("read value"), want);
    };
    check(5, true);
    check(-3, false);
}

#[test]
fn string_field_in_buf_loads_pointer() {
    // Phase 2.c: layout supports a `String` parameter. The wasm body
    // never references `name`, so it doesn't need to load the
    // pointer — the test checks that the **layout** for a
    // (String, Int) signature still places `x` at the right offset.
    // A bug in the fixed-area sizing would surface as the codegen
    // reading garbage out of `in_buf`.
    let (wasm, main_schema, return_schema) = compile("#main(String name, Int x) -> Int\nx * 2");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    assert!(main_layout.requires_tail_area());

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("name", "ada").expect("write name");
    builder.write_int("x", 21).expect("write x");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(
        IN_PTR,
        in_bytes.len() as i32,
        OUT_PTR,
        return_layout.root_size as i32,
    );
    assert_eq!(bw as usize, return_layout.root_size);
    let out = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader = BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
    assert_eq!(reader.read_int("value").expect("read value"), 42);
}

#[test]
fn list_int_field_in_buf_loads_pointer() {
    // Phase 2.c: same shape as the String test but with a
    // `List<Int>` parameter. `nums` is unused in the body — the
    // test pins the layout's tail-area indirection for List<Int>.
    let (wasm, main_schema, return_schema) = compile("#main(List<Int> nums, Int x) -> Int\nx + 1");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    assert!(main_layout.requires_tail_area());

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder
        .write_list_int("nums", &[10, 20, 30])
        .expect("write nums");
    builder.write_int("x", 41).expect("write x");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(
        IN_PTR,
        in_bytes.len() as i32,
        OUT_PTR,
        return_layout.root_size as i32,
    );
    assert_eq!(bw as usize, return_layout.root_size);
    let out = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader = BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
    assert_eq!(reader.read_int("value").expect("read value"), 42);
}

#[test]
fn if_branch_type_mismatch_rejected_at_lowering() {
    // `b ? x : true` — `x` lowers to Int, `true` is a Bool, so
    // lowering must report the branch-type mismatch rather than a
    // successful build.
    let err = lower_only("#main(Bool b, Int x) -> Int\nb ? x : true")
        .expect_err("if branch mismatch must reject");
    assert!(
        matches!(err, LoweringError::IfBranchTypeMismatch { .. }),
        "expected IfBranchTypeMismatch, got {err:?}"
    );
}

#[test]
fn bool_literal_true_returns_true() {
    // Phase 3.a: `true` as the only body expression. Returns a 1-byte
    // Bool with value 1.
    let (wasm, main_schema, return_schema) = compile("#main(Int x) -> Bool\ntrue");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 5).expect("write x");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(
        IN_PTR,
        in_bytes.len() as i32,
        OUT_PTR,
        return_layout.root_size as i32,
    );
    assert_eq!(bw as usize, return_layout.root_size);
    let out = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader = BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
    assert!(reader.read_bool("value").expect("read value"));
}

#[test]
fn bool_literal_false_returns_false() {
    let (wasm, main_schema, return_schema) = compile("#main(Int x) -> Bool\nfalse");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 5).expect("write x");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(
        IN_PTR,
        in_bytes.len() as i32,
        OUT_PTR,
        return_layout.root_size as i32,
    );
    assert_eq!(bw as usize, return_layout.root_size);
    let out = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader = BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
    assert!(!reader.read_bool("value").expect("read value"));
}

#[test]
fn let_binding_via_where_clause() {
    // Phase 3.a: `<expr> where { name: value }` introduces a user-let
    // binding. The bound value rides in a dedicated wasm local so a
    // body that reuses `y` twice computes `x * 2` once.
    //
    // `#relaxed` opts the source out of strict-mode resolution — the
    // analyzer otherwise reports `UnknownReferenceType` for the
    // where-bound name because its `typecheck::check_unresolved_ref`
    // pass doesn't yet model the body's extended scope. Lowering
    // does, so the wasm output is well-formed regardless.
    let (wasm, main_schema, return_schema) =
        compile("#relaxed\n#main(Int x) -> Int\n(y + y) where { y: x * 2 }");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 5).expect("write x");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(
        IN_PTR,
        in_bytes.len() as i32,
        OUT_PTR,
        return_layout.root_size as i32,
    );
    assert_eq!(bw as usize, return_layout.root_size);
    let out = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader = BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
    // y = 5 * 2 = 10 ; y + y = 20.
    assert_eq!(reader.read_int("value").expect("read value"), 20);
}

#[test]
fn let_binding_multiple_names() {
    // Two let bindings in the same `where` block. Each one gets a
    // fresh wasm local; the body references both alongside the
    // `#main` param. `#relaxed` opts out of strict-mode analyzer
    // resolution (see `let_binding_via_where_clause` for the same
    // workaround rationale).
    let (wasm, main_schema, return_schema) =
        compile("#relaxed\n#main(Int x) -> Int\n(a + b + x) where { a: 1, b: 2 }");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 100).expect("write x");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(
        IN_PTR,
        in_bytes.len() as i32,
        OUT_PTR,
        return_layout.root_size as i32,
    );
    assert_eq!(bw as usize, return_layout.root_size);
    let out = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader = BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
    assert_eq!(reader.read_int("value").expect("read value"), 103);
}

#[test]
fn string_literal_output_roundtrips() {
    // Phase 3.a: returning a String literal. The wasm module owns a
    // `[len:u32][utf8]` record inside its const data section; the
    // body memcpy's that record into the caller's `out_buf` tail
    // area at runtime and writes the buffer-relative offset to the
    // fixed-area pointer slot.
    let (wasm, main_schema, return_schema) = compile("#main(Int x) -> String\n\"hi\"");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 7).expect("write x");
    let in_bytes = builder.finish();

    // Allocate plenty of out_cap so the tail record fits.
    let out_cap: i32 = 64;
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, out_cap);
    assert!(bw as usize >= return_layout.root_size);

    // Read enough bytes to cover the fixed area + the worst-case
    // record. `bytes_written` tells us exactly how many.
    let out = session.read(OUT_PTR as usize, bw as usize);
    let reader = BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
    assert_eq!(reader.read_string("value").expect("read value"), "hi");
}

#[test]
fn list_int_literal_output_roundtrips() {
    // Phase 3.a: returning a List<Int> literal. The data section
    // record carries the `[len:u32][pad:u32][i64 elements]` shape so
    // the wasm-side memory.copy lands at an 8-aligned tail offset
    // with the element bytes intact.
    let (wasm, main_schema, return_schema) = compile("#main(Int x) -> List<Int>\n[10, 20, 30]");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 0).expect("write x");
    let in_bytes = builder.finish();

    let out_cap: i32 = 128;
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, out_cap);
    assert!(bw as usize >= return_layout.root_size);

    let out = session.read(OUT_PTR as usize, bw as usize);
    let reader = BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
    assert_eq!(
        reader.read_list_int("value").expect("read value"),
        vec![10, 20, 30]
    );
}

#[test]
fn string_return_out_cap_too_small_traps() {
    // out_cap covers the 4-byte fixed-area pointer slot but is too
    // small to fit the tail record — the runtime bounds check inside
    // `emit_store_pointer_indirect` must trap.
    let (wasm, main_schema, _return_schema) = compile("#main(Int x) -> String\n\"x\"");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 1).expect("write x");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    // 4-byte fixed area + 1 padding byte = 5; the record needs `4 + "x".len()` = 5
    // more bytes, so out_cap=8 trips the tail-area bounds guard.
    session.call_expect_trap(IN_PTR, in_bytes.len() as i32, OUT_PTR, 8);
}

#[test]
fn if_condition_not_bool_rejected_at_lowering() {
    // `x ? 1 : 0` — `x` is Int, not Bool, so lowering must reject
    // the ternary's condition.
    let err =
        lower_only("#main(Int x) -> Int\nx ? 1 : 0").expect_err("non-bool condition must reject");
    assert!(
        matches!(err, LoweringError::IfConditionNotBool { .. }),
        "expected IfConditionNotBool, got {err:?}"
    );
}
