//! Phase 4.a stdlib roundtrip tests.
//!
//! End-to-end coverage for the bundled `length(String) -> Int` stdlib
//! function: parser -> analyzer -> IR -> codegen -> wasmtime. The
//! tests pin both invocation shapes the lowering pass accepts
//! (method-call and free-call) plus boundary cases (empty string,
//! const-string receiver).
//!
//! The tests intentionally avoid the `#main(...): <body>` colon
//! syntax — the parser rejects that form, so every fixture uses the
//! newline-separated body shape that mirrors the rest of the Phase 3
//! integration tests.

use relon_codegen_wasm::compile_lowered_entry;
use relon_eval_api::buffer::{BufferBuilder, BufferReader};
use relon_eval_api::layout::SchemaLayout;
use relon_eval_api::schema_canonical::Schema;
use relon_ir::lower_workspace_single;
use wasmtime::{
    Engine, Global, Instance, Memory, Module, Mutability, Store, TypedFunc, Val, ValType,
};

fn compile(src: &str) -> (Vec<u8>, Schema, Schema) {
    let ast = relon_parser::parse_document(src).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    // Analyzer warnings about unresolved `length` (free-call form)
    // are expected — the analyzer's name-resolution pass doesn't yet
    // know about bundled stdlib names. Only fail on hard errors.
    assert!(
        !analyzed.has_errors(),
        "analyzer reported errors: {:?}",
        analyzed.diagnostics
    );
    let ir = lower_workspace_single(&analyzed, &ast).expect("lower");
    let bytes = compile_lowered_entry(&ir).expect("compile");
    (bytes, ir.main_schema, ir.return_schema)
}

struct WasmSession {
    store: Store<()>,
    memory: Memory,
    run_main: TypedFunc<(i32, i32, i32, i32), i32>,
}

impl WasmSession {
    fn new(bytes: &[u8]) -> Self {
        let engine = Engine::default();
        let module = Module::new(&engine, bytes).expect("module load");
        let mut store: Store<()> = Store::new(&engine, ());
        let caps_avail = Global::new(
            &mut store,
            wasmtime::GlobalType::new(ValType::I64, Mutability::Const),
            Val::I64(i64::MAX),
        )
        .expect("create relon_caps_avail global");
        let instance =
            Instance::new(&mut store, &module, &[caps_avail.into()]).expect("instantiate");
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

    fn write(&mut self, offset: usize, bytes: &[u8]) {
        self.memory
            .write(&mut self.store, offset, bytes)
            .expect("memory write");
    }

    fn read(&mut self, offset: usize, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        self.memory
            .read(&mut self.store, offset, &mut out)
            .expect("memory read");
        out
    }

    fn call(&mut self, in_ptr: i32, in_len: i32, out_ptr: i32, out_cap: i32) -> i32 {
        self.run_main
            .call(&mut self.store, (in_ptr, in_len, out_ptr, out_cap))
            .expect("run_main call should not trap")
    }
}

const IN_PTR: i32 = 0;
const OUT_PTR: i32 = 1024;

#[test]
fn string_length_via_method() {
    // `s.length()` — method-call form. The receiver `s` lowers to
    // `LoadStringPtr` (the in_buf-relative pointer lifted to an
    // absolute wasm-memory address); the call hands the absolute
    // address to the stdlib `length` body which loads `i32.load offset=0`
    // and widens to i64.
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> Int\ns.length()");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", "hello").expect("write s");
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
    assert_eq!(reader.read_int("value").expect("read value"), 5);
}

#[test]
fn string_length_via_free_call() {
    // `length(s)` — free-call form. Same lowering as the method-call
    // form except the receiver moves into the first `args` slot. The
    // analyzer reports `length` as an unresolved reference (Phase 4.a
    // hasn't taught the analyzer about bundled stdlib names), but
    // that's a warning rather than a hard error so lowering still
    // succeeds.
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> Int\nlength(s)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", "hello").expect("write s");
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
    assert_eq!(reader.read_int("value").expect("read value"), 5);
}

#[test]
fn length_on_empty_string() {
    // Empty string roundtrip — the stdlib reads `[len=0][...]` so the
    // returned i64 is exactly 0.
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> Int\ns.length()");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", "").expect("write s");
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
    assert_eq!(reader.read_int("value").expect("read value"), 0);
}

#[test]
fn length_on_const_string() {
    // Receiver is a `ConstString` literal living in the wasm data
    // section. The IR pushes the absolute data-section address; the
    // stdlib `length` body reads the u32 LE prefix at that address
    // and reports it as an i64.
    let (wasm, main_schema, return_schema) = compile("#main(Int x) -> Int\nlength(\"world\")");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 0).expect("write x");
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
    assert_eq!(reader.read_int("value").expect("read value"), 5);
}

