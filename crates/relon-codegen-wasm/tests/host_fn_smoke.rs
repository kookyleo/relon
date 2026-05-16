//! Phase 6 host-fn integration tests.
//!
//! These tests build IR modules by hand because the parser /
//! analyzer doesn't yet accept a top-level `#native fn` declaration
//! — only schema-method `#native` is wired through. Hand-built IR
//! lets us exercise the full Phase 6 surface (import emit, capability
//! check, host-fn signature validation) without waiting on the
//! syntax landing.
//!
//! Coverage:
//!
//! * `echo_string_roundtrip` — single `#native fn echo(String) ->
//!   String` invocation, returns the input pointer unchanged so the
//!   wasm side copies it back to `out_buf` tail area.
//! * `multi_param_native_add` — `add(Int, Int) -> Int` with a const
//!   second argument.
//! * `capability_granted_allows_call` /
//!   `capability_missing_traps` — `write_file(...)` guarded behind
//!   capability bit 0; trap path and success path.
//! * `missing_host_fn_rejected` — module declares `echo`, host SDK
//!   supplies an empty table → `MissingHostFn`.
//! * `signature_drift_rejected` — host SDK supplies a same-named fn
//!   with mismatched canonical hash → `HostFnSignatureDrift`.
//! * `host_fns_section_present` — module emits the `relon.host_fns`
//!   custom section with the declared entries.

use relon_codegen_wasm::{
    compile_module_with_host_fns, hash_params, hash_return, host_fns::SECTION_NAME, HostFnEntry,
    HostFnTable, LoadError, WasmModule, NO_CAPABILITY,
};
use relon_eval_api::buffer::{BufferBuilder, BufferReader};
use relon_eval_api::layout::SchemaLayout;
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_ir::{Func, IrType, Module as IrModule, NativeImport, Op, TaggedOp};
use relon_parser::TokenRange;
use wasmparser::{Parser, Payload};
use wasmtime::{
    Caller, Engine, Global, Linker, Memory, Module, Mutability, Store, TypedFunc, Val, ValType,
};

/// Canonical entry-fn synthetic source range. Stdlib bodies use the
/// same `(1, 1, 0)` anchor so the srcmap encoder's 1-based invariant
/// stays happy.
fn synth_range() -> TokenRange {
    TokenRange {
        start: relon_parser::TokenPosition {
            line: 1,
            column: 1,
            offset: 0,
        },
        end: relon_parser::TokenPosition {
            line: 1,
            column: 1,
            offset: 0,
        },
    }
}

/// Wrap `op` in a [`TaggedOp`] with the synthetic source range.
fn t(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: synth_range(),
    }
}

/// Build a single-field input schema with `name: String`.
fn string_in_schema(field_name: &str) -> Schema {
    Schema {
        name: "MainParams".to_string(),
        generics: vec![],
        fields: vec![Field {
            name: field_name.to_string(),
            ty: TypeRepr::String,
            default: None,
        }],
    }
}

/// Build a single-field return schema with `value: String`.
fn string_return_schema() -> Schema {
    Schema {
        name: "Ret".to_string(),
        generics: vec![],
        fields: vec![Field {
            name: "value".to_string(),
            ty: TypeRepr::String,
            default: None,
        }],
    }
}

/// Build a single-field input schema with `name: Int`.
fn int_in_schema(field_name: &str) -> Schema {
    Schema {
        name: "MainParams".to_string(),
        generics: vec![],
        fields: vec![Field {
            name: field_name.to_string(),
            ty: TypeRepr::Int,
            default: None,
        }],
    }
}

/// Build a single-field return schema with `value: Int`.
fn int_return_schema() -> Schema {
    Schema {
        name: "Ret".to_string(),
        generics: vec![],
        fields: vec![Field {
            name: "value".to_string(),
            ty: TypeRepr::Int,
            default: None,
        }],
    }
}

/// Build a single-field return schema with `value: Bool`.
fn bool_return_schema() -> Schema {
    Schema {
        name: "Ret".to_string(),
        generics: vec![],
        fields: vec![Field {
            name: "value".to_string(),
            ty: TypeRepr::Bool,
            default: None,
        }],
    }
}

