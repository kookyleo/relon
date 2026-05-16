#![forbid(unsafe_code)]

//! Lower `relon-ir` to WebAssembly bytecode + runtime adapter (Phase 2.b+).
//!
//! Implements the four locked design decisions:
//!   1. Binary memory handshake for `#main` params + return
//!      (see `wasm-binary-layout-v1-2026-05-16.md`)
//!   2. Stdlib self-contained (bundled bytecode + check_cap opcode)
//!   3. Source map + ABI metadata in custom sections
//!      (see `wasm-srcmap-section-v1-2026-05-16.md`)
//!   4. Static topological eager evaluation for dict fields
//!
//! Phase 2.b flips the entry function signature from the v1.beta
//! scalar form (`(i64) -> i64`) to the real handshake form:
//!
//! ```text
//! (memory 1)                                  ;; 1 page (64 KiB)
//! (export "memory" (memory 0))
//! (export "run_main" (func 0))
//! (func (param i32 i32 i32 i32) (result i32)
//!     ;; in_ptr, in_len, out_ptr, out_cap → bytes_written
//!     ;; guard: in_len < main_root_size  → unreachable
//!     ;; guard: out_cap < return_root_size → unreachable
//!     <body>                                  ;; LoadField / StoreField
//!     i32.const <return_root_size>            ;; bytes_written
//! )
//! ```
//!
//! The `relon.abi` section now carries the real sha256 of the
//! canonical `#main` schema (params + return), so host SDKs can
//! reject schema drift at load time.

pub mod abi;
pub mod error;
pub mod srcmap;

pub use abi::{AbiError, AbiMetadata};
pub use error::{CodegenError, LoadError};
pub use srcmap::{Entry as SrcMapEntry, SrcMap, SrcMapError};

use relon_eval_api::layout::{OffsetTable, SchemaLayout};
use relon_eval_api::schema_canonical::{schema_hash, Schema};
use relon_ir::{
    IrType, LoweredEntry, Module as IrModule, Op, WASM_LOCAL_IN_LEN, WASM_LOCAL_IN_PTR,
    WASM_LOCAL_OUT_CAP, WASM_LOCAL_OUT_PTR,
};
use relon_parser::TokenRange;
use wasm_encoder::{
    BlockType, CodeSection, CustomSection, ExportKind, ExportSection, Function, FunctionSection,
    Ieee64, Instruction, MemArg, MemorySection, MemoryType, Module, TypeSection, ValType,
};

/// Memory export name used by the binary handshake.
const MEMORY_EXPORT_NAME: &str = "memory";

/// Default initial memory size in wasm pages (64 KiB each). Phase 2.b
/// only needs a small staging area for the in/out buffers — the host
/// places its pointers at whatever offsets it pleases.
const DEFAULT_MEMORY_PAGES: u64 = 1;

