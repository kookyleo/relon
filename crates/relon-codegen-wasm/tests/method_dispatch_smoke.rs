//! Phase 5 schema-method dispatch end-to-end smoke tests.
//!
//! Exercise the full pipeline: parser -> analyzer -> IR (schema
//! methods materialised as IR funcs + entry resolves `obj.method()`
//! via the registry) -> codegen-wasm -> wasmtime. Each test pins a
//! distinct dispatch shape:
//!
//! * `simple_method_returns_bool` — predicate method on a 1-field
//!   schema, called from `#main`.
//! * `method_with_args` — multi-arg method taking another instance
//!   of the same schema.
//! * `method_called_inside_method` — `self.bar()` from inside `foo()`
//!   on the same schema; tests the inter-method self-dispatch path.
//! * `method_returns_int` — covers the i64 return-slot variant.

use relon_codegen_wasm::compile_lowered_entry;
use relon_eval_api::buffer::{BufferBuilder, BufferReader};
use relon_eval_api::layout::{OffsetTable, SchemaLayout};
use relon_eval_api::schema_canonical::{Schema, TypeRepr};
use relon_ir::lower_workspace_single;
use wasmtime::{
    Engine, Global, Instance, Memory, Module, Mutability, Store, TypedFunc, Val, ValType,
};

/// Pack a schema-typed parameter into a `MainParams`-shaped buffer.
///
/// The host SDK's [`BufferBuilder`] only exposes scalar / String /
/// List<Int> writers in Phase 5; schema-typed slots ride a pointer
/// to a buffer-relative offset, so the test fixtures patch them in
/// directly. `parent_bytes` is the in-progress buffer (already sized
/// to fit the fixed area). `slot_offset` is the pointer-slot's byte
/// position inside the fixed area. `sub_bytes` is the materialised
/// fixed-area + tail-area record for the schema instance the param
/// names; `sub_alignment` is the schema's root-area alignment.
fn patch_schema_param(
    parent_bytes: &mut Vec<u8>,
    slot_offset: usize,
    sub_alignment: usize,
    sub_bytes: &[u8],
) {
    // Pad the parent buffer up to the sub-record's required alignment
    // before appending.
    if sub_alignment > 1 {
        let rem = parent_bytes.len() % sub_alignment;
        if rem != 0 {
            parent_bytes.resize(parent_bytes.len() + (sub_alignment - rem), 0);
        }
    }
    let sub_offset = parent_bytes.len() as u32;
    parent_bytes.extend_from_slice(sub_bytes);
    parent_bytes[slot_offset..slot_offset + 4].copy_from_slice(&sub_offset.to_le_bytes());
}

/// Resolve a schema-typed field's `(slot_offset, sub_schema)` pair on
/// the `MainParams`-shaped layout/schema pair.
fn schema_field_meta<'a>(
    parent_layout: &'a OffsetTable,
    parent_schema: &'a Schema,
    field_name: &str,
) -> (usize, Schema) {
    let layout_field = parent_layout
        .fields
        .iter()
        .find(|f| f.name == field_name)
        .expect("layout field");
    let schema_field = parent_schema
        .fields
        .iter()
        .find(|f| f.name == field_name)
        .expect("schema field");
    let sub_schema = match &schema_field.ty {
        TypeRepr::Schema { schema } => (**schema).clone(),
        other => panic!("expected schema field for `{field_name}`, got {other:?}"),
    };
    (layout_field.offset, sub_schema)
}

