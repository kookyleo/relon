//! Phase 4.c-1 control-flow + scratch-allocator integration tests.
//!
//! These tests exercise the wasm-encoder paths added in Phase 4.c-1:
//!
//! * `Op::Block` / `Op::Loop` / `Op::Br` / `Op::BrIf` — structured
//!   control flow on the wasm side, validated against a hand-built
//!   IR that computes `sum = 0; for i in 0..n { sum += i }` and
//!   returns `sum`.
//! * `Op::AllocScratch` — wasm-internal bump allocator. The test
//!   runs the alloc inside a counted loop to prove both the loop
//!   machinery and the cursor bump cooperate without tripping the
//!   bounds check.
//! * Trap path — `Op::AllocScratch { size_bytes }` with a request
//!   that overflows the 1-page memory ceiling traps with the
//!   `ScratchOOM` `UnreachableKind` and translates into
//!   [`RuntimeError::WasmScratchOOM`].
//!
//! The tests construct the IR directly (no parser / analyzer) so the
//! lowering pipeline is decoupled from the Phase 4.c-1 codegen path
//! — user-facing `for` / `while` lowering is deferred to a later
//! phase. Phase 4.c-2 stdlib bodies (`concat`, `upper`, `fold`, ...)
//! will use the same op shapes the tests pin here.

use relon_codegen_wasm::{compile_module, WasmModule};
use relon_eval_api::buffer::{BufferBuilder, BufferReader};
use relon_eval_api::layout::{OffsetTable, SchemaLayout};
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_eval_api::RuntimeError;
use relon_ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::TokenRange;
use wasmtime::{
    Engine, Global, Instance, Memory, Module, Mutability, Store, TypedFunc, Val, ValType,
};

const IN_PTR: i32 = 0;
const OUT_PTR: i32 = 256;

/// 1-based synthetic source range used by every hand-built IR op.
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

