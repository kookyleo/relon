//! Phase 4.c-2 stdlib expansion roundtrip tests.
//!
//! End-to-end coverage for the seven stdlib functions added in this
//! phase: `concat`, `upper`, `lower`, `substring`, `starts_with`,
//! `list_int_sum`, `list_int_max`. Same parser → analyzer → IR →
//! codegen → wasmtime pipeline the Phase 4.a/4.b smoke tests use.
//!
//! Tests cover:
//!   * both method-call (`s.upper()`) and free-call (`upper(s)`)
//!     dispatch paths,
//!   * boundary inputs (empty receiver, single-byte string, all-ASCII
//!     and mixed-ASCII payloads),
//!   * trap-emitting bodies (`substring` with out-of-range bounds,
//!     `list_int_max` on an empty list), validated through
//!     `WasmModule::translate_trap`.

use relon_codegen_wasm::{compile_lowered_entry, WasmModule};
use relon_eval_api::buffer::{BufferBuilder, BufferReader};
use relon_eval_api::layout::SchemaLayout;
use relon_eval_api::schema_canonical::Schema;
use relon_eval_api::RuntimeError;
use relon_ir::lower_workspace_single;
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

/// Compile a snippet through the full pipeline. Stops at the wasm
/// bytes — callers spin up their own wasmtime session through
/// [`WasmSession`] when they need to invoke `run_main`.
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

const IN_PTR: i32 = 0;
/// Scratch heap + outputs all share the linear memory above IN_PTR;
/// the offset is large enough that nothing the host writes overlaps
/// with a typical stdlib-body output. Tests using larger inputs
/// override the layout per-call.
const OUT_PTR: i32 = 1024;
/// Default out_cap — large enough for any stdlib-produced record up
/// to 256 bytes plus the bookkeeping the codegen reserves.
const OUT_CAP: i32 = 512;

/// Bundled wasmtime session — mirrors the pattern in
/// `stdlib_smoke.rs` so the two files stay self-contained.
struct WasmSession {
    store: Store<()>,
    memory: Memory,
    run_main: TypedFunc<(i32, i32, i32, i32, i64), i32>,
}

impl WasmSession {
    fn new(bytes: &[u8]) -> Self {
        let engine = Engine::default();
        let module = Module::new(&engine, bytes).expect("module load");
        let mut store: Store<()> = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).expect("instantiate");
        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("memory export");
        let run_main = instance
            .get_typed_func::<(i32, i32, i32, i32, i64), i32>(&mut store, "run_main")
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

    /// Phase 11: stdlib bodies in this file don't touch
    /// capability-guarded host fns, so the caps argument always
    /// hands in `i64::MAX`.
    fn call(&mut self, in_ptr: i32, in_len: i32, out_ptr: i32, out_cap: i32) -> i32 {
        self.run_main
            .call(
                &mut self.store,
                (in_ptr, in_len, out_ptr, out_cap, i64::MAX),
            )
            .expect("run_main call must not trap")
    }

    fn call_expect_trap(
        &mut self,
        in_ptr: i32,
        in_len: i32,
        out_ptr: i32,
        out_cap: i32,
    ) -> wasmtime::Error {
        self.run_main
            .call(
                &mut self.store,
                (in_ptr, in_len, out_ptr, out_cap, i64::MAX),
            )
            .expect_err("expected a wasm trap")
    }
}

/// Build the `in_bytes` for a snippet that takes one String arg `s`.
fn build_str_input(main_schema: &Schema, name: &str, value: &str) -> Vec<u8> {
    let main_layout = SchemaLayout::offsets_for(main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder
        .write_string(name, value)
        .expect("write string field");
    builder.finish()
}

/// Read the single-string return value out of `out_bytes`.
fn read_string_return(return_schema: &Schema, out_bytes: &[u8]) -> String {
    let return_layout = SchemaLayout::offsets_for(return_schema).expect("return layout");
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, out_bytes).expect("reader");
    reader
        .read_string("value")
        .expect("read string value")
        .to_string()
}

