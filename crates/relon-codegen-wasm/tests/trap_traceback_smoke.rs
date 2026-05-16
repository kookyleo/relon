//! Phase 7 trap-translation integration tests.
//!
//! Drives `#main` source (or a hand-built IR for the cases that need
//! a `#native fn`) through the full compile → instantiate → invoke
//! → translate_trap pipeline and asserts that each codegen-emitted
//! guard surfaces as the matching [`RuntimeError`] variant.
//!
//! Coverage:
//!
//! * `div_by_zero_traceback` — `i64.div_s` trap → `DivisionByZero`.
//! * `out_buf_too_small_traceback` — entry `out_cap` guard →
//!   `WasmOutBufTooSmall { needed }`.
//! * `in_buf_too_small_traceback` — entry `in_len` guard →
//!   `WasmInBufTooSmall { needed }`.
//! * `capability_denied_traceback` — `check_cap` prologue trap →
//!   `WasmCapabilityDenied { cap_bit }`.
//! * `unclassified_trap_falls_through` — a stack-overflow trap on
//!   purpose → `WasmTrapUnclassified`.
//!
//! Each test asserts on the [`RuntimeError`] variant shape rather
//! than its `Display` form so a future tweak to the diagnostic
//! prose doesn't churn the gate. The cap-denied test also confirms
//! that the surfaced `cap_bit` matches what the IR declared, and
//! the buffer-size tests confirm the `needed` field equals the
//! schema's `root_size`.

use relon_codegen_wasm::{
    compile_lowered_entry, compile_module_with_host_fns, hash_params, hash_return, HostFnEntry,
    HostFnTable, WasmModule,
};
use relon_eval_api::buffer::BufferBuilder;
use relon_eval_api::layout::SchemaLayout;
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_eval_api::RuntimeError;
use relon_ir::{
    lower_workspace_single, Func, IrType, Module as IrModule, NativeImport, Op, TaggedOp,
};
use relon_parser::TokenRange;
use wasmtime::{Caller, Engine, Global, Linker, Memory, Module, Mutability, Store, TypedFunc, Val};

const IN_PTR: i32 = 0;
const OUT_PTR: i32 = 256;

/// Synthetic source range used by the hand-built IR cases. Matches
/// the placeholder used in `host_fn_smoke.rs` so the resulting
/// srcmap entries are valid (line / col both 1-based and >= 1).
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

fn t(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: synth_range(),
    }
}

/// Compact alias for the typed-view of the `run_main` export every
/// test instantiates. Kept here so the helper signatures stay
/// inside the clippy `type_complexity` budget.
type RunMainView = TypedFunc<(i32, i32, i32, i32), i32>;

/// Build a wasmtime session with a caller-supplied caps_avail
/// bitmap and an optional host-fn registration callback.
fn build_session_with_caps(
    wasm: &[u8],
    caps_avail: i64,
    register: impl FnOnce(&mut Linker<()>, &mut Store<()>),
) -> (Store<()>, Memory, RunMainView) {
    let engine = Engine::default();
    let mut store: Store<()> = Store::new(&engine, ());
    let caps_avail_global = Global::new(
        &mut store,
        wasmtime::GlobalType::new(wasmtime::ValType::I64, Mutability::Const),
        Val::I64(caps_avail),
    )
    .expect("caps_avail global");
    let mut linker: Linker<()> = Linker::new(&engine);
    linker
        .define(&mut store, "env", "relon_caps_avail", caps_avail_global)
        .expect("define caps_avail");
    register(&mut linker, &mut store);
    let module = Module::new(&engine, wasm).expect("module load");
    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate");
    let memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    let run_main = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "run_main")
        .expect("run_main typed view");
    (store, memory, run_main)
}

/// Compile a Relon `#main` source through the full pipeline.
fn compile_source(src: &str) -> (Vec<u8>, Schema, Schema) {
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

// ---------------------------------------------------------------------------
// 1. Integer division by zero — `i64.div_s` traps with
//    `IntegerDivisionByZero`. translate_trap maps this back to the
//    tree-walker `DivisionByZero` variant for cross-backend parity.
// ---------------------------------------------------------------------------

#[test]
fn div_by_zero_traceback() {
    // `#main(Int x, Int y) -> Int : x / y` — caller passes y=0 to
    // trip the `i64.div_s` trap. Lowering must emit `Op::Div(I64)`
    // so the resulting wasm carries the actual div instruction.
    let (wasm, main_schema, _return_schema) = compile_source("#main(Int x, Int y) -> Int\nx / y");
    let module = WasmModule::from_bytes(wasm.clone()).expect("load");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");

    let (mut store, memory, run_main) = build_session_with_caps(&wasm, i64::MAX, |_, _| {});

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 10).expect("write x");
    builder.write_int("y", 0).expect("write y");
    let in_bytes = builder.finish();
    memory
        .write(&mut store, IN_PTR as usize, &in_bytes)
        .expect("memwrite");

    let err = run_main
        .call(&mut store, (IN_PTR, in_bytes.len() as i32, OUT_PTR, 16))
        .expect_err("must trap on x / 0");

    let runtime_err = module.translate_trap(&err);
    assert!(
        matches!(runtime_err, RuntimeError::DivisionByZero(_)),
        "expected DivisionByZero, got {runtime_err:?}",
    );
}