/// Bare two-field input schema `(path: String, content: String)`.
fn two_string_in_schema() -> Schema {
    Schema {
        name: "MainParams".to_string(),
        generics: vec![],
        fields: vec![
            Field {
                name: "path".to_string(),
                ty: TypeRepr::String,
                default: None,
            },
            Field {
                name: "content".to_string(),
                ty: TypeRepr::String,
                default: None,
            },
        ],
    }
}

/// Look up the offset of the named field inside `layout`. Test
/// fixtures use it to map a schema field name to its byte offset
/// inside the in_buf or out_buf record.
fn offset_of(layout: &relon_eval_api::layout::OffsetTable, field: &str) -> u32 {
    let entry = layout
        .fields
        .iter()
        .find(|f| f.name == field)
        .unwrap_or_else(|| panic!("field `{field}` not in layout"));
    entry.offset as u32
}

const IN_PTR: i32 = 0;
const OUT_PTR: i32 = 1024;

/// Build the wasmtime store + linker pair plus the imported
/// `relon_caps_avail` global. Centralises the boilerplate every test
/// would otherwise repeat.
fn setup_store_with_caps(caps_avail: i64) -> (Store<()>, Linker<()>, Engine, Global) {
    let engine = Engine::default();
    let mut store: Store<()> = Store::new(&engine, ());
    let caps_avail_global = Global::new(
        &mut store,
        wasmtime::GlobalType::new(ValType::I64, Mutability::Const),
        Val::I64(caps_avail),
    )
    .expect("create relon_caps_avail global");
    let mut linker: Linker<()> = Linker::new(&engine);
    linker
        .define(&mut store, "env", "relon_caps_avail", caps_avail_global)
        .expect("define caps_avail import");
    (store, linker, engine, caps_avail_global)
}

#[test]
fn echo_string_roundtrip() {
    // IR layout: an entry function that loads the `s` String pointer
    // from the in_buf, calls `echo(s)` (which returns the same pointer
    // unchanged), and stores the returned pointer back through a
    // String StoreField (which memcpys the `[len][bytes]` record into
    // the out_buf tail area).
    let main_schema = string_in_schema("s");
    let return_schema = string_return_schema();
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let s_offset = offset_of(&main_layout, "s");
    let value_offset = offset_of(&return_layout, "value");

    let ir_module = IrModule {
        imports: vec![NativeImport {
            name: "echo".to_string(),
            param_tys: vec![IrType::String],
            ret_ty: IrType::String,
            cap_bit: relon_ir::NO_CAPABILITY_BIT,
        }],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret: IrType::I32,
            range: synth_range(),
            body: vec![
                t(Op::LoadStringPtr { offset: s_offset }),
                t(Op::CallNative {
                    import_idx: 0,
                    param_tys: vec![IrType::String],
                    ret_ty: IrType::String,
                    cap_bit: relon_ir::NO_CAPABILITY_BIT,
                }),
                t(Op::StoreField {
                    offset: value_offset,
                    ty: IrType::String,
                }),
                t(Op::Return),
            ],
        }],
        entry_func_index: Some(0),
    };

    let host_fns = HostFnTable {
        entries: vec![HostFnEntry {
            name: "echo".to_string(),
            params_canonical_hash: hash_params(&[IrType::String]),
            ret_canonical_hash: hash_return(IrType::String),
            cap_bit: NO_CAPABILITY,
        }],
    };

    let wasm = compile_module_with_host_fns(&ir_module, &main_schema, &return_schema, &host_fns)
        .expect("compile");

    // Host fn body: identity — return the input pointer unchanged so
    // the wasm side memcpys the same record into the out_buf.
    let (mut store, mut linker, engine, _caps) = setup_store_with_caps(i64::MAX);
    linker
        .func_wrap("env", "echo", |_caller: Caller<'_, ()>, ptr: i32| -> i32 {
            ptr
        })
        .expect("define echo");
    let module = Module::new(&engine, &wasm).expect("module load");
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate");

    let memory: Memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    let run_main: TypedFunc<(i32, i32, i32, i32), i32> = instance
        .get_typed_func(&mut store, "run_main")
        .expect("run_main");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", "ping").expect("write s");
    let in_bytes = builder.finish();
    memory
        .write(&mut store, IN_PTR as usize, &in_bytes)
        .expect("memwrite");

    let out_cap = (return_layout.root_size + 4 + "ping".len()) as i32;
    let bytes_written = run_main
        .call(
            &mut store,
            (IN_PTR, in_bytes.len() as i32, OUT_PTR, out_cap),
        )
        .expect("run_main");
    assert!(bytes_written > return_layout.root_size as i32);

    let mut out = vec![0u8; bytes_written as usize];
    memory
        .read(&mut store, OUT_PTR as usize, &mut out)
        .expect("memread");
    let reader = BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
    assert_eq!(reader.read_string("value").expect("read value"), "ping");
}