/// Read the single-i64 return value out of `out_bytes`.
fn read_int_return(return_schema: &Schema, out_bytes: &[u8]) -> i64 {
    let return_layout = SchemaLayout::offsets_for(return_schema).expect("return layout");
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, out_bytes).expect("reader");
    reader.read_int("value").expect("read i64 value")
}

/// Read the single-bool return value out of `out_bytes`.
fn read_bool_return(return_schema: &Schema, out_bytes: &[u8]) -> bool {
    let return_layout = SchemaLayout::offsets_for(return_schema).expect("return layout");
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, out_bytes).expect("reader");
    reader.read_bool("value").expect("read bool value")
}

// ---------------------------------------------------------------------------
// concat(a: String, b: String) -> String
// ---------------------------------------------------------------------------

#[test]
fn concat_two_strings() {
    let (wasm, main_schema, return_schema) =
        compile("#main(String a, String b) -> String\nconcat(a, b)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("a", "foo").expect("write a");
    builder.write_string("b", "bar").expect("write b");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    assert!(bw as usize >= return_layout.root_size);
    let out = session.read(OUT_PTR as usize, bw as usize);
    let got = read_string_return(&return_schema, &out);
    assert_eq!(got, "foobar");
}

#[test]
fn concat_via_method() {
    let (wasm, main_schema, return_schema) =
        compile("#main(String a, String b) -> String\na.concat(b)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("a", "hello").expect("write a");
    builder.write_string("b", " world").expect("write b");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    let got = read_string_return(&return_schema, &out);
    assert_eq!(got, "hello world");
}

#[test]
fn concat_with_empty_left() {
    let (wasm, main_schema, return_schema) =
        compile("#main(String a, String b) -> String\nconcat(a, b)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("a", "").expect("write a");
    builder.write_string("b", "tail").expect("write b");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    let got = read_string_return(&return_schema, &out);
    assert_eq!(got, "tail");
}

// ---------------------------------------------------------------------------
// upper(s: String) -> String, lower(s: String) -> String
// ---------------------------------------------------------------------------

#[test]
fn upper_ascii_mix() {
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> String\nupper(s)");
    let in_bytes = build_str_input(&main_schema, "s", "Hello, World! 123");
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(
        read_string_return(&return_schema, &out),
        "HELLO, WORLD! 123"
    );
}

#[test]
fn lower_ascii_mix() {
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> String\nlower(s)");
    let in_bytes = build_str_input(&main_schema, "s", "Hello, WORLD! 123");
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(
        read_string_return(&return_schema, &out),
        "hello, world! 123"
    );
}

#[test]
fn upper_method_form() {
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> String\ns.upper()");
    let in_bytes = build_str_input(&main_schema, "s", "rust");
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_string_return(&return_schema, &out), "RUST");
}

#[test]
fn upper_empty_string() {
    let (wasm, main_schema, return_schema) = compile("#main(String s) -> String\nupper(s)");
    let in_bytes = build_str_input(&main_schema, "s", "");
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_string_return(&return_schema, &out), "");
}

// ---------------------------------------------------------------------------
// substring(s: String, start: Int, len: Int) -> String
// ---------------------------------------------------------------------------

#[test]
fn substring_middle_slice() {
    let (wasm, main_schema, return_schema) =
        compile("#main(String s, Int start, Int len) -> String\nsubstring(s, start, len)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", "hello world").expect("write s");
    builder.write_int("start", 6).expect("write start");
    builder.write_int("len", 5).expect("write len");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_string_return(&return_schema, &out), "world");
}

#[test]
fn substring_prefix() {
    let (wasm, main_schema, return_schema) =
        compile("#main(String s, Int start, Int len) -> String\nsubstring(s, start, len)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", "abcdef").expect("write s");
    builder.write_int("start", 0).expect("write start");
    builder.write_int("len", 3).expect("write len");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_string_return(&return_schema, &out), "abc");
}