// ---------------------------------------------------------------
// Phase 4.b stdlib roundtrip tests.
//
// Coverage:
//   * `list_int_length` via method-call and free-call forms — both
//     resolve through the bundled body that re-uses
//     `Op::ReadStringLen` (the record layout shares the u32 LE length
//     prefix between String and List<Int>).
//   * `abs(x: Int) -> Int` — `Op::Select` selects between `-x` and
//     `x` based on `x < 0`. Three sub-cases: positive, negative, zero.
//   * `min` / `max` — two-arg `Op::Select` against the `<` / `>`
//     comparison ops.
//   * `is_empty(s: String) -> Bool` — `Op::ReadStringLen` followed by
//     i64 equality with zero, returning the i32 Bool slot. Coverage
//     on both empty and non-empty inputs.
// ---------------------------------------------------------------

#[test]
fn list_int_length_method() {
    // `xs.length()` for a `List<Int>` receiver dispatches through
    // the Phase 4.b method-dispatch table to `list_int_length`.
    let (wasm, main_schema, return_schema) = compile("#main(List<Int> xs) -> Int\nxs.length()");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder
        .write_list_int("xs", &[1, 2, 3, 4, 5])
        .expect("write xs");
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
    assert_eq!(reader.read_int("value").expect("read value"), 5);
}

#[test]
fn list_int_length_free_call() {
    // Free-call form on a `List<Int>` resolves through the explicit
    // `list_int_length` name. `length(xs)` would surface as an arg-
    // type mismatch since `length` is declared `String -> Int`.
    let (wasm, main_schema, return_schema) =
        compile("#main(List<Int> xs) -> Int\nlist_int_length(xs)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder
        .write_list_int("xs", &[10, 20, 30])
        .expect("write xs");
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
    assert_eq!(reader.read_int("value").expect("read value"), 3);
}

fn run_abs(input: i64) -> i64 {
    // Builds a `#main(Int x) -> Int : abs(x)` module and runs it
    // with the provided input. Returns the resulting i64.
    let (wasm, main_schema, return_schema) = compile("#main(Int x) -> Int\nabs(x)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", input).expect("write x");
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
    reader.read_int("value").expect("read value")
}

#[test]
fn abs_positive() {
    assert_eq!(run_abs(5), 5);
}

#[test]
fn abs_negative() {
    assert_eq!(run_abs(-5), 5);
}

#[test]
fn abs_zero() {
    // Boundary: `0 < 0` is false, so `select` picks the `false` arm
    // (which is `x == 0`). The body must not negate zero away from
    // the zero value (no `-0` artifact in i64).
    assert_eq!(run_abs(0), 0);
}

#[test]
fn min_picks_smaller() {
    let (wasm, main_schema, return_schema) = compile("#main(Int a, Int b) -> Int\nmin(a, b)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("a", 3).expect("write a");
    builder.write_int("b", 7).expect("write b");
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
    assert_eq!(reader.read_int("value").expect("read value"), 3);
}

#[test]
fn max_picks_larger() {
    let (wasm, main_schema, return_schema) = compile("#main(Int a, Int b) -> Int\nmax(a, b)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("a", 3).expect("write a");
    builder.write_int("b", 7).expect("write b");
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
    assert_eq!(reader.read_int("value").expect("read value"), 7);
}

fn run_is_empty(input: &str) -> bool {
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> Bool\ns.is_empty()");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", input).expect("write s");
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
    reader.read_bool("value").expect("read value")
}

#[test]
fn is_empty_true() {
    assert!(run_is_empty(""));
}

#[test]
fn is_empty_false() {
    assert!(!run_is_empty("x"));
}

#[test]
fn abi_section_still_emitted_when_stdlib_present() {
    // Phase 4.a prepends stdlib functions, which means the module's
    // function index space shifts. The `relon.abi` section is keyed
    // off the schema hashes (not function indices), so it still has
    // to decode and the schema-hash check passes.
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> Int\ns.length()");
    let module =
        relon_codegen_wasm::WasmModule::from_bytes_with_schema(wasm, &main_schema, &return_schema)
            .expect("from_bytes_with_schema");
    let abi = module.abi();
    // Magic / versions stay pinned at the v1 defaults.
    assert_eq!(
        abi.abi_version,
        relon_codegen_wasm::abi::CURRENT_ABI_VERSION
    );
    assert_eq!(
        abi.codegen_version,
        relon_codegen_wasm::abi::CURRENT_CODEGEN_VERSION
    );
}