/// Lower a [`LoweredEntry`] to a wasm binary.
///
/// `entry.main_schema` and `entry.return_schema` are the canonical
/// shapes the IR was lowered against; codegen recomputes their layout
/// here (cheap, deterministic) so it can:
///
/// * size the entry-function's in_len / out_cap guards,
/// * pick the right wasm store opcode for the trailing `bytes_written`
///   return, and
/// * compute the sha256 hashes the `relon.abi` section embeds for
///   schema-drift detection at load time.
pub fn compile_module(
    ir: &IrModule,
    main_schema: &Schema,
    return_schema: &Schema,
) -> Result<Vec<u8>, CodegenError> {
    if ir.funcs.is_empty() {
        return Err(CodegenError::EmptyModule);
    }

    let main_layout =
        SchemaLayout::offsets_for(main_schema).map_err(|e| CodegenError::Layout(e.to_string()))?;
    let return_layout = SchemaLayout::offsets_for(return_schema)
        .map_err(|e| CodegenError::Layout(e.to_string()))?;

    let mut module = Module::new();
    let mut types = TypeSection::new();
    let mut functions = FunctionSection::new();
    let mut memories = MemorySection::new();
    let mut exports = ExportSection::new();
    let mut codes = CodeSection::new();

    // Single shared memory: one page (64 KiB). Host writes the input
    // buffer somewhere inside, hands the pointer to `run_main`, then
    // reads `out_buf` back out from the same memory.
    memories.memory(MemoryType {
        minimum: DEFAULT_MEMORY_PAGES,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    exports.export(MEMORY_EXPORT_NAME, ExportKind::Memory, 0);

    let mut type_table: Vec<(Vec<ValType>, ValType, u32)> = Vec::new();
    let mut per_func_ranges: Vec<(TokenRange, Vec<TokenRange>)> =
        Vec::with_capacity(ir.funcs.len());

    for (func_index, func) in ir.funcs.iter().enumerate() {
        let params_vt: Vec<ValType> = func.params.iter().map(ir_to_val_type).collect();
        let ret_vt = ir_to_val_type(&func.ret);
        let type_index = match type_table
            .iter()
            .find(|(p, r, _)| p == &params_vt && *r == ret_vt)
        {
            Some(&(_, _, idx)) => idx,
            None => {
                let idx = type_table.len() as u32;
                types.ty().function(params_vt.clone(), vec![ret_vt]);
                type_table.push((params_vt, ret_vt, idx));
                idx
            }
        };
        functions.function(type_index);

        if Some(func_index) == ir.entry_func_index {
            exports.export(func.name.as_str(), ExportKind::Func, func_index as u32);
        }

        let (body, ranges) = emit_function_body(func, &main_layout, &return_layout)?;
        codes.function(&body);
        per_func_ranges.push((func.range, ranges));
    }

    module.section(&types);
    module.section(&functions);
    module.section(&memories);
    module.section(&exports);
    module.section(&codes);

    let bytes_so_far = module.as_slice().to_vec();
    let srcmap = build_srcmap(&bytes_so_far, &per_func_ranges)?;
    let srcmap_bytes = srcmap::encode_to_bytes(&srcmap);
    module.section(&CustomSection {
        name: srcmap::SECTION_NAME.into(),
        data: (&srcmap_bytes[..]).into(),
    });

    let abi = AbiMetadata {
        abi_version: abi::CURRENT_ABI_VERSION,
        codegen_version: abi::CURRENT_CODEGEN_VERSION,
        main_schema_hash: schema_hash(main_schema),
        return_schema_hash: schema_hash(return_schema),
        flags: 0,
    };
    let abi_bytes = abi::encode(&abi);
    module.section(&CustomSection {
        name: abi::SECTION_NAME.into(),
        data: (&abi_bytes[..]).into(),
    });

    Ok(module.finish())
}

/// Convenience wrapper around [`compile_module`] for callers that
/// already hold a [`LoweredEntry`]. Mirrors the v1.beta call site
/// shape where the caller would hand the IR straight in; Phase 2.b
/// just plumbs the canonical schemas through.
pub fn compile_lowered_entry(entry: &LoweredEntry) -> Result<Vec<u8>, CodegenError> {
    compile_module(&entry.module, &entry.main_schema, &entry.return_schema)
}

fn build_srcmap(
    module_bytes: &[u8],
    per_func: &[(TokenRange, Vec<TokenRange>)],
) -> Result<SrcMap, CodegenError> {
    let mut entries: Vec<SrcMapEntry> = Vec::new();
    let mut func_iter = per_func.iter();

    for payload in wasmparser::Parser::new(0).parse_all(module_bytes) {
        let payload =
            payload.map_err(|e| CodegenError::SrcMapEncode(format!("wasmparser error: {e}")))?;
        if let wasmparser::Payload::CodeSectionEntry(body) = payload {
            let (func_range, op_ranges) = func_iter.next().ok_or_else(|| {
                CodegenError::SrcMapEncode("more wasm function bodies than IR funcs".into())
            })?;
            let body_start = body.range().start as u32;
            entries.push(token_range_to_entry(body_start, *func_range));

            let ops_reader = body
                .get_operators_reader()
                .map_err(|e| CodegenError::SrcMapEncode(format!("operators reader: {e}")))?;
            let mut op_idx = 0usize;
            for item in ops_reader.into_iter_with_offsets() {
                let (_op, offset) =
                    item.map_err(|e| CodegenError::SrcMapEncode(format!("op decode: {e}")))?;
                let range = op_ranges.get(op_idx).copied().ok_or_else(|| {
                    CodegenError::SrcMapEncode(format!(
                        "wasm body has more operators ({}) than IR op-ranges ({}) recorded",
                        op_idx + 1,
                        op_ranges.len()
                    ))
                })?;
                entries.push(token_range_to_entry(offset as u32, range));
                op_idx += 1;
            }
            if op_idx != op_ranges.len() {
                return Err(CodegenError::SrcMapEncode(format!(
                    "wasm body produced {} operators but IR recorded {} ranges",
                    op_idx,
                    op_ranges.len()
                )));
            }
        }
    }

    if func_iter.next().is_some() {
        return Err(CodegenError::SrcMapEncode(
            "fewer wasm function bodies than IR funcs".into(),
        ));
    }

    entries.sort_by_key(|e| e.pc);

    Ok(SrcMap {
        files: vec![SRCMAP_PLACEHOLDER_FILE.to_string()],
        entries,
    })
}

const SRCMAP_PLACEHOLDER_FILE: &str = "<entry>";

fn token_range_to_entry(pc: u32, range: TokenRange) -> SrcMapEntry {
    let line = range.start.line;
    let col = range.start.column as u32;
    let range_len = range
        .end
        .offset
        .saturating_sub(range.start.offset)
        .min(u32::MAX as usize) as u32;
    SrcMapEntry {
        pc,
        file_idx: 0,
        line,
        col,
        range_len,
    }
}

/// Translate an IR function body into a `wasm_encoder::Function`,
/// emitting one wasm instruction per `Op` plus the binary-handshake
/// prologue / epilogue.
///
/// Prologue:
///   `in_len < main_root_size` → unreachable
///   `out_cap < return_root_size` → unreachable
///
/// Each user-facing op:
///   * `LoadField { offset, ty }` → `local.get $in_ptr; <load>.offset=N`
///   * `StoreField { offset, ty }` → `local.get $out_ptr; <swap-friendly emit>`
///     (see body for the precise sequence)
///   * Arithmetic / constants / `LocalGet` stay as in v1.beta
///
/// Epilogue:
///   `i32.const <return_root_size>; end`
///
/// Returns the encoded body plus the parallel vector of source
/// [`TokenRange`]s for the srcmap zip pass.
fn emit_function_body(
    func: &relon_ir::Func,
    main_layout: &OffsetTable,
    return_layout: &OffsetTable,
) -> Result<(Function, Vec<TokenRange>), CodegenError> {
    // Walk the body once up front to determine which wasm value type
    // the trailing StoreField needs as its spill local. Phase 2.b
    // emits at most one StoreField, so this is a single-pass scan
    // looking for the first such op.
    let store_local_ty = func
        .body
        .iter()
        .find_map(|t| {
            if let Op::StoreField { ty, .. } = &t.op {
                Some(store_field_local_valtype(*ty))
            } else {
                None
            }
        })
        .unwrap_or(ValType::I64);

    // Declare one scratch local of the right shape so
    // `emit_store_field` can spill the value before pushing
    // `out_ptr` underneath it.
    let mut f = Function::new(vec![(1u32, store_local_ty)]);

    // Per-emitted-instruction source ranges, lock-step with the
    // wasm op stream the encoder builds.
    let mut ranges: Vec<TokenRange> = Vec::with_capacity(func.body.len() * 2 + 16);

    // Prologue: in_len guard. `local.get in_len; i32.const N;
    // i32.lt_u; if; unreachable; end`. Total: 6 wasm ops.
    let main_root_size = u32::try_from(main_layout.root_size)
        .map_err(|_| CodegenError::Layout("main schema root_size exceeds u32".into()))?;
    let return_root_size = u32::try_from(return_layout.root_size)
        .map_err(|_| CodegenError::Layout("return schema root_size exceeds u32".into()))?;

    emit_size_guard(
        &mut f,
        &mut ranges,
        WASM_LOCAL_IN_LEN,
        main_root_size,
        func.range,
    );
    emit_size_guard(
        &mut f,
        &mut ranges,
        WASM_LOCAL_OUT_CAP,
        return_root_size,
        func.range,
    );

    // Virtual stack used to validate arithmetic type tags.
    let mut vstack: Vec<IrType> = Vec::new();
    let param_types = &func.params;

    for tagged in &func.body {
        match &tagged.op {
            Op::ConstI64(v) => {
                f.instruction(&Instruction::I64Const(*v));
                vstack.push(IrType::I64);
                ranges.push(tagged.range);
            }
            Op::ConstF64(v) => {
                f.instruction(&Instruction::F64Const(Ieee64::from(v.into_inner())));
                vstack.push(IrType::F64);
                ranges.push(tagged.range);
            }
            Op::LocalGet(idx) => {
                // Phase 2.b's `LocalGet` only refers to handshake
                // slots (the four i32 params). User-facing field
                // access goes through `LoadField`.
                let ty = *param_types
                    .get(*idx as usize)
                    .ok_or(CodegenError::MixedNumericTypes)?;
                f.instruction(&Instruction::LocalGet(*idx));
                vstack.push(ty);
                ranges.push(tagged.range);
            }
            Op::LoadField { offset, ty } => {
                emit_load_field(&mut f, &mut ranges, *offset, *ty, tagged.range);
                vstack.push(load_field_stack_type(*ty));
            }
            Op::StoreField { offset, ty } => {
                // Store layout: the value to store is currently on
                // top of the stack. Wasm's `<load/store>` op takes
                // `[addr, value]` (in that order), so we have to
                // shuffle: emit `local.get $out_ptr` *before* the
                // value computation. Phase 2.b only ever lowers a
                // single trailing StoreField per body, so we don't
                // need a generic shuffle — we instead spill the
                // value to a synthesized local just before storing.
                //
                // Simpler approach: insert the address push right
                // before the StoreField op. But the value is already
                // on the stack — we'd need to swap. Wasm's MVP has
                // no `swap`, so we spill to a local instead.
                //
                // For Phase 2.b we sidestep the shuffle entirely by
                // requiring the StoreField caller to have already
                // emitted `local.get $out_ptr` before the value
                // computation. Lowering does *not* emit that pre-
                // push, so we must spill here.
                //
                // Emit sequence (for non-Null types):
                //   <value already on stack>
                //   local.set $tmp
                //   local.get $out_ptr
                //   local.get $tmp
                //   <store>.offset=N
                let popped = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
                if popped != stack_type_for_storefield(*ty) {
                    return Err(CodegenError::MixedNumericTypes);
                }
                emit_store_field(&mut f, &mut ranges, *offset, *ty, tagged.range)?;
            }
            Op::Add(tag) => {
                emit_arith(&mut f, &mut vstack, *tag, ArithOp::Add)?;
                ranges.push(tagged.range);
            }
            Op::Sub(tag) => {
                emit_arith(&mut f, &mut vstack, *tag, ArithOp::Sub)?;
                ranges.push(tagged.range);
            }
            Op::Mul(tag) => {
                emit_arith(&mut f, &mut vstack, *tag, ArithOp::Mul)?;
                ranges.push(tagged.range);
            }
            Op::Div(tag) => {
                emit_arith(&mut f, &mut vstack, *tag, ArithOp::Div)?;
                ranges.push(tagged.range);
            }
            Op::Mod(tag) => {
                emit_arith(&mut f, &mut vstack, *tag, ArithOp::Mod)?;
                ranges.push(tagged.range);
            }
            Op::Return => {
                // Wasm encodes "return at end" as a bare `end` —
                // the function's last expression on the stack is
                // the result. Phase 2.b pushes `bytes_written`
                // below; the actual `End` is emitted at the very
                // bottom of this function.
            }
        }
    }

    // Epilogue: push `bytes_written = return_root_size` and emit the
    // trailing `End`.
    f.instruction(&Instruction::I32Const(return_root_size as i32));
    ranges.push(func.range);

    f.instruction(&Instruction::End);
    ranges.push(func.range);

    Ok((f, ranges))
}

/// Wasm-side stack representation of a loaded field. `Int` / `Float`
/// load as `i64` / `f64`; `Bool` / `Null` load as `i32`.
fn load_field_stack_type(ty: IrType) -> IrType {
    match ty {
        IrType::I64 | IrType::F64 => ty,
        IrType::Bool | IrType::Null => IrType::I32,
        // `I32` field reads aren't used in Phase 2.b (the canonical
        // schema doesn't surface a raw i32 leaf), but we keep the
        // arm exhaustive for forward compat.
        IrType::I32 => IrType::I32,
    }
}

/// The stack value type a `StoreField` of `ty` consumes. Must match
/// what `LoadField` of the same `ty` (or arithmetic on Int/Float
/// values) leaves on the operand stack.
fn stack_type_for_storefield(ty: IrType) -> IrType {
    load_field_stack_type(ty)
}

/// Emit `local.get $slot; i32.const limit; i32.lt_u; if; unreachable; end`.
/// Records six srcmap entries — one per emitted instruction — anchored
/// at the function's declaration range so a trap inside the guard
/// resolves to the `#main(...)` line.
fn emit_size_guard(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    slot: u32,
    limit: u32,
    range: TokenRange,
) {
    f.instruction(&Instruction::LocalGet(slot));
    ranges.push(range);
    f.instruction(&Instruction::I32Const(limit as i32));
    ranges.push(range);
    f.instruction(&Instruction::I32LtU);
    ranges.push(range);
    f.instruction(&Instruction::If(BlockType::Empty));
    ranges.push(range);
    f.instruction(&Instruction::Unreachable);
    ranges.push(range);
    f.instruction(&Instruction::End);
    ranges.push(range);
}

/// Emit the `LoadField` wasm sequence for `ty` at byte `offset`.
fn emit_load_field(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    offset: u32,
    ty: IrType,
    range: TokenRange,
) {
    match ty {
        IrType::Null => {
            // `Null` fields read as the constant `0` — no memory
            // access needed. One emitted op (`i32.const 0`).
            f.instruction(&Instruction::I32Const(0));
            ranges.push(range);
        }
        IrType::Bool => {
            f.instruction(&Instruction::LocalGet(WASM_LOCAL_IN_PTR));
            ranges.push(range);
            f.instruction(&Instruction::I32Load8U(MemArg {
                offset: offset as u64,
                align: 0,
                memory_index: 0,
            }));
            ranges.push(range);
        }
        IrType::I64 => {
            f.instruction(&Instruction::LocalGet(WASM_LOCAL_IN_PTR));
            ranges.push(range);
            f.instruction(&Instruction::I64Load(MemArg {
                offset: offset as u64,
                // 8-byte alignment for i64 (log2 = 3).
                align: 3,
                memory_index: 0,
            }));
            ranges.push(range);
        }
        IrType::F64 => {
            f.instruction(&Instruction::LocalGet(WASM_LOCAL_IN_PTR));
            ranges.push(range);
            f.instruction(&Instruction::F64Load(MemArg {
                offset: offset as u64,
                align: 3,
                memory_index: 0,
            }));
            ranges.push(range);
        }
        IrType::I32 => {
            f.instruction(&Instruction::LocalGet(WASM_LOCAL_IN_PTR));
            ranges.push(range);
            f.instruction(&Instruction::I32Load(MemArg {
                offset: offset as u64,
                align: 2,
                memory_index: 0,
            }));
            ranges.push(range);
        }
    }
}

/// Emit the `StoreField` wasm sequence for `ty` at byte `offset`.
///
/// Wasm stores take `[addr, value, <store>]`, but at emit time the
/// value sits on top of the stack and the address is still in the
/// `$out_ptr` local. We spill the value to a per-store synthesized
/// local so the address can be pushed underneath it without needing
/// a `swap` opcode (wasm MVP has none).
///
/// For Phase 2.b, every body emits at most one `StoreField`, so we
/// can hard-code the spill local index without colliding. We use a
/// fresh local per emit by passing it as `Function::new(locals)` —
/// but that requires preflighting all locals up front. Instead, we
/// reserve a single i64-or-f64-shaped temp slot at body start (TODO
/// in Phase 2.c if multiple StoreFields appear in one body).
///
/// Simpler current approach: do the spill via a local at index 4
/// (right after the four handshake params). The function header
/// reserves this slot via `Function::new(...)` at the caller side;
/// see `emit_function_body` below.
fn emit_store_field(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    offset: u32,
    ty: IrType,
    range: TokenRange,
) -> Result<(), CodegenError> {
    // Phase 2.b sidesteps the spill problem by using a synthesised
    // local. We hold one of three flavours of temp:
    //   - i64 for Int
    //   - f64 for Float
    //   - i32 for Bool / Null
    // The body always uses exactly one; emit_function_body declares
    // the matching local up front (see STORE_TMP_LOCAL_INDEX).
    let store_op = match ty {
        IrType::I64 => Instruction::I64Store(MemArg {
            offset: offset as u64,
            align: 3,
            memory_index: 0,
        }),
        IrType::F64 => Instruction::F64Store(MemArg {
            offset: offset as u64,
            align: 3,
            memory_index: 0,
        }),
        IrType::Bool | IrType::Null | IrType::I32 => Instruction::I32Store8(MemArg {
            offset: offset as u64,
            align: 0,
            memory_index: 0,
        }),
    };
    f.instruction(&Instruction::LocalSet(STORE_TMP_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(WASM_LOCAL_OUT_PTR));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(STORE_TMP_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&store_op);
    ranges.push(range);
    Ok(())
}

/// Wasm-local index used as the scratch slot for `emit_store_field`.
/// Sits right after the four binary-handshake params; the function
/// declares one entry of the appropriate value type as part of its
/// `locals` header.
const STORE_TMP_LOCAL_INDEX: u32 = 4;

/// Wasm value type used for the scratch local in `emit_store_field`.
/// `Int` stores need an i64 slot; `Float` an f64 slot; `Bool` / `Null`
/// an i32 slot. The slot is preallocated by `emit_function_body`
/// based on the first `StoreField` op in the body — see the call site
/// for the single-StoreField assumption rationale.
fn store_field_local_valtype(ty: IrType) -> ValType {
    match ty {
        IrType::I64 => ValType::I64,
        IrType::F64 => ValType::F64,
        IrType::I32 | IrType::Bool | IrType::Null => ValType::I32,
    }
}

enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

fn emit_arith(
    f: &mut Function,
    vstack: &mut Vec<IrType>,
    tag: IrType,
    op: ArithOp,
) -> Result<(), CodegenError> {
    let rhs = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
    let lhs = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
    if lhs != tag || rhs != tag {
        return Err(CodegenError::MixedNumericTypes);
    }
    let instr = match (tag, op) {
        (IrType::I64, ArithOp::Add) => Instruction::I64Add,
        (IrType::I64, ArithOp::Sub) => Instruction::I64Sub,
        (IrType::I64, ArithOp::Mul) => Instruction::I64Mul,
        (IrType::I64, ArithOp::Div) => Instruction::I64DivS,
        (IrType::I64, ArithOp::Mod) => Instruction::I64RemS,
        (IrType::F64, ArithOp::Add) => Instruction::F64Add,
        (IrType::F64, ArithOp::Sub) => Instruction::F64Sub,
        (IrType::F64, ArithOp::Mul) => Instruction::F64Mul,
        (IrType::F64, ArithOp::Div) => Instruction::F64Div,
        (IrType::F64, ArithOp::Mod) => return Err(CodegenError::MixedNumericTypes),
        // Arithmetic on I32 / Bool / Null is not part of the surface
        // — the lowering pass rejects bodies with these tags. A
        // hand-crafted IR landing here gets the same treatment as a
        // mixed-type body.
        (IrType::I32, _) | (IrType::Bool, _) | (IrType::Null, _) => {
            return Err(CodegenError::MixedNumericTypes);
        }
    };
    f.instruction(&instr);
    vstack.push(tag);
    Ok(())
}

/// Map an [`IrType`] to its wasm value type.
fn ir_to_val_type(t: &IrType) -> ValType {
    match t {
        IrType::I32 => ValType::I32,
        IrType::I64 => ValType::I64,
        IrType::F64 => ValType::F64,
        // Bool / Null occupy an i32 slot on the wasm operand stack
        // (they're 1 byte on the wire but always loaded into an i32
        // via `i32.load8_u` / `i32.const`).
        IrType::Bool | IrType::Null => ValType::I32,
    }
}

/// Phase 1.alpha smoke generator. Retained as a regression reference
/// so the encoder + engine smoke test survives later codegen rewrites.
/// **Not part of the v1.beta / 2.b pipeline** — exists solely to prove
/// `wasm-encoder` + `wasmtime` keep linking after dependency bumps.
///
/// Body (wat-style):
/// ```text
/// (func (export "run_main") (param i32) (result i32)
///   local.get 0
///   i32.const 2
///   i32.mul)
/// ```
pub fn compile_hardcoded_double() -> Vec<u8> {
    let mut module = Module::new();

    let mut types = TypeSection::new();
    types.ty().function(vec![ValType::I32], vec![ValType::I32]);
    module.section(&types);

    let mut functions = FunctionSection::new();
    functions.function(0);
    module.section(&functions);

    let mut exports = ExportSection::new();
    exports.export("run_main", ExportKind::Func, 0);
    module.section(&exports);

    let mut codes = CodeSection::new();
    let locals: Vec<(u32, ValType)> = Vec::new();
    let mut f = Function::new(locals);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(2));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::End);
    codes.function(&f);
    module.section(&codes);

    module.finish()
}

/// Loaded wasm module surface used by host SDKs.
///
/// Wraps the raw module bytes alongside the parsed `relon.abi` +
/// `relon.srcmap` sections so a host can keep one value handy for
/// instantiation, trap translation, and ABI compatibility checks.
#[derive(Debug, Clone)]
pub struct WasmModule {
    /// Raw module bytes ready to be passed to a wasm engine.
    bytes: Vec<u8>,
    /// ABI metadata parsed out of `relon.abi`.
    abi: AbiMetadata,
    /// Source map parsed out of `relon.srcmap`.
    srcmap: SrcMap,
}

impl WasmModule {
    /// Borrow the raw module bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Borrow the parsed ABI metadata.
    pub fn abi(&self) -> &AbiMetadata {
        &self.abi
    }

    /// Borrow the parsed source map.
    pub fn srcmap(&self) -> &SrcMap {
        &self.srcmap
    }

    /// Parse a wasm module's custom sections and validate the ABI
    /// shape only (versions, magic). Schema-hash validation requires
    /// the caller to supply the expected `#main` / return schemas via
    /// [`Self::from_bytes_with_schema`]; this entry point is for
    /// hosts that don't yet know what they expect (introspection
    /// tools, debug dumps).
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, LoadError> {
        Self::from_bytes_inner(bytes, None)
    }

    /// Parse a wasm module and validate it against the supplied
    /// schemas. Returns [`LoadError::Abi(AbiError::SchemaDrift)`]
    /// when either hash disagrees with the module's `relon.abi`
    /// payload.
    pub fn from_bytes_with_schema(
        bytes: Vec<u8>,
        expected_main: &Schema,
        expected_return: &Schema,
    ) -> Result<Self, LoadError> {
        let main_hash = schema_hash(expected_main);
        let return_hash = schema_hash(expected_return);
        Self::from_bytes_inner(bytes, Some((main_hash, return_hash)))
    }

    fn from_bytes_inner(
        bytes: Vec<u8>,
        expected: Option<([u8; 32], [u8; 32])>,
    ) -> Result<Self, LoadError> {
        let mut abi_bytes: Option<Vec<u8>> = None;
        let mut srcmap_bytes: Option<Vec<u8>> = None;

        for payload in wasmparser::Parser::new(0).parse_all(&bytes) {
            let payload = payload.map_err(|e| LoadError::WasmParse(e.to_string()))?;
            if let wasmparser::Payload::CustomSection(reader) = payload {
                match reader.name() {
                    name if name == abi::SECTION_NAME => {
                        abi_bytes = Some(reader.data().to_vec());
                    }
                    name if name == srcmap::SECTION_NAME => {
                        srcmap_bytes = Some(reader.data().to_vec());
                    }
                    _ => {}
                }
            }
        }

        let abi_bytes = abi_bytes.ok_or(LoadError::Abi(AbiError::AbiSectionMissing))?;
        let abi = abi::decode(&abi_bytes)?;
        abi::check_versions(&abi)?;

        if let Some((main_hash, return_hash)) = expected {
            if abi.main_schema_hash != main_hash {
                return Err(LoadError::Abi(AbiError::SchemaDrift { which: "main" }));
            }
            if abi.return_schema_hash != return_hash {
                return Err(LoadError::Abi(AbiError::SchemaDrift { which: "return" }));
            }
        }

        let srcmap_bytes = srcmap_bytes.ok_or(LoadError::MissingCustomSection {
            name: srcmap::SECTION_NAME,
        })?;
        let srcmap = srcmap::decode_from_bytes(&srcmap_bytes)?;

        Ok(Self { bytes, abi, srcmap })
    }
}