#[test]
fn multi_param_native_add() {
    // `add(a, b)` with both arguments resolved from the in_buf is
    // tedious to lay out by hand because the second slot would need
    // its own schema field. Keep the surface simple: feed `add(x, 1)`
    // — the `1` becomes a `ConstI64` op, exercising the multi-param
    // path without requiring a 2-field schema.
    let main_schema = int_in_schema("x");
    let return_schema = int_return_schema();
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let x_offset = offset_of(&main_layout, "x");
    let value_offset = offset_of(&return_layout, "value");

    let ir_module = IrModule {
        imports: vec![NativeImport {
            name: "add".to_string(),
            param_tys: vec![IrType::I64, IrType::I64],
            ret_ty: IrType::I64,
            cap_bit: relon_ir::NO_CAPABILITY_BIT,
        }],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret: IrType::I32,
            range: synth_range(),
            body: vec![
                t(Op::LoadField {
                    offset: x_offset,
                    ty: IrType::I64,
                }),
                t(Op::ConstI64(1)),
                t(Op::CallNative {
                    import_idx: 0,
                    param_tys: vec![IrType::I64, IrType::I64],
                    ret_ty: IrType::I64,
                    cap_bit: relon_ir::NO_CAPABILITY_BIT,
                }),
                t(Op::StoreField {
                    offset: value_offset,
                    ty: IrType::I64,
                }),
                t(Op::Return),
            ],
        }],
        entry_func_index: Some(0),
    };

    let host_fns = HostFnTable {
        entries: vec![HostFnEntry {
            name: "add".to_string(),
            params_canonical_hash: hash_params(&[IrType::I64, IrType::I64]),
            ret_canonical_hash: hash_return(IrType::I64),
            cap_bit: NO_CAPABILITY,
        }],
    };

    let wasm = compile_module_with_host_fns(&ir_module, &main_schema, &return_schema, &host_fns)
        .expect("compile");

    let (mut store, mut linker, engine, _caps) = setup_store_with_caps(i64::MAX);
    linker
        .func_wrap(
            "env",
            "add",
            |_caller: Caller<'_, ()>, a: i64, b: i64| -> i64 { a + b },
        )
        .expect("define add");
    let module = Module::new(&engine, &wasm).expect("module load");
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate");
    let memory: Memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    let run_main: TypedFunc<(i32, i32, i32, i32), i32> = instance
        .get_typed_func(&mut store, "run_main")
        .expect("run_main");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 41).expect("write x");
    let in_bytes = builder.finish();
    memory
        .write(&mut store, IN_PTR as usize, &in_bytes)
        .expect("memwrite");
    let bw = run_main
        .call(
            &mut store,
            (
                IN_PTR,
                in_bytes.len() as i32,
                OUT_PTR,
                return_layout.root_size as i32,
            ),
        )
        .expect("run_main");
    assert_eq!(bw as usize, return_layout.root_size);
    let mut out = vec![0u8; return_layout.root_size];
    memory
        .read(&mut store, OUT_PTR as usize, &mut out)
        .expect("memread");
    let reader = BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
    assert_eq!(reader.read_int("value").expect("read value"), 42);
}