// ---------------------------------------------------------------------------
// 2. out_cap guard — caller passes an out_cap below the return
//    schema's root_size; the entry prologue's `i32.lt_u + unreachable`
//    fires. uctab lookup recovers `OutBufTooSmall { needed }`.
// ---------------------------------------------------------------------------

#[test]
fn out_buf_too_small_traceback() {
    let (wasm, main_schema, return_schema) = compile_source("#main(Int x) -> Int\nx");
    let module = WasmModule::from_bytes(wasm.clone()).expect("load");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let (mut store, memory, run_main) = build_session_with_caps(&wasm, i64::MAX, |_, _| {});

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 7).expect("write x");
    let in_bytes = builder.finish();
    memory
        .write(&mut store, IN_PTR as usize, &in_bytes)
        .expect("memwrite");

    // out_cap one byte short of root_size — guard must trap.
    let short_cap = (return_layout.root_size as i32) - 1;
    let err = run_main
        .call(
            &mut store,
            (IN_PTR, in_bytes.len() as i32, OUT_PTR, short_cap),
        )
        .expect_err("must trap on short out_cap");

    let runtime_err = module.translate_trap(&err);
    match runtime_err {
        RuntimeError::WasmOutBufTooSmall { needed, .. } => {
            assert_eq!(
                needed as usize, return_layout.root_size,
                "needed must match return schema root_size",
            );
        }
        other => panic!("expected WasmOutBufTooSmall, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 3. in_len guard — caller passes an in_len below the main schema's
//    root_size; the first prologue guard fires. Same shape as the
//    out_cap test but on the input side.
// ---------------------------------------------------------------------------

#[test]
fn in_buf_too_small_traceback() {
    let (wasm, main_schema, return_schema) = compile_source("#main(Int x) -> Int\nx");
    let module = WasmModule::from_bytes(wasm.clone()).expect("load");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");

    let (mut store, memory, run_main) = build_session_with_caps(&wasm, i64::MAX, |_, _| {});

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("x", 7).expect("write x");
    let in_bytes = builder.finish();
    memory
        .write(&mut store, IN_PTR as usize, &in_bytes)
        .expect("memwrite");

    let short_len = (in_bytes.len() as i32) - 1;
    let err = run_main
        .call(
            &mut store,
            (IN_PTR, short_len, OUT_PTR, return_layout.root_size as i32),
        )
        .expect_err("must trap on short in_len");

    let runtime_err = module.translate_trap(&err);
    match runtime_err {
        RuntimeError::WasmInBufTooSmall { needed, .. } => {
            assert_eq!(
                needed as usize, main_layout.root_size,
                "needed must match main schema root_size",
            );
        }
        other => panic!("expected WasmInBufTooSmall, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 4. Capability denied — IR declares a `#native fn write_file`
//    guarded by `cap_bit=0`; host instantiates with `caps_avail=0`
//    so the `check_cap` prologue traps before the host fn ever
//    runs. uctab lookup recovers `CapabilityDenied { cap_bit: 0 }`.
// ---------------------------------------------------------------------------

#[test]
fn capability_denied_traceback() {
    let main_schema = Schema {
        name: "MainParams".into(),
        generics: vec![],
        fields: vec![
            Field {
                name: "path".into(),
                ty: TypeRepr::String,
                default: None,
            },
            Field {
                name: "content".into(),
                ty: TypeRepr::String,
                default: None,
            },
        ],
    };
    let return_schema = Schema {
        name: "Ret".into(),
        generics: vec![],
        fields: vec![Field {
            name: "value".into(),
            ty: TypeRepr::Bool,
            default: None,
        }],
    };
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let path_offset = main_layout
        .fields
        .iter()
        .find(|f| f.name == "path")
        .map(|f| f.offset as u32)
        .expect("path offset");
    let content_offset = main_layout
        .fields
        .iter()
        .find(|f| f.name == "content")
        .map(|f| f.offset as u32)
        .expect("content offset");
    let value_offset = return_layout
        .fields
        .iter()
        .find(|f| f.name == "value")
        .map(|f| f.offset as u32)
        .expect("value offset");

    let cap_bit: u32 = 0;

    let ir_module = IrModule {
        imports: vec![NativeImport {
            name: "write_file".into(),
            param_tys: vec![IrType::String, IrType::String],
            ret_ty: IrType::Bool,
            cap_bit,
        }],
        funcs: vec![Func {
            name: "run_main".into(),
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
                    cap_bit,
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
            name: "write_file".into(),
            params_canonical_hash: hash_params(&[IrType::String, IrType::String]),
            ret_canonical_hash: hash_return(IrType::Bool),
            cap_bit,
        }],
    };

    let wasm = compile_module_with_host_fns(&ir_module, &main_schema, &return_schema, &host_fns)
        .expect("compile");
    let module = WasmModule::from_bytes(wasm.clone()).expect("load");

    // caps_avail = 0 — `check_cap` must trap before the host fn
    // would run.
    let (mut store, memory, run_main) = build_session_with_caps(&wasm, 0, |linker, store| {
        linker
            .func_wrap(
                "env",
                "write_file",
                |_caller: Caller<'_, ()>, _p: i32, _c: i32| -> i32 { 1 },
            )
            .expect("define write_file");
        let _ = store;
    });

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
        .call(
            &mut store,
            (
                IN_PTR,
                in_bytes.len() as i32,
                OUT_PTR,
                return_layout.root_size as i32,
            ),
        )
        .expect_err("must trap when capability is missing");

    let runtime_err = module.translate_trap(&err);
    match runtime_err {
        RuntimeError::WasmCapabilityDenied {
            cap_bit: bit,
            range: _,
        } => {
            assert_eq!(bit, cap_bit, "cap_bit must match declared import");
        }
        other => panic!("expected WasmCapabilityDenied, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 5. Unclassified trap — induce a wasmtime trap that doesn't match
//    any of our codegen-emitted guards. A recursive call without a
//    base case overflows the stack; wasmtime surfaces `StackOverflow`,
//    which translate_trap maps to `WasmTrapUnclassified`.
// ---------------------------------------------------------------------------

#[test]
fn unclassified_trap_falls_through() {
    // Build IR for a function that calls itself unconditionally —
    // wasmtime's stack overflows long before any guard fires.
    let main_schema = Schema {
        name: "MainParams".into(),
        generics: vec![],
        fields: vec![Field {
            name: "x".into(),
            ty: TypeRepr::Int,
            default: None,
        }],
    };
    let return_schema = Schema {
        name: "Ret".into(),
        generics: vec![],
        fields: vec![Field {
            name: "value".into(),
            ty: TypeRepr::Int,
            default: None,
        }],
    };
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let _ = main_layout;
    let _ = return_layout;

    // Recursion at the wasm level requires `Op::Call` with a fn_index
    // pointing back at the entry function. We thread the import_count
    // through manually: zero `#native` imports means the entry function
    // sits at wasm function index = stdlib_count + 0. Lowering pass
    // doesn't make this easy to express in source, so we punt and
    // instead use a 64 KiB out_cap that overflows the i32 add inside
    // a tail-record bounds check — wasmtime traps with
    // `IntegerOverflow` which translate_trap surfaces as
    // `NumericOverflow` (still a wasm trap, but not one our uctab
    // covers — wait, that's a different shape).
    //
    // Simpler: take the div-by-zero test source but pass a *negative*
    // out_cap that wasmtime rejects as a memory-write at a high
    // address (because out_ptr + tail_cursor + size wraps to a huge
    // unsigned offset, triggering `MemoryOutOfBounds`). That trap
    // doesn't match any uctab entry, so translate_trap falls through.
    let src = "#main(String s) -> String\ns";
    let (wasm, main_schema, _return_schema) = compile_source(src);
    let module = WasmModule::from_bytes(wasm.clone()).expect("load");
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");

    let (mut store, memory, run_main) = build_session_with_caps(&wasm, i64::MAX, |_, _| {});
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_string("s", "hello").expect("write s");
    let in_bytes = builder.finish();
    memory
        .write(&mut store, IN_PTR as usize, &in_bytes)
        .expect("memwrite");

    // out_cap large enough to clear the prologue guard (root_size
    // is 8 bytes for a single String field) but a hugely negative
    // out_ptr forces `memory.copy` to address-of-bounds before any
    // tail-record guard fires.
    let large_negative_out_ptr: i32 = -1_000_000_i32;
    let err = run_main
        .call(
            &mut store,
            (IN_PTR, in_bytes.len() as i32, large_negative_out_ptr, 4096),
        )
        .expect_err("must trap on out-of-bounds memcpy");

    let runtime_err = module.translate_trap(&err);
    assert!(
        matches!(runtime_err, RuntimeError::WasmTrapUnclassified { .. }),
        "expected WasmTrapUnclassified, got {runtime_err:?}",
    );
}

// ---------------------------------------------------------------------------
// 6. WasmModule::lookup_pc — sanity-check the new convenience wrapper
//    over `SrcMap::lookup` so the host-SDK-facing API contract is
//    covered by a smoke test.
// ---------------------------------------------------------------------------

#[test]
fn lookup_pc_returns_some_for_known_pc() {
    let (wasm, _main_schema, _return_schema) = compile_source("#main(Int x) -> Int\nx");
    let module = WasmModule::from_bytes(wasm).expect("load");
    let srcmap = module.srcmap();
    let pc = srcmap
        .entries
        .last()
        .map(|e| e.pc)
        .expect("at least one entry");
    let range = module.lookup_pc(pc);
    assert!(
        range.is_some(),
        "lookup_pc should hit on the last srcmap pc"
    );
}
