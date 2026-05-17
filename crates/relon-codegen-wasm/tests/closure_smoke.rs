//! Phase 10-a closure roundtrip tests.
//!
//! End-to-end coverage for the closure machinery added in this phase:
//! `list_int_map` / `list_int_filter` / `list_int_fold` driven through
//! the user-facing surface `xs.map(|x| ...)` / `xs.filter(|x| ...)` /
//! `xs.fold(init, |acc, x| ...)`. Each test compiles a real Relon
//! snippet through the parser -> analyzer -> IR -> codegen -> wasmtime
//! pipeline shared with `stdlib_phase4c2_smoke.rs`.
//!
//! Coverage:
//!   * `xs.map((Int x) => x * 2)` over `[1, 2, 3]` -> `[2, 4, 6]`.
//!   * `xs.filter((Int x) => x > 0)` over `[-1, 2, -3, 4]` -> `[2, 4]`.
//!   * `xs.fold(0, (Int acc, Int x) => acc + x)` over `[1, 2, 3]` -> `6`.
//!   * Capture-bearing closure: `xs.map((Int x) => x * factor)` with
//!     `factor` defined in an outer `where` clause.
//!   * Boundary rejection: `#main((Int x) => x * 2)` surfaces
//!     `LoweringError::ClosureAcrossBoundary`.

use relon_codegen_wasm::compile_lowered_entry;
use relon_eval_api::buffer::{BufferBuilder, BufferReader};
use relon_eval_api::layout::SchemaLayout;
use relon_eval_api::schema_canonical::Schema;
use relon_ir::lower_workspace_single;
use wasmtime::{Engine, Instance, Memory, Module, Store, TypedFunc};

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
const OUT_PTR: i32 = 1024;
const OUT_CAP: i32 = 512;

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

    /// Phase 11: closures in this file don't touch capability-guarded
    /// host fns, so `caps_avail` is always `i64::MAX`.
    fn call(&mut self, in_ptr: i32, in_len: i32, out_ptr: i32, out_cap: i32) -> i32 {
        self.run_main
            .call(
                &mut self.store,
                (in_ptr, in_len, out_ptr, out_cap, i64::MAX),
            )
            .expect("run_main call must not trap")
    }
}

fn build_list_input(main_schema: &Schema, name: &str, values: &[i64]) -> Vec<u8> {
    let main_layout = SchemaLayout::offsets_for(main_schema).expect("main layout");
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder
        .write_list_int(name, values)
        .expect("write list field");
    builder.finish()
}

fn read_list_return(return_schema: &Schema, out_bytes: &[u8]) -> Vec<i64> {
    let return_layout = SchemaLayout::offsets_for(return_schema).expect("return layout");
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, out_bytes).expect("reader");
    reader.read_list_int("value").expect("read list value")
}

fn read_int_return(return_schema: &Schema, out_bytes: &[u8]) -> i64 {
    let return_layout = SchemaLayout::offsets_for(return_schema).expect("return layout");
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, out_bytes).expect("reader");
    reader.read_int("value").expect("read i64 value")
}

#[test]
fn list_int_map_doubles_elements() {
    let (wasm, main_schema, return_schema) =
        compile("#main(List<Int> xs) -> List<Int>\nxs.map((Int x) => x * 2)");
    let in_bytes = build_list_input(&main_schema, "xs", &[1, 2, 3]);
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_list_return(&return_schema, &out), vec![2, 4, 6]);
}

#[test]
fn list_int_map_empty_passthrough() {
    let (wasm, main_schema, return_schema) =
        compile("#main(List<Int> xs) -> List<Int>\nxs.map((Int x) => x + 100)");
    let in_bytes = build_list_input(&main_schema, "xs", &[]);
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_list_return(&return_schema, &out), Vec::<i64>::new());
}