/// Build the `write_file(path, content)` IR module + host-fns table.
/// Used by both the capability-granted and capability-denied tests.
fn write_file_module() -> (
    IrModule,
    HostFnTable,
    Schema,
    Schema,
    relon_eval_api::layout::OffsetTable,
    relon_eval_api::layout::OffsetTable,
) {
    let main_schema = two_string_in_schema();
    let return_schema = bool_return_schema();
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let path_offset = offset_of(&main_layout, "path");
    let content_offset = offset_of(&main_layout, "content");
    let value_offset = offset_of(&return_layout, "value");

    let ir_module = IrModule {
        imports: vec![NativeImport {
            name: "write_file".to_string(),
            param_tys: vec![IrType::String, IrType::String],
            ret_ty: IrType::Bool,
            cap_bit: 0,
        }],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret: IrType::I32,
            range: synth_range(),
            body: vec![
                t(Op::LoadStringPtr {
                    offset: path_offset,
                }),
                t(Op::LoadStringPtr {
                    offset: content_offset,
                }),
                t(Op::CallNative {
                    import_idx: 0,
                    param_tys: vec![IrType::String, IrType::String],
                    ret_ty: IrType::Bool,
                    cap_bit: 0,
                }),
                t(Op::StoreField {
                    offset: value_offset,
                    ty: IrType::Bool,
                }),
                t(Op::Return),
            ],
        }],
        entry_func_index: Some(0),
    };

    let host_fns = HostFnTable {
        entries: vec![HostFnEntry {
            name: "write_file".to_string(),
            params_canonical_hash: hash_params(&[IrType::String, IrType::String]),
            ret_canonical_hash: hash_return(IrType::Bool),
            cap_bit: 0,
        }],
    };

    (
        ir_module,
        host_fns,
        main_schema,
        return_schema,
        main_layout,
        return_layout,
    )
}

#[test]
fn capability_granted_allows_call() {
    let (ir_module, host_fns, main_schema, return_schema, main_layout, return_layout) =
        write_file_module();

    let wasm = compile_module_with_host_fns(&ir_module, &main_schema, &return_schema, &host_fns)
        .expect("compile");

    // Confirm `required_capabilities` rolled bit 0 into the ABI
    // metadata so a host SDK can run the subset check before
    // instantiating.
    let module = WasmModule::from_bytes(wasm.clone()).expect("load");
    assert_eq!(module.abi().required_capabilities, 0b1);

    let (mut store, mut linker, engine, _caps) = setup_store_with_caps(0b1);
    linker
        .func_wrap(
            "env",
            "write_file",
            |_caller: Caller<'_, ()>, _p: i32, _c: i32| -> i32 { 1 },
        )
        .expect("define write_file");
    let module = Module::new(&engine, &wasm).expect("module load");
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate");
    let memory: Memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    let run_main: TypedFunc<(i32, i32, i32, i32), i32> = instance
        .get_typed_func(&mut store, "run_main")
        .expect("run_main");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("path", "/tmp/x").expect("write path");
    builder
        .write_string("content", "hi")
        .expect("write content");
    let in_bytes = builder.finish();
    memory
        .write(&mut store, IN_PTR as usize, &in_bytes)
        .expect("memwrite");
    let bw = run_main
        .call(
            &mut store,
            (
                IN_PTR,
                in_bytes.len() as i32,
                OUT_PTR,
                return_layout.root_size as i32,
            ),
        )
        .expect("run_main");
    assert_eq!(bw as usize, return_layout.root_size);
    let mut out = vec![0u8; return_layout.root_size];
    memory
        .read(&mut store, OUT_PTR as usize, &mut out)
        .expect("memread");
    let reader = BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
    assert!(reader.read_bool("value").expect("read value"));
}

#[test]
fn capability_missing_traps() {
    let (ir_module, host_fns, main_schema, return_schema, main_layout, _return_layout) =
        write_file_module();

    let wasm = compile_module_with_host_fns(&ir_module, &main_schema, &return_schema, &host_fns)
        .expect("compile");

    // `caps_avail = 0` — wasm-side `check_cap` must trap before the
    // host fn ever executes.
    let (mut store, mut linker, engine, _caps) = setup_store_with_caps(0);
    let host_call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host_call_count_clone = host_call_count.clone();
    linker
        .func_wrap(
            "env",
            "write_file",
            move |_caller: Caller<'_, ()>, _p: i32, _c: i32| -> i32 {
                host_call_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                1
            },
        )
        .expect("define write_file");
    let module = Module::new(&engine, &wasm).expect("module load");
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate");
    let memory: Memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    let run_main: TypedFunc<(i32, i32, i32, i32), i32> = instance
        .get_typed_func(&mut store, "run_main")
        .expect("run_main");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("path", "/tmp/x").expect("write path");
    builder
        .write_string("content", "hi")
        .expect("write content");
    let in_bytes = builder.finish();
    memory
        .write(&mut store, IN_PTR as usize, &in_bytes)
        .expect("memwrite");
    let err = run_main
        .call(&mut store, (IN_PTR, in_bytes.len() as i32, OUT_PTR, 32))
        .expect_err("must trap when capability is missing");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("wasm `unreachable` instruction executed") || msg.contains("unreachable"),
        "expected unreachable trap, got: {msg}"
    );
    assert_eq!(
        host_call_count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "host fn must not be invoked when capability is denied"
    );
}