fn compile(src: &str) -> (Vec<u8>, Schema, Schema) {
    let ast = relon_parser::parse_document(src).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    // Allow non-fatal diagnostics — the analyzer still flags unknown
    // identifiers under method bodies until Phase B closes its
    // outstanding gaps. Hard errors (parse failures, missing schema
    // refs) still bail.
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
fn simple_method_returns_bool() {
    let src = "#schema User { Int age: * } with {\n  \
        is_adult() -> Bool: self.age >= 18\n\
        }\n\
        #main(User u) -> Bool\n\
        u.is_adult()";
    let (wasm, main_schema, return_schema) = compile(src);
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    // Adult: 20.
    let (slot_offset, user_schema) = schema_field_meta(&main_layout, &main_schema, "u");
    let user_layout = SchemaLayout::offsets_for(&user_schema).expect("user layout");
    let mut user_builder = BufferBuilder::new(&user_layout, &user_schema.fields);
    user_builder.write_int("age", 20).expect("write age");
    let user_bytes = user_builder.finish();
    let mut in_bytes = vec![0u8; main_layout.root_size];
    patch_schema_param(
        &mut in_bytes,
        slot_offset,
        user_layout.root_align,
        &user_bytes,
    );

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

    // Minor: 16.
    let mut user_builder = BufferBuilder::new(&user_layout, &user_schema.fields);
    user_builder.write_int("age", 16).expect("write age");
    let user_bytes = user_builder.finish();
    let mut in_bytes = vec![0u8; main_layout.root_size];
    patch_schema_param(
        &mut in_bytes,
        slot_offset,
        user_layout.root_align,
        &user_bytes,
    );

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
fn method_returns_int() {
    let src = "#schema Box { Int n: * } with {\n  \
        doubled() -> Int: self.n * 2\n\
        }\n\
        #main(Box b) -> Int\n\
        b.doubled()";
    let (wasm, main_schema, return_schema) = compile(src);
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let (slot_offset, box_schema) = schema_field_meta(&main_layout, &main_schema, "b");
    let box_layout = SchemaLayout::offsets_for(&box_schema).expect("box layout");

    let mut box_builder = BufferBuilder::new(&box_layout, &box_schema.fields);
    box_builder.write_int("n", 21).expect("write n");
    let box_bytes = box_builder.finish();
    let mut in_bytes = vec![0u8; main_layout.root_size];
    patch_schema_param(
        &mut in_bytes,
        slot_offset,
        box_layout.root_align,
        &box_bytes,
    );

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
fn method_with_args() {
    let src = "#schema Vec2 { Int x: *, Int y: * } with {\n  \
        dot(other: Vec2) -> Int: self.x * other.x + self.y * other.y\n\
        }\n\
        #main(Vec2 a, Vec2 b) -> Int\n\
        a.dot(b)";
    let (wasm, main_schema, return_schema) = compile(src);
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let (a_slot, vec2_schema) = schema_field_meta(&main_layout, &main_schema, "a");
    let (b_slot, _) = schema_field_meta(&main_layout, &main_schema, "b");
    let vec2_layout = SchemaLayout::offsets_for(&vec2_schema).expect("vec2 layout");

    let mut a_builder = BufferBuilder::new(&vec2_layout, &vec2_schema.fields);
    a_builder.write_int("x", 3).expect("a.x");
    a_builder.write_int("y", 4).expect("a.y");
    let a_bytes = a_builder.finish();
    let mut b_builder = BufferBuilder::new(&vec2_layout, &vec2_schema.fields);
    b_builder.write_int("x", 5).expect("b.x");
    b_builder.write_int("y", 6).expect("b.y");
    let b_bytes = b_builder.finish();

    let mut in_bytes = vec![0u8; main_layout.root_size];
    patch_schema_param(&mut in_bytes, a_slot, vec2_layout.root_align, &a_bytes);
    patch_schema_param(&mut in_bytes, b_slot, vec2_layout.root_align, &b_bytes);

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
    // 3*5 + 4*6 = 15 + 24 = 39.
    assert_eq!(reader.read_int("value").expect("read value"), 39);
}

#[test]
fn method_called_inside_method() {
    // `foo()` calls `self.bar()` — the inter-method self-dispatch
    // path. `bar()` doubles `self.x`; `foo()` adds 1 to that.
    let src = "#schema Cell { Int x: * } with {\n  \
        bar() -> Int: self.x * 2\n  \
        foo() -> Int: self.bar() + 1\n\
        }\n\
        #main(Cell c) -> Int\n\
        c.foo()";
    let (wasm, main_schema, return_schema) = compile(src);
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let (slot_offset, cell_schema) = schema_field_meta(&main_layout, &main_schema, "c");
    let cell_layout = SchemaLayout::offsets_for(&cell_schema).expect("cell layout");

    let mut cell_builder = BufferBuilder::new(&cell_layout, &cell_schema.fields);
    cell_builder.write_int("x", 7).expect("write x");
    let cell_bytes = cell_builder.finish();
    let mut in_bytes = vec![0u8; main_layout.root_size];
    patch_schema_param(
        &mut in_bytes,
        slot_offset,
        cell_layout.root_align,
        &cell_bytes,
    );

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
    // (7 * 2) + 1 = 15.
    assert_eq!(reader.read_int("value").expect("read value"), 15);
}