/// Canonical `#main(Int n) -> Int` schema pair, hand-rolled so the
/// test owns the layout without depending on the parser.
fn schemas_n_to_int() -> (Schema, Schema) {
    let main_schema = Schema {
        name: "MainParams".into(),
        generics: vec![],
        fields: vec![Field {
            name: "n".into(),
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
    (main_schema, return_schema)
}

/// Look up a field's byte offset inside a schema layout.
fn field_offset(layout: &OffsetTable, name: &str) -> u32 {
    layout
        .fields
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.offset as u32)
        .unwrap_or_else(|| panic!("missing field `{name}`"))
}

struct WasmSession {
    store: Store<()>,
    memory: Memory,
    run_main: TypedFunc<(i32, i32, i32, i32), i32>,
}

/// Spin up a wasmtime session for `wasm` with all capabilities granted.
fn build_session(wasm: &[u8]) -> WasmSession {
    let engine = Engine::default();
    let module = Module::new(&engine, wasm).expect("module load");
    let mut store: Store<()> = Store::new(&engine, ());
    let caps_avail = Global::new(
        &mut store,
        wasmtime::GlobalType::new(ValType::I64, Mutability::Const),
        Val::I64(i64::MAX),
    )
    .expect("caps_avail global");
    let instance = Instance::new(&mut store, &module, &[caps_avail.into()]).expect("instantiate");
    let memory = instance
        .get_memory(&mut store, "memory")
        .expect("memory export");
    let run_main = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "run_main")
        .expect("run_main typed view");
    WasmSession {
        store,
        memory,
        run_main,
    }
}

impl WasmSession {
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
}

// ---------------------------------------------------------------------------
// 1. Block + Loop + BrIf form a counting loop.
// ---------------------------------------------------------------------------

/// Hand-build the IR equivalent of:
///
/// ```text
/// let mut sum: i64 = 0;
/// let mut i: i64 = 0;
/// while i < n {
///     sum += i;
///     i += 1;
/// }
/// return sum;
/// ```
fn loop_sum_module() -> (Vec<u8>, Schema, Schema, OffsetTable, OffsetTable) {
    let (main_schema, return_schema) = schemas_n_to_int();
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let n_offset = field_offset(&main_layout, "n");
    let value_offset = field_offset(&return_layout, "value");

    // Two let-locals: sum (idx 0), i (idx 1). Both I64.
    const SUM: u32 = 0;
    const I: u32 = 1;

    let body = vec![
        // sum = 0
        t(Op::ConstI64(0)),
        t(Op::LetSet {
            idx: SUM,
            ty: IrType::I64,
        }),
        // i = 0
        t(Op::ConstI64(0)),
        t(Op::LetSet {
            idx: I,
            ty: IrType::I64,
        }),
        // block { loop { ... br 1 to exit; br 0 to continue } }
        t(Op::Block {
            result_ty: None,
            body: vec![t(Op::Loop {
                result_ty: None,
                body: vec![
                    // if i >= n { br 1 (the outer block) }
                    t(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::LoadField {
                        offset: n_offset,
                        ty: IrType::I64,
                    }),
                    t(Op::Ge(IrType::I64)),
                    t(Op::BrIf { label_depth: 1 }),
                    // sum = sum + i
                    t(Op::LetGet {
                        idx: SUM,
                        ty: IrType::I64,
                    }),
                    t(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: SUM,
                        ty: IrType::I64,
                    }),
                    // i = i + 1
                    t(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::ConstI64(1)),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    // br 0 — continue the inner loop.
                    t(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        // Store sum into the return slot.
        t(Op::LetGet {
            idx: SUM,
            ty: IrType::I64,
        }),
        t(Op::StoreField {
            offset: value_offset,
            ty: IrType::I64,
        }),
        t(Op::Return),
    ];

    let ir = IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".into(),
            params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret: IrType::I32,
            range: synth_range(),
            body,
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    };

    let bytes = compile_module(&ir, &main_schema, &return_schema).expect("compile");
    (
        bytes,
        main_schema,
        return_schema,
        main_layout,
        return_layout,
    )
}

#[test]
fn block_loop_brif_counting_sum() {
    let (wasm, main_schema, return_schema, main_layout, return_layout) = loop_sum_module();

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("n", 10).expect("write n");
    let in_bytes = builder.finish();

    let mut session = build_session(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bytes_written = session
        .run_main
        .call(
            &mut session.store,
            (
                IN_PTR,
                in_bytes.len() as i32,
                OUT_PTR,
                return_layout.root_size as i32,
            ),
        )
        .expect("run_main call");
    assert_eq!(bytes_written as usize, return_layout.root_size);
    let out = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, &out).expect("BufferReader");
    // 0+1+2+...+9 = 45.
    assert_eq!(reader.read_int("value").expect("read value"), 45);
}

#[test]
fn block_loop_brif_zero_iterations() {
    // n = 0 → the BrIf at the top of the loop fires on the first
    // pass, so the sum stays at zero.
    let (wasm, main_schema, return_schema, main_layout, return_layout) = loop_sum_module();

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("n", 0).expect("write n");
    let in_bytes = builder.finish();

    let mut session = build_session(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    session
        .run_main
        .call(
            &mut session.store,
            (
                IN_PTR,
                in_bytes.len() as i32,
                OUT_PTR,
                return_layout.root_size as i32,
            ),
        )
        .expect("run_main call");
    let out = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, &out).expect("BufferReader");
    assert_eq!(reader.read_int("value").expect("read value"), 0);
}

// ---------------------------------------------------------------------------
// 2. AllocScratch + Loop: alloc 8 i64-sized scratch slots inside a
//    counted loop, return the iteration count to prove both the
//    cursor bump and the loop machinery completed without tripping
//    the bounds check.
// ---------------------------------------------------------------------------

/// IR: alloc 8 × 8-byte scratch slots inside a `Block { Loop { ... } }`
/// driven by an i64 counter; return the final iteration count.
///
/// The 64 total scratch bytes fit comfortably inside the 1-page (64
/// KiB) memory the codegen emits, so the bounds check at every bump
/// passes. The test asserts the loop counter equals `8` at exit —
/// proving both that the loop ran 8 times *and* that no bump tripped
/// a `ScratchOOM` trap before exit.
fn scratch_loop_module() -> (Vec<u8>, Schema, Schema, OffsetTable, OffsetTable) {
    let (main_schema, return_schema) = schemas_n_to_int();
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let value_offset = field_offset(&return_layout, "value");

    const I: u32 = 0;
    // Discarded i32 spill for the scratch base — we don't read the
    // address back in this test (Phase 4.c-2 stdlib bodies will).
    const BASE: u32 = 1;

    let body = vec![
        // i = 0
        t(Op::ConstI64(0)),
        t(Op::LetSet {
            idx: I,
            ty: IrType::I64,
        }),
        // block { loop { ... } } — same shape as `loop_sum_module`.
        t(Op::Block {
            result_ty: None,
            body: vec![t(Op::Loop {
                result_ty: None,
                body: vec![
                    // exit when i >= 8
                    t(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::ConstI64(8)),
                    t(Op::Ge(IrType::I64)),
                    t(Op::BrIf { label_depth: 1 }),
                    // alloc 8 bytes and stash the base.
                    t(Op::AllocScratch { size_bytes: 8 }),
                    t(Op::LetSet {
                        idx: BASE,
                        ty: IrType::I32,
                    }),
                    // i += 1
                    t(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::ConstI64(1)),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    t(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        // Return the final iteration count.
        t(Op::LetGet {
            idx: I,
            ty: IrType::I64,
        }),
        t(Op::StoreField {
            offset: value_offset,
            ty: IrType::I64,
        }),
        t(Op::Return),
    ];

    let ir = IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".into(),
            params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret: IrType::I32,
            range: synth_range(),
            body,
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    };

    let bytes = compile_module(&ir, &main_schema, &return_schema).expect("compile");
    (
        bytes,
        main_schema,
        return_schema,
        main_layout,
        return_layout,
    )
}

#[test]
fn scratch_alloc_inside_loop_completes_eight_iterations() {
    let (wasm, main_schema, return_schema, main_layout, return_layout) = scratch_loop_module();

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("n", 0).expect("write n");
    let in_bytes = builder.finish();

    let mut session = build_session(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bytes_written = session
        .run_main
        .call(
            &mut session.store,
            (
                IN_PTR,
                in_bytes.len() as i32,
                OUT_PTR,
                return_layout.root_size as i32,
            ),
        )
        .expect("run_main call");
    assert_eq!(bytes_written as usize, return_layout.root_size);
    let out = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, &out).expect("BufferReader");
    assert_eq!(reader.read_int("value").expect("read value"), 8);
}

// ---------------------------------------------------------------------------
// 3. AllocScratch traps with `ScratchOOM` when the requested size
//    overflows the 1-page memory ceiling.
// ---------------------------------------------------------------------------

#[test]
fn alloc_scratch_oom_traps_with_scratch_oom_kind() {
    // Request a single allocation of 128 KiB — strictly larger than
    // the codegen's 1-page memory size (64 KiB). The bounds check at
    // the bump site fires before the cursor moves.
    let (main_schema, return_schema) = schemas_n_to_int();
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let value_offset = field_offset(&return_layout, "value");

    let body = vec![
        // Drop the returned base into a let so the vstack discipline
        // stays clean past the trap site (codegen does not skip emit
        // when the immediate looks dangerous).
        t(Op::AllocScratch {
            size_bytes: 128 * 1024,
        }),
        t(Op::LetSet {
            idx: 0,
            ty: IrType::I32,
        }),
        // Unreachable — the alloc above traps before we get here —
        // but the body must still type-check at codegen emit time.
        t(Op::ConstI64(0)),
        t(Op::StoreField {
            offset: value_offset,
            ty: IrType::I64,
        }),
        t(Op::Return),
    ];

    let ir = IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".into(),
            params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret: IrType::I32,
            range: synth_range(),
            body,
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    };
    let wasm = compile_module(&ir, &main_schema, &return_schema).expect("compile");
    let module = WasmModule::from_bytes(wasm.clone()).expect("load");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("n", 0).expect("write n");
    let in_bytes = builder.finish();

    let mut session = build_session(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let err = session
        .run_main
        .call(
            &mut session.store,
            (
                IN_PTR,
                in_bytes.len() as i32,
                OUT_PTR,
                return_layout.root_size as i32,
            ),
        )
        .expect_err("must trap");

    let runtime_err = module.translate_trap(&err);
    match runtime_err {
        RuntimeError::WasmScratchOOM { needed, .. } => {
            assert_eq!(needed, 128 * 1024, "needed must mirror the alloc request");
        }
        other => panic!("expected WasmScratchOOM, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 4. AllocScratchDyn: dynamic-size variant. We feed the bump with
//    `in_len` (the entry's i32 handshake slot), which is small and
//    safely within the page budget. The test asserts the call
//    succeeds without trapping; Phase 4.c-2 stdlib bodies will pin
//    the contents-readback shape this op enables.
// ---------------------------------------------------------------------------

#[test]
fn alloc_scratch_dyn_with_in_len_succeeds() {
    let (main_schema, return_schema) = schemas_n_to_int();
    let main_layout = SchemaLayout::offsets_for(&main_schema).expect("main layout");
    let return_layout = SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let value_offset = field_offset(&return_layout, "value");

    // WASM_LOCAL_IN_LEN is wasm-local 1 (the four handshake params
    // are pinned at indices 0..=3 by the lowering pass; the test
    // mirrors the same convention).
    const WASM_LOCAL_IN_LEN: u32 = 1;

    let body = vec![
        // Push in_len (i32) and use it as the dynamic alloc size.
        t(Op::LocalGet(WASM_LOCAL_IN_LEN)),
        t(Op::AllocScratchDyn),
        // Stash the returned base in a let so the vstack stays clean.
        t(Op::LetSet {
            idx: 0,
            ty: IrType::I32,
        }),
        // Return the constant 1 so the caller can confirm the call
        // landed past the alloc without trapping.
        t(Op::ConstI64(1)),
        t(Op::StoreField {
            offset: value_offset,
            ty: IrType::I64,
        }),
        t(Op::Return),
    ];

    let ir = IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".into(),
            params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret: IrType::I32,
            range: synth_range(),
            body,
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    };
    let wasm = compile_module(&ir, &main_schema, &return_schema).expect("compile");

    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    builder.write_int("n", 42).expect("write n");
    let in_bytes = builder.finish();

    let mut session = build_session(&wasm);
    session.write(IN_PTR as usize, &in_bytes);
    let bytes_written = session
        .run_main
        .call(
            &mut session.store,
            (
                IN_PTR,
                in_bytes.len() as i32,
                OUT_PTR,
                return_layout.root_size as i32,
            ),
        )
        .expect("run_main call");
    assert_eq!(bytes_written as usize, return_layout.root_size);
    let out = session.read(OUT_PTR as usize, return_layout.root_size);
    let reader =
        BufferReader::new(&return_layout, &return_schema.fields, &out).expect("BufferReader");
    assert_eq!(reader.read_int("value").expect("read value"), 1);
}