#[test]
fn missing_host_fn_rejected() {
    // Module declares `echo`; host SDK supplies an empty table →
    // `LoadError::MissingHostFn { name = "echo" }`.
    let main_schema = string_in_schema("s");
    let return_schema = string_return_schema();
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let s_offset = offset_of(&main_layout, "s");
    let value_offset = offset_of(&return_layout, "value");

    let ir_module = IrModule {
        imports: vec![NativeImport {
            name: "echo".to_string(),
            param_tys: vec![IrType::String],
            ret_ty: IrType::String,
            cap_bit: relon_ir::NO_CAPABILITY_BIT,
        }],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret: IrType::I32,
            range: synth_range(),
            body: vec![
                t(Op::LoadStringPtr { offset: s_offset }),
                t(Op::CallNative {
                    import_idx: 0,
                    param_tys: vec![IrType::String],
                    ret_ty: IrType::String,
                    cap_bit: relon_ir::NO_CAPABILITY_BIT,
                }),
                t(Op::StoreField {
                    offset: value_offset,
                    ty: IrType::String,
                }),
                t(Op::Return),
            ],
        }],
        entry_func_index: Some(0),
    };

    let declared_table = HostFnTable {
        entries: vec![HostFnEntry {
            name: "echo".to_string(),
            params_canonical_hash: hash_params(&[IrType::String]),
            ret_canonical_hash: hash_return(IrType::String),
            cap_bit: NO_CAPABILITY,
        }],
    };
    let wasm =
        compile_module_with_host_fns(&ir_module, &main_schema, &return_schema, &declared_table)
            .expect("compile");
    let _ = return_layout;

    let empty_host = HostFnTable::empty();
    match WasmModule::from_bytes_with_host_fns(wasm, &empty_host) {
        Err(LoadError::MissingHostFn { name }) => assert_eq!(name, "echo"),
        other => panic!("expected MissingHostFn, got {other:?}"),
    }
}

#[test]
fn signature_drift_rejected() {
    // Module declares `echo(String) -> String`; host SDK supplies a
    // same-named fn with `(Int) -> Int` (different hashes). Loader
    // surfaces `LoadError::HostFnSignatureDrift`.
    let main_schema = string_in_schema("s");
    let return_schema = string_return_schema();
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let s_offset = offset_of(&main_layout, "s");
    let value_offset = offset_of(&return_layout, "value");

    let ir_module = IrModule {
        imports: vec![NativeImport {
            name: "echo".to_string(),
            param_tys: vec![IrType::String],
            ret_ty: IrType::String,
            cap_bit: relon_ir::NO_CAPABILITY_BIT,
        }],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret: IrType::I32,
            range: synth_range(),
            body: vec![
                t(Op::LoadStringPtr { offset: s_offset }),
                t(Op::CallNative {
                    import_idx: 0,
                    param_tys: vec![IrType::String],
                    ret_ty: IrType::String,
                    cap_bit: relon_ir::NO_CAPABILITY_BIT,
                }),
                t(Op::StoreField {
                    offset: value_offset,
                    ty: IrType::String,
                }),
                t(Op::Return),
            ],
        }],
        entry_func_index: Some(0),
    };

    let declared_table = HostFnTable {
        entries: vec![HostFnEntry {
            name: "echo".to_string(),
            params_canonical_hash: hash_params(&[IrType::String]),
            ret_canonical_hash: hash_return(IrType::String),
            cap_bit: NO_CAPABILITY,
        }],
    };
    let wasm =
        compile_module_with_host_fns(&ir_module, &main_schema, &return_schema, &declared_table)
            .expect("compile");
    let _ = return_layout;

    let drifted_host = HostFnTable {
        entries: vec![HostFnEntry {
            name: "echo".to_string(),
            params_canonical_hash: hash_params(&[IrType::I64]),
            ret_canonical_hash: hash_return(IrType::I64),
            cap_bit: NO_CAPABILITY,
        }],
    };
    match WasmModule::from_bytes_with_host_fns(wasm, &drifted_host) {
        Err(LoadError::HostFnSignatureDrift { name, which }) => {
            assert_eq!(name, "echo");
            assert!(which == "params" || which == "return");
        }
        other => panic!("expected HostFnSignatureDrift, got {other:?}"),
    }
}