#[test]
fn list_int_filter_keeps_positive() {
    let (wasm, main_schema, return_schema) =
        compile("#main(List<Int> xs) -> List<Int>\nxs.filter((Int x) => x > 0)");
    let in_bytes = build_list_input(&main_schema, "xs", &[-1, 2, -3, 4]);
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_list_return(&return_schema, &out), vec![2, 4]);
}

#[test]
fn list_int_filter_all_rejected() {
    let (wasm, main_schema, return_schema) =
        compile("#main(List<Int> xs) -> List<Int>\nxs.filter((Int x) => x > 100)");
    let in_bytes = build_list_input(&main_schema, "xs", &[1, 2, 3]);
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_list_return(&return_schema, &out), Vec::<i64>::new());
}

#[test]
fn list_int_fold_sums() {
    let (wasm, main_schema, return_schema) =
        compile("#main(List<Int> xs) -> Int\nxs.fold(0, (Int acc, Int x) => acc + x)");
    let in_bytes = build_list_input(&main_schema, "xs", &[1, 2, 3]);
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_int_return(&return_schema, &out), 6);
}

#[test]
fn list_int_fold_empty_returns_init() {
    let (wasm, main_schema, return_schema) =
        compile("#main(List<Int> xs) -> Int\nxs.fold(42, (Int acc, Int x) => acc + x)");
    let in_bytes = build_list_input(&main_schema, "xs", &[]);
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_int_return(&return_schema, &out), 42);
}

#[test]
fn list_int_map_with_captured_let() {
    // `factor` is bound in a `where` clause and captured by the
    // lambda body. Exercises the closure-conversion path that
    // copies a captured let-local into the captures struct.
    let (wasm, main_schema, return_schema) = compile(
        "#main(List<Int> xs) -> List<Int>\n\
         xs.map((Int x) => x * factor) where { factor: 10 }",
    );
    let in_bytes = build_list_input(&main_schema, "xs", &[1, 2, 3]);
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    assert_eq!(read_list_return(&return_schema, &out), vec![10, 20, 30]);
}

#[test]
fn list_int_fold_with_captured_seed() {
    // Captured `bias` rides on the closure, the fold's `init` is a
    // plain literal — exercises mixed-arg closure invocation.
    let (wasm, main_schema, return_schema) = compile(
        "#main(List<Int> xs) -> Int\n\
         xs.fold(0, (Int acc, Int x) => acc + x + bias) where { bias: 100 }",
    );
    let in_bytes = build_list_input(&main_schema, "xs", &[1, 2, 3]);
    let mut session = WasmSession::new(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bw = session.call(IN_PTR, in_bytes.len() as i32, OUT_PTR, OUT_CAP);
    let out = session.read(OUT_PTR as usize, bw as usize);
    // (((0 + 1 + 100) + 2 + 100) + 3 + 100) = 306
    assert_eq!(read_int_return(&return_schema, &out), 306);
}

// ---------------------------------------------------------------------------
// Boundary rejection: closure can't cross #main.
// ---------------------------------------------------------------------------

#[test]
fn closure_typed_main_param_rejected() {
    let src = "#main(Closure<Int, Int> f) -> Int\nf";
    let ast = relon_parser::parse_document(src).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    // The analyzer may or may not report this — the IR side is the
    // authoritative defence. We tolerate analyzer noise here and
    // check the lowering's reject directly.
    let err = lower_workspace_single(&analyzed, &ast)
        .expect_err("lowering should reject closure-typed #main param");
    let msg = format!("{}", err);
    assert!(msg.contains("closure"), "unexpected error message: {msg}");
}

#[test]
fn closure_returned_from_main_rejected() {
    let src = "#main(Int x) -> Closure<Int, Int>\n(Int y) => y + x";
    let ast = relon_parser::parse_document(src).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    let err = lower_workspace_single(&analyzed, &ast)
        .expect_err("lowering should reject closure-typed #main return");
    let msg = format!("{}", err);
    assert!(msg.contains("closure"), "unexpected error message: {msg}");
}