#[test]
fn substring_out_of_bounds_traps() {
    let (wasm, main_schema, _return_schema) =
        compile("#main(String s, Int start, Int len) -> String\nsubstring(s, start, len)");
    let module = WasmModule::from_bytes(wasm.clone()).expect("load");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", "abc").expect("write s");
    builder.write_int("start", 1).expect("write start");
    // start + len = 5 > s.len = 3 → trap.
    builder.write_int("len", 4).expect("write len");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let err = session.call_expect_trap(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    match module.translate_trap(&err) {
        RuntimeError::WasmIndexOutOfBounds { .. } => {}
        other => panic!("expected WasmIndexOutOfBounds, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// starts_with(s: String, prefix: String) -> Bool
// ---------------------------------------------------------------------------

#[test]
fn starts_with_match() {
    let (wasm, main_schema, return_schema) =
        compile("#main(String s, String p) -> Bool\nstarts_with(s, p)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", "hello world").expect("write s");
    builder.write_string("p", "hello").expect("write p");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert!(read_bool_return(&return_schema, &out));
}

#[test]
fn starts_with_no_match() {
    let (wasm, main_schema, return_schema) =
        compile("#main(String s, String p) -> Bool\nstarts_with(s, p)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", "hello").expect("write s");
    builder.write_string("p", "world").expect("write p");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert!(!read_bool_return(&return_schema, &out));
}

#[test]
fn starts_with_prefix_longer_than_string() {
    let (wasm, main_schema, return_schema) =
        compile("#main(String s, String p) -> Bool\nstarts_with(s, p)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", "hi").expect("write s");
    builder.write_string("p", "hello").expect("write p");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert!(!read_bool_return(&return_schema, &out));
}

#[test]
fn starts_with_empty_prefix() {
    let (wasm, main_schema, return_schema) =
        compile("#main(String s, String p) -> Bool\nstarts_with(s, p)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", "anything").expect("write s");
    builder.write_string("p", "").expect("write p");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert!(read_bool_return(&return_schema, &out));
}

// ---------------------------------------------------------------------------
// list_int_sum(xs: List<Int>) -> Int
// ---------------------------------------------------------------------------

#[test]
fn list_int_sum_basic() {
    let (wasm, main_schema, return_schema) =
        compile("#main(List<Int> xs) -> Int\nlist_int_sum(xs)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder
        .write_list_int("xs", &[1, 2, 3, 4, 5])
        .expect("write xs");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_int_return(&return_schema, &out), 15);
}

#[test]
fn list_int_sum_empty() {
    let (wasm, main_schema, return_schema) =
        compile("#main(List<Int> xs) -> Int\nlist_int_sum(xs)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_list_int("xs", &[]).expect("write xs");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_int_return(&return_schema, &out), 0);
}

#[test]
fn list_int_sum_via_method() {
    let (wasm, main_schema, return_schema) = compile("#main(List<Int> xs) -> Int\nxs.sum()");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder
        .write_list_int("xs", &[-1, 1, 100])
        .expect("write xs");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_int_return(&return_schema, &out), 100);
}

// ---------------------------------------------------------------------------
// list_int_max(xs: List<Int>) -> Int
// ---------------------------------------------------------------------------

#[test]
fn list_int_max_basic() {
    let (wasm, main_schema, return_schema) =
        compile("#main(List<Int> xs) -> Int\nlist_int_max(xs)");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder
        .write_list_int("xs", &[3, 1, 4, 1, 5, 9, 2, 6])
        .expect("write xs");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_int_return(&return_schema, &out), 9);
}

#[test]
fn list_int_max_empty_traps() {
    let (wasm, main_schema, _return_schema) =
        compile("#main(List<Int> xs) -> Int\nlist_int_max(xs)");
    let module = WasmModule::from_bytes(wasm.clone()).expect("load");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_list_int("xs", &[]).expect("write xs");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let err = session.call_expect_trap(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    match module.translate_trap(&err) {
        RuntimeError::WasmEmptyList { .. } => {}
        other => panic!("expected WasmEmptyList, got {other:?}"),
    }
}

#[test]
fn list_int_max_via_method() {
    let (wasm, main_schema, return_schema) = compile("#main(List<Int> xs) -> Int\nxs.max()");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder
        .write_list_int("xs", &[-10, -20, -3])
        .expect("write xs");
    let in_bytes = builder.finish();

    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_int_return(&return_schema, &out), -3);
}