#[test]
fn host_fns_section_present() {
    // Sanity: codegen emits the `relon.host_fns` section verbatim and
    // a wasmparser walk surfaces it under the expected name.
    let main_schema = string_in_schema("s");
    let return_schema = string_return_schema();
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let s_offset = offset_of(&main_layout, "s");
    let value_offset = offset_of(&return_layout, "value");
    let _ = return_layout;

    let ir_module = IrModule {
        imports: vec![NativeImport {
            name: "echo".to_string(),
            param_tys: vec![IrType::String],
            ret_ty: IrType::String,
            cap_bit: relon_ir::NO_CAPABILITY_BIT,
        }],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret: IrType::I32,
            range: synth_range(),
            body: vec![
                t(Op::LoadStringPtr { offset: s_offset }),
                t(Op::CallNative {
                    import_idx: 0,
                    param_tys: vec![IrType::String],
                    ret_ty: IrType::String,
                    cap_bit: relon_ir::NO_CAPABILITY_BIT,
                }),
                t(Op::StoreField {
                    offset: value_offset,
                    ty: IrType::String,
                }),
                t(Op::Return),
            ],
        }],
        entry_func_index: Some(0),
    };
    let host_fns = HostFnTable {
        entries: vec![HostFnEntry {
            name: "echo".to_string(),
            params_canonical_hash: hash_params(&[IrType::String]),
            ret_canonical_hash: hash_return(IrType::String),
            cap_bit: NO_CAPABILITY,
        }],
    };

    let wasm = compile_module_with_host_fns(&ir_module, &main_schema, &return_schema, &host_fns)
        .expect("compile");

    let mut saw_section = false;
    let mut entry_count_check_passed = false;
    for payload in Parser::new(0).parse_all(&wasm) {
        let payload = payload.expect("payload");
        if let Payload::CustomSection(reader) = payload {
            if reader.name() == SECTION_NAME {
                saw_section = true;
                let decoded = relon_codegen_wasm::host_fns::decode(reader.data()).expect("decode");
                assert_eq!(decoded, host_fns);
                entry_count_check_passed = decoded.entries.len() == 1;
            }
        }
    }
    assert!(saw_section, "relon.host_fns custom section must be emitted");
    assert!(entry_count_check_passed);
}

// ---------------------------------------------------------------------------
// Capability bit table — Phase 9.b-2 wires `Capabilities` to the wasm
// `relon_caps_avail` bitmap. The two tests below mirror the existing
// `capability_granted_allows_call` / `capability_missing_traps` pair
// but route the bitmap through `Capabilities::all_granted` /
// `Capabilities::default` so the host SDK ↔ codegen contract on
// `CapabilityBit::ReadsFs` is exercised end-to-end.
// ---------------------------------------------------------------------------

#[test]
fn capability_via_all_granted_bitmap_runs() {
    // `write_file` declares `cap_bit = CapabilityBit::ReadsFs (0)`.
    // Driving the imported `relon_caps_avail` global from
    // `Capabilities::all_granted` must light up the matching bit so
    // the codegen `check_cap` prologue passes through.
    use relon_eval_api::{Capabilities, CapabilityBit};

    let (ir_module, host_fns, main_schema, return_schema, main_layout, return_layout) =
        write_file_module();
    assert_eq!(
        ir_module.imports[0].cap_bit,
        CapabilityBit::ReadsFs.bit_index(),
        "fixture must guard write_file behind the ReadsFs bit so the \
         Capabilities mapping is exercised here"
    );

    let wasm = compile_module_with_host_fns(&ir_module, &main_schema, &return_schema, &host_fns)
        .expect("compile");

    let caps_bitmap = Capabilities::all_granted().to_cap_bitmap();
    assert_ne!(
        caps_bitmap & CapabilityBit::ReadsFs.mask(),
        0,
        "all_granted must publish the ReadsFs bit"
    );

    let (mut store, mut linker, engine, _caps) = setup_store_with_caps(caps_bitmap as i64);
    linker
        .func_wrap(
            "env",
            "write_file",
            |_caller: Caller<'_, ()>, _p: i32, _c: i32| -> i32 { 1 },
        )
        .expect("define write_file");
    let module = Module::new(&engine, &wasm).expect("module load");
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate");
    let memory: Memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    let run_main: TypedFunc<(i32, i32, i32, i32), i32> = instance
        .get_typed_func(&mut store, "run_main")
        .expect("run_main");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("path", "/tmp/x").expect("write path");
    builder
        .write_string("content", "hi")
        .expect("write content");
    let in_bytes = builder.finish();
    memory
        .write(&mut store, IN_PTR as usize, &in_bytes)
        .expect("memwrite");
    let bw = run_main
        .call(
            &mut store,
            (
                IN_PTR,
                in_bytes.len() as i32,
                OUT_PTR,
                return_layout.root_size as i32,
            ),
        )
        .expect("run_main with all_granted bitmap");
    assert_eq!(bw as usize, return_layout.root_size);
    let mut out = vec![0u8; return_layout.root_size];
    memory
        .read(&mut store, OUT_PTR as usize, &mut out)
        .expect("memread");
    let reader = BufferReader::new(&return_layout, &return_schema.fields, &out).expect("reader");
    assert!(reader.read_bool("value").expect("read value"));
}

#[test]
fn capability_via_default_bitmap_denied() {
    // Zero-trust default (`Capabilities::default`) publishes a zero
    // bitmap, so the codegen `check_cap` prologue traps before
    // write_file ever runs. translate_trap must recover
    // `WasmCapabilityDenied { cap_bit: 0 }` matching ReadsFs.
    use relon_eval_api::{Capabilities, CapabilityBit, RuntimeError};

    let (ir_module, host_fns, main_schema, return_schema, main_layout, _return_layout) =
        write_file_module();
    let wasm = compile_module_with_host_fns(&ir_module, &main_schema, &return_schema, &host_fns)
        .expect("compile");
    let parsed = WasmModule::from_bytes(wasm.clone()).expect("load");

    let caps_bitmap = Capabilities::default().to_cap_bitmap();
    assert_eq!(
        caps_bitmap, 0,
        "default Capabilities must publish a zero-trust bitmap"
    );

    let (mut store, mut linker, engine, _caps) = setup_store_with_caps(caps_bitmap as i64);
    let host_call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host_call_count_clone = host_call_count.clone();
    linker
        .func_wrap(
            "env",
            "write_file",
            move |_caller: Caller<'_, ()>, _p: i32, _c: i32| -> i32 {
                host_call_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                1
            },
        )
        .expect("define write_file");
    let module = Module::new(&engine, &wasm).expect("module load");
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate");
    let memory: Memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    let run_main: TypedFunc<(i32, i32, i32, i32), i32> = instance
        .get_typed_func(&mut store, "run_main")
        .expect("run_main");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("path", "/tmp/x").expect("write path");
    builder
        .write_string("content", "hi")
        .expect("write content");
    let in_bytes = builder.finish();
    memory
        .write(&mut store, IN_PTR as usize, &in_bytes)
        .expect("memwrite");
    let err = run_main
        .call(&mut store, (IN_PTR, in_bytes.len() as i32, OUT_PTR, 32))
        .expect_err("must trap when default Capabilities denies ReadsFs");

    let runtime_err = parsed.translate_trap(&err);
    match runtime_err {
        RuntimeError::WasmCapabilityDenied { cap_bit, range: _ } => {
            assert_eq!(
                cap_bit,
                CapabilityBit::ReadsFs.bit_index(),
                "expected ReadsFs bit, got {cap_bit}"
            );
        }
        other => panic!("expected WasmCapabilityDenied, got {other:?}"),
    }
    assert_eq!(
        host_call_count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "host fn must not be invoked when capability is denied"
    );
}
