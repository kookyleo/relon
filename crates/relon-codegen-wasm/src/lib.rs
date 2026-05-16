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
    builtin_stdlib, Func as IrFunc, IrType, LoweredEntry, Module as IrModule, Op,
    WASM_LOCAL_IN_LEN, WASM_LOCAL_IN_PTR, WASM_LOCAL_OUT_CAP, WASM_LOCAL_OUT_PTR,
};
use relon_parser::TokenRange;
use std::collections::HashMap;
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, CustomSection, DataSection, ExportKind, ExportSection,
    Function, FunctionSection, GlobalSection, GlobalType, Ieee64, Instruction, MemArg,
    MemorySection, MemoryType, Module, TypeSection, ValType,
};

/// Memory export name used by the binary handshake.
const MEMORY_EXPORT_NAME: &str = "memory";

/// Default initial memory size in wasm pages (64 KiB each). Phase 2.b
/// only needs a small staging area for the in/out buffers — the host
/// places its pointers at whatever offsets it pleases.
const DEFAULT_MEMORY_PAGES: u64 = 1;

/// Base address of the codegen-owned read-only data section inside
/// wasm linear memory. Phase 3.a parks `ConstString` / `ConstListInt`
/// records here so a wasm-side memory.copy can pull bytes into the
/// caller's `out_buf` tail area at runtime.
///
/// Picked well above the host's typical staging area (`in_ptr` /
/// `out_ptr` at offsets 0 / 256 in the integration tests) so the
/// regions don't collide. Hosts that want to write past 4 KiB should
/// either drop their pointers below it or grow memory; both stay
/// compatible because the linear memory is host-writable.
const DATA_SECTION_BASE: u32 = 4096;

/// Exported i32 global telling the host where the codegen-managed
/// const data section ends. Host SDKs that allocate `in_buf` /
/// `out_buf` dynamically can read this once at instantiation and
/// place their buffers above it to avoid overwriting const records.
const DATA_TOP_GLOBAL_EXPORT_NAME: &str = "relon_data_top";

/// Wasm-local index used as the scratch slot for `emit_store_field`
/// when the value being stored is a scalar (i32 / i64 / f64). Sits
/// right after the four binary-handshake params; the function
/// declares one entry of the appropriate value type as part of its
/// `locals` header.
const STORE_TMP_LOCAL_INDEX: u32 = 4;
/// Wasm-local index of the tail-area write cursor (i32). Initialised
/// to `return_root_size` and bumped after every String / List<Int>
/// return write so multiple pointer-indirect outputs can coexist in
/// the same `out_buf`.
const TAIL_CURSOR_LOCAL_INDEX: u32 = 5;
/// Wasm-local index of the memcpy source pointer scratch (i32). Used
/// by [`Op::StoreField`] of pointer-indirect types — we `tee` the
/// source pointer here, then pull it back out twice (once for the
/// length read, once for the memory.copy source argument).
const MEMCPY_SRC_LOCAL_INDEX: u32 = 6;
/// Wasm-local index of the memcpy record-size scratch (i32). Holds
/// the total byte count we feed to `memory.copy` for the current
/// pointer-indirect store; the same value bumps `$tail_cursor` after
/// the copy completes.
const MEMCPY_LEN_LOCAL_INDEX: u32 = 7;
/// Scratch i32 local used as a generic spill for the new pointer-
/// store-at-record path (Phase 3.b). Holds the popped record-relative
/// address before the i32.store instruction consumes it.
const RECORD_STORE_TMP_LOCAL_INDEX: u32 = 8;
/// First wasm-local index reserved for user-let bindings. Each
/// `Op::LetSet` allocates a fresh local of the right valtype past
/// this point; codegen tracks the index map per function in
/// [`emit_function_body`].
const FIRST_LET_LOCAL_INDEX: u32 = 9;

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

    // Phase 4.a: prepend bundled stdlib functions before the user
    // functions. Each stdlib entry contributes one wasm function at
    // index `0..N`; user functions slide to `N..N+U`. The
    // `entry_func_index` field on the IR module is an *IR-side*
    // index (into `ir.funcs`); when we slot the user functions after
    // the stdlib, we shift it to compute the wasm-level export
    // target.
    let stdlib_funcs = builtin_stdlib();
    let stdlib_count = stdlib_funcs.len();
    let combined_funcs: Vec<IrFunc> = stdlib_funcs
        .into_iter()
        .map(stdlib_to_ir_func)
        .chain(ir.funcs.iter().cloned())
        .collect();
    let combined_entry_index = ir.entry_func_index.map(|i| i + stdlib_count);

    // Walk every function body up front and lay out the const data
    // section. Each `ConstString` / `ConstListInt` op gets a stable
    // absolute address inside `DATA_SECTION_BASE..DATA_SECTION_BASE+
    // const_pool.bytes.len()` so the runtime emit can hardcode an
    // `i32.const <addr>` per op. The walk covers both stdlib and
    // user funcs so a stdlib body using a `ConstString` (none in
    // Phase 4.a, but the framework stays uniform) gets its data
    // laid out alongside the user records.
    let const_pool = build_const_pool_for_funcs(&combined_funcs)?;

    let mut module = Module::new();
    let mut types = TypeSection::new();
    let mut functions = FunctionSection::new();
    let mut memories = MemorySection::new();
    let mut globals = GlobalSection::new();
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

    // `relon_data_top` global — high-water mark of the const-data
    // region. Host SDKs can place their `in_buf` / `out_buf` past
    // this point at instantiation time without colliding with const
    // records. We always emit the global (even when the pool is
    // empty) so a runtime check against it doesn't need to special-
    // case the missing-export branch.
    let data_top = DATA_SECTION_BASE + const_pool.total_bytes() as u32;
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: false,
            shared: false,
        },
        &ConstExpr::i32_const(data_top as i32),
    );
    exports.export(DATA_TOP_GLOBAL_EXPORT_NAME, ExportKind::Global, 0);

    let mut type_table: Vec<(Vec<ValType>, ValType, u32)> = Vec::new();
    let mut per_func_ranges: Vec<(TokenRange, Vec<TokenRange>)> =
        Vec::with_capacity(combined_funcs.len());

    for (func_index, func) in combined_funcs.iter().enumerate() {
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

        if Some(func_index) == combined_entry_index {
            exports.export(func.name.as_str(), ExportKind::Func, func_index as u32);
        }

        let is_entry = Some(func_index) == combined_entry_index;
        let (body, ranges) =
            emit_function_body(func, &main_layout, &return_layout, &const_pool, is_entry)?;
        codes.function(&body);
        per_func_ranges.push((func.range, ranges));
    }

    module.section(&types);
    module.section(&functions);
    module.section(&memories);
    module.section(&globals);
    module.section(&exports);
    module.section(&codes);

    // Initialise the const-data region inside linear memory. Active
    // segment at `DATA_SECTION_BASE`; the body bytes are exactly the
    // `[len:u32 LE][payload]` records the `Op::StoreField` path will
    // `memory.copy` into the caller's `out_buf`.
    if !const_pool.bytes.is_empty() {
        let mut data = DataSection::new();
        data.active(
            0,
            &ConstExpr::i32_const(DATA_SECTION_BASE as i32),
            const_pool.bytes.iter().copied(),
        );
        module.section(&data);
    }

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

/// Layout of the per-module read-only const data section.
///
/// Every [`Op::ConstString`] and [`Op::ConstListInt`] in the IR maps
/// to a single record in `bytes`; codegen emits `i32.const <addr>`
/// at the op's source position, where `<addr>` is
/// `DATA_SECTION_BASE + offset`. The record layout matches the
/// host-side `BufferBuilder` so the wasm runtime can `memory.copy`
/// the bytes verbatim into the caller's `out_buf` tail area without
/// reformatting.
///
/// String record: `[len:u32 LE][utf8 bytes]`, total `4 + value.len()`.
/// List<Int> record: `[len:u32 LE][pad:u32 zero][i64 elements LE]`,
/// total `8 + 8 * elements.len()`. The 4-byte pad keeps the elements
/// at byte offset 8 within the record so the wasm runtime can place
/// the record at an 8-aligned `out_buf` tail offset (the
/// `BufferReader` re-aligns past the length prefix the same way).
#[derive(Debug, Default)]
struct ConstPool {
    /// The encoded bytes — passed verbatim to a wasm `Data` section
    /// initialiser anchored at `DATA_SECTION_BASE`.
    bytes: Vec<u8>,
    /// Map from per-module `ConstString` index to byte offset inside
    /// `bytes`. Absolute memory address = `DATA_SECTION_BASE + offset`.
    string_offsets: HashMap<u32, u32>,
    /// Map from per-module `ConstListInt` index to byte offset.
    list_int_offsets: HashMap<u32, u32>,
}

impl ConstPool {
    fn total_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// Look up the absolute wasm-memory address of a String const.
    fn string_addr(&self, idx: u32) -> Result<u32, CodegenError> {
        self.string_offsets
            .get(&idx)
            .map(|off| DATA_SECTION_BASE + off)
            .ok_or(CodegenError::MixedNumericTypes)
    }

    /// Look up the absolute wasm-memory address of a List<Int> const.
    fn list_int_addr(&self, idx: u32) -> Result<u32, CodegenError> {
        self.list_int_offsets
            .get(&idx)
            .map(|off| DATA_SECTION_BASE + off)
            .ok_or(CodegenError::MixedNumericTypes)
    }
}

/// Pre-walk every IR function body and lay out the per-module const
/// data records. Returns the encoded bytes plus index lookups so the
/// runtime emit pass can hardcode the matching `i32.const <addr>`.
fn build_const_pool_for_funcs(funcs: &[IrFunc]) -> Result<ConstPool, CodegenError> {
    let mut pool = ConstPool::default();
    for func in funcs {
        collect_consts(&func.body, &mut pool)?;
    }
    Ok(pool)
}

/// Translate a [`relon_ir::StdlibFunction`] into a wasm-codegen-ready
/// [`IrFunc`]. The stdlib body uses the same IR op stream as user
/// functions; only the synthetic `name` distinguishes a stdlib entry
/// from a user `#main` function in diagnostics.
///
/// Synthetic source range: stdlib functions don't appear in user
/// source, but the srcmap section's invariant is `line >= 1` /
/// `col >= 1` (1-based positions). We anchor every stdlib op at
/// `(line=1, col=1)` with `file_idx=0` (the placeholder file slot);
/// a host translating a trap inside a stdlib body still gets a
/// non-degenerate position to surface, and the srcmap roundtrip
/// invariants stay intact.
fn stdlib_to_ir_func(f: relon_ir::StdlibFunction) -> IrFunc {
    let synthetic_range = synthetic_stdlib_range();
    let body = f
        .body
        .into_iter()
        .map(|mut t| {
            // Stdlib bodies are hand-written with `TokenRange::default()`
            // ranges; rewrite them so the srcmap pass sees the
            // 1-based synthetic range uniformly.
            t.range = synthetic_range;
            t
        })
        .collect();
    IrFunc {
        name: format!("__relon_stdlib_{}", f.name),
        params: f.params,
        ret: f.ret,
        body,
        range: synthetic_range,
    }
}

/// 1-based synthetic source position used for every stdlib op range.
/// Pinning the `line` / `col` to `1` keeps the srcmap encoder's
/// 1-based invariant true; a host that translates a trap inside a
/// stdlib body still surfaces a well-formed position (the host's
/// renderer can recognise the synthetic anchor and append a
/// "in stdlib" marker if it wants).
fn synthetic_stdlib_range() -> TokenRange {
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

/// Recursively walk a body (descending into [`Op::If`] arms) and
/// append const records to the pool. Records are appended in the
/// order they appear; the offset map is keyed on the IR-level index
/// so cross-function references still point at the right bytes.
fn collect_consts(body: &[relon_ir::TaggedOp], pool: &mut ConstPool) -> Result<(), CodegenError> {
    for tagged in body {
        match &tagged.op {
            Op::ConstString { idx, value } => {
                let value_bytes = value.as_bytes();
                let len = u32::try_from(value_bytes.len())
                    .map_err(|_| CodegenError::Layout("string literal exceeds u32".into()))?;
                let offset = u32::try_from(pool.bytes.len()).map_err(|_| {
                    CodegenError::Layout("const data section exceeds u32 bytes".into())
                })?;
                pool.bytes.extend_from_slice(&len.to_le_bytes());
                pool.bytes.extend_from_slice(value_bytes);
                pool.string_offsets.insert(*idx, offset);
            }
            Op::ConstListInt { idx, elements } => {
                // Align the record start to 8 inside the data section
                // so the in-record `[len:4][pad:4][i64 elements]`
                // layout is byte-identical to what the host builder
                // would have written at an 8-aligned offset. Without
                // this alignment the memory.copy would still produce
                // correct bytes (memcpy is byte-level), but keeping
                // the source aligned makes hand-debugging the wasm
                // module easier.
                while !pool.bytes.len().is_multiple_of(8) {
                    pool.bytes.push(0);
                }
                let offset = u32::try_from(pool.bytes.len()).map_err(|_| {
                    CodegenError::Layout("const data section exceeds u32 bytes".into())
                })?;
                let count = u32::try_from(elements.len()).map_err(|_| {
                    CodegenError::Layout("list literal exceeds u32 elements".into())
                })?;
                pool.bytes.extend_from_slice(&count.to_le_bytes());
                // 4-byte pad so the i64 payload sits at record_offset + 8.
                pool.bytes.extend_from_slice(&[0u8; 4]);
                for v in elements {
                    pool.bytes.extend_from_slice(&v.to_le_bytes());
                }
                pool.list_int_offsets.insert(*idx, offset);
            }
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                collect_consts(then_body, pool)?;
                collect_consts(else_body, pool)?;
            }
            _ => {}
        }
    }
    Ok(())
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
    const_pool: &ConstPool,
    is_entry: bool,
) -> Result<(Function, Vec<TokenRange>), CodegenError> {
    // Walk the body (recursively into if-branches) to determine
    // which wasm value type the trailing StoreField needs as its
    // spill local. The lowering pass keeps the StoreField at the
    // top-level tail, but the scan recurses anyway so a future phase
    // moving the store inside a branch still picks up the right
    // valtype.
    let store_local_ty = find_store_field_local_ty(&func.body).unwrap_or(ValType::I64);

    // Discover every user-let-binding referenced in this body so we
    // can declare matching wasm locals up front. Each `Op::LetSet`
    // records a `(idx, IrType)` pair; codegen turns the IR-local
    // index into a wasm-local index at `FIRST_LET_LOCAL_INDEX + idx`.
    let let_locals = collect_let_locals(&func.body)?;
    let max_let_idx = let_locals.iter().map(|(idx, _)| *idx).max();

    // Phase 3.b: the dict-construction ops carry a per-function
    // record-local index. We scan the body to find the highest seen
    // index so the locals header reserves enough i32 slots, placed
    // immediately after the let-locals so each record's base offset
    // sits at a stable index.
    let max_record_idx = collect_max_record_idx(&func.body);

    // Whether the body needs the pointer-indirect store machinery —
    // tail cursor + memcpy scratch locals plus the `out_cap` runtime
    // bounds check that traps when the tail area overflows.
    let needs_tail_cursor = body_needs_tail_cursor(&func.body);

    // Locals layout (positions in the wasm-encoder's `locals` header):
    //   STORE_TMP_LOCAL_INDEX = 4         (scalar spill, ValType varies)
    //   TAIL_CURSOR_LOCAL_INDEX = 5       (i32)
    //   MEMCPY_SRC_LOCAL_INDEX = 6        (i32)
    //   MEMCPY_LEN_LOCAL_INDEX = 7        (i32)
    //   RECORD_STORE_TMP_LOCAL_INDEX = 8  (i32, Phase 3.b)
    //   FIRST_LET_LOCAL_INDEX..           user-let locals, contiguous
    //   ..record-base locals              i32, one per AllocRootRecord/
    //                                      AllocSubRecord op (Phase 3.b)
    //
    // Even when `needs_tail_cursor` is false (a pure-scalar return
    // body) we still reserve the slots so the contiguous let-local
    // numbering stays stable across body shapes.
    let mut locals_header: Vec<(u32, ValType)> = vec![(1, store_local_ty)];
    // Reserve TAIL_CURSOR / MEMCPY_SRC / MEMCPY_LEN / RECORD_STORE_TMP
    // (4 contiguous i32 slots so the let-locals always start at
    // `FIRST_LET_LOCAL_INDEX = 9`).
    locals_header.push((4, ValType::I32));
    let mut let_locals_count: u32 = 0;
    if let Some(max_idx) = max_let_idx {
        // Allocate one local per declared user-let. Grouping by
        // valtype keeps the locals-header compact, but for simplicity
        // we emit one entry per local in declaration order — the
        // encoder collapses adjacent same-valtype entries on its own.
        let count = max_idx + 1;
        let_locals_count = count;
        let mut by_idx: Vec<Option<IrType>> = vec![None; count as usize];
        for (idx, ty) in &let_locals {
            by_idx[*idx as usize] = Some(*ty);
        }
        for slot in by_idx {
            let vt = slot
                .map(|t| ir_to_val_type(&t))
                // Unused let-local slots default to i32 — the unused
                // declaration costs zero at runtime and keeps the
                // index map dense.
                .unwrap_or(ValType::I32);
            locals_header.push((1, vt));
        }
    }
    // Phase 3.b record-base locals: one i32 per unique
    // record-local index seen in the body.
    if let Some(max_rec) = max_record_idx {
        locals_header.push((max_rec + 1, ValType::I32));
    }
    // Pass record-local base index so the op-walker can compute its
    // matching wasm-local index without re-deriving the offset.
    let record_local_base = FIRST_LET_LOCAL_INDEX + let_locals_count;
    let mut f = Function::new(locals_header);

    // Per-emitted-instruction source ranges, lock-step with the
    // wasm op stream the encoder builds.
    let mut ranges: Vec<TokenRange> = Vec::with_capacity(func.body.len() * 2 + 16);

    // Prologue (entry-only): binary-handshake size guards + tail-
    // cursor init. Stdlib functions don't run this — they have a
    // bespoke `(param) -> ret` signature and rely on the engine to
    // type-check arguments at the call site.
    let main_root_size = u32::try_from(main_layout.root_size)
        .map_err(|_| CodegenError::Layout("main schema root_size exceeds u32".into()))?;
    let return_root_size = u32::try_from(return_layout.root_size)
        .map_err(|_| CodegenError::Layout("return schema root_size exceeds u32".into()))?;

    if is_entry {
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

        // Initialise the tail cursor to `return_root_size` so the
        // first pointer-indirect store lands immediately after the
        // fixed area inside `out_buf`. Skipped when the body has no
        // String/List return — leaves `TAIL_CURSOR_LOCAL_INDEX` at
        // its default zero (and unused in that case).
        if needs_tail_cursor {
            f.instruction(&Instruction::I32Const(return_root_size as i32));
            ranges.push(func.range);
            f.instruction(&Instruction::LocalSet(TAIL_CURSOR_LOCAL_INDEX));
            ranges.push(func.range);
        }
    }

    // Virtual stack used to validate arithmetic type tags.
    let mut vstack: Vec<IrType> = Vec::new();
    let mut ectx = EmitCtx { record_local_base };
    let _ = return_root_size; // referenced earlier in this function
    emit_op_seq(
        &mut f,
        &mut ranges,
        &mut vstack,
        &func.body,
        func,
        const_pool,
        &mut ectx,
    )?;

    // Epilogue.
    //
    // Entry function: push `bytes_written` (the tail cursor when the
    // body emitted pointer-indirect stores, otherwise the fixed-area
    // return root size) then emit the trailing `End`. Stdlib functions
    // leave their single result value on top of the operand stack —
    // wasm's implicit return rule turns the trailing `end` into the
    // function's return.
    if is_entry {
        if needs_tail_cursor {
            f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
            ranges.push(func.range);
        } else {
            f.instruction(&Instruction::I32Const(return_root_size as i32));
            ranges.push(func.range);
        }
    }

    f.instruction(&Instruction::End);
    ranges.push(func.range);

    Ok((f, ranges))
}

/// Walk the body (descending into `Op::If` arms) and gather every
/// `Op::LetSet` so the function header can declare matching wasm
/// locals. Each unique `idx` is recorded with its `IrType`; a
/// duplicate idx with a different type is a lowering bug and
/// surfaces as a codegen error.
fn collect_let_locals(body: &[relon_ir::TaggedOp]) -> Result<Vec<(u32, IrType)>, CodegenError> {
    let mut out: Vec<(u32, IrType)> = Vec::new();
    collect_let_locals_inner(body, &mut out)?;
    Ok(out)
}

fn collect_let_locals_inner(
    body: &[relon_ir::TaggedOp],
    out: &mut Vec<(u32, IrType)>,
) -> Result<(), CodegenError> {
    for tagged in body {
        match &tagged.op {
            Op::LetSet { idx, ty } => {
                if let Some(existing) = out.iter().find(|(i, _)| i == idx) {
                    if existing.1 != *ty {
                        return Err(CodegenError::MixedNumericTypes);
                    }
                } else {
                    out.push((*idx, *ty));
                }
            }
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                collect_let_locals_inner(then_body, out)?;
                collect_let_locals_inner(else_body, out)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Per-function emit-side context threaded through the recursive
/// op walker. Carries the indices / sizes the dict-construction ops
/// need at every emit site without copying them through every
/// helper signature.
#[derive(Debug, Clone, Copy)]
struct EmitCtx {
    /// Wasm-local index of the first record-base local in the
    /// function. An IR `record_local_idx` of `n` maps to
    /// `record_local_base + n`.
    record_local_base: u32,
}

/// Scan `body` (and any nested if-branches) for the largest
/// record-local index referenced by [`Op::AllocRootRecord`] /
/// [`Op::AllocSubRecord`] / [`Op::StoreFieldAtRecord`] /
/// [`Op::PushRecordBase`]. Returns `None` when no record-construction
/// op appears — the function then needs zero record-base locals.
fn collect_max_record_idx(body: &[relon_ir::TaggedOp]) -> Option<u32> {
    let mut max: Option<u32> = None;
    let mut update = |idx: u32| {
        max = Some(max.map_or(idx, |m| m.max(idx)));
    };
    fn walk(body: &[relon_ir::TaggedOp], update: &mut impl FnMut(u32)) {
        for tagged in body {
            match &tagged.op {
                Op::AllocRootRecord { record_local_idx }
                | Op::AllocSubRecord {
                    record_local_idx, ..
                }
                | Op::StoreFieldAtRecord {
                    record_local_idx, ..
                }
                | Op::PushRecordBase { record_local_idx } => update(*record_local_idx),
                Op::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    walk(then_body, update);
                    walk(else_body, update);
                }
                _ => {}
            }
        }
    }
    walk(body, &mut update);
    max
}

/// `true` when the function body emits at least one String / List<Int>
/// store into the `out_buf`. Tail-cursor scratch locals and the
/// runtime out_cap bounds check only matter for these stores.
fn body_needs_tail_cursor(body: &[relon_ir::TaggedOp]) -> bool {
    for tagged in body {
        match &tagged.op {
            Op::StoreField {
                ty: IrType::String | IrType::ListInt,
                ..
            } => {
                return true;
            }
            // Phase 3.b dict-construction ops live entirely on the
            // tail-cursor path: AllocSubRecord bumps it, the field
            // stores write into out_ptr + cursor-relative offsets,
            // and the epilogue uses `$tail_cursor` as `bytes_written`.
            // AllocRootRecord doesn't bump the cursor but still
            // produces a record whose nested pointer-indirect fields
            // need the cursor machinery.
            Op::AllocRootRecord { .. }
            | Op::AllocSubRecord { .. }
            | Op::EmitTailRecordFromAbsoluteAddr { .. } => {
                return true;
            }
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                if body_needs_tail_cursor(then_body) || body_needs_tail_cursor(else_body) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Emit a sequence of [`TaggedOp`] into `f`, growing `ranges` and
/// `vstack` in lock-step. Used by [`emit_function_body`] for the top
/// level body and recursively for the `then`/`else` arms of [`Op::If`].
fn emit_op_seq(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    vstack: &mut Vec<IrType>,
    body: &[relon_ir::TaggedOp],
    func: &relon_ir::Func,
    const_pool: &ConstPool,
    ectx: &mut EmitCtx,
) -> Result<(), CodegenError> {
    let param_types = &func.params;
    for tagged in body {
        match &tagged.op {
            Op::ConstBool(b) => {
                // Bool literal materialises as `i32.const 1/0` so
                // downstream `If` / `i32.eq` see the canonical
                // 0/1 byte form.
                f.instruction(&Instruction::I32Const(if *b { 1 } else { 0 }));
                vstack.push(IrType::Bool);
                ranges.push(tagged.range);
            }
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
            Op::ConstString { idx, .. } => {
                let addr = const_pool.string_addr(*idx)?;
                f.instruction(&Instruction::I32Const(addr as i32));
                vstack.push(IrType::String);
                ranges.push(tagged.range);
            }
            Op::ConstListInt { idx, .. } => {
                let addr = const_pool.list_int_addr(*idx)?;
                f.instruction(&Instruction::I32Const(addr as i32));
                vstack.push(IrType::ListInt);
                ranges.push(tagged.range);
            }
            Op::LetGet { idx, ty } => {
                let local_idx = FIRST_LET_LOCAL_INDEX
                    .checked_add(*idx)
                    .ok_or(CodegenError::MixedNumericTypes)?;
                f.instruction(&Instruction::LocalGet(local_idx));
                vstack.push(*ty);
                ranges.push(tagged.range);
            }
            Op::LetSet { idx, ty } => {
                let local_idx = FIRST_LET_LOCAL_INDEX
                    .checked_add(*idx)
                    .ok_or(CodegenError::MixedNumericTypes)?;
                let popped = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
                if popped.wasm_slot() != ty.wasm_slot() {
                    return Err(CodegenError::MixedNumericTypes);
                }
                f.instruction(&Instruction::LocalSet(local_idx));
                ranges.push(tagged.range);
            }
            Op::LocalGet(idx) => {
                // `LocalGet` refers to handshake slots (the four i32
                // params). User-facing field access goes through
                // `LoadField`; user-let bindings go through `LetGet`.
                let ty = *param_types
                    .get(*idx as usize)
                    .ok_or(CodegenError::MixedNumericTypes)?;
                f.instruction(&Instruction::LocalGet(*idx));
                vstack.push(ty);
                ranges.push(tagged.range);
            }
            Op::LoadField { offset, ty } => {
                emit_load_field(f, ranges, *offset, *ty, tagged.range);
                vstack.push(load_field_stack_type(*ty));
            }
            Op::LoadStringPtr { offset } => {
                emit_load_absolute_pointer(f, ranges, *offset, tagged.range);
                vstack.push(IrType::String);
            }
            Op::LoadListIntPtr { offset } => {
                emit_load_absolute_pointer(f, ranges, *offset, tagged.range);
                vstack.push(IrType::ListInt);
            }
            Op::StoreField { offset, ty } => {
                let popped = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
                if popped.wasm_slot() != stack_type_for_storefield(*ty).wasm_slot() {
                    return Err(CodegenError::MixedNumericTypes);
                }
                match ty {
                    IrType::String | IrType::ListInt => {
                        // Pointer-indirect store. The top-of-stack
                        // value is the absolute memory address of a
                        // `[len:u32 LE][...]` record (either a
                        // ConstString/ConstListInt addr or a
                        // LoadStringPtr/LoadListIntPtr-lifted in_buf
                        // pointer). We memcpy the record into the
                        // out_buf tail area, then store the
                        // buffer-relative offset of the record to
                        // the fixed-area slot.
                        emit_store_pointer_indirect(f, ranges, *offset, *ty, tagged.range)?;
                    }
                    _ => {
                        emit_store_field(f, ranges, *offset, *ty, tagged.range)?;
                    }
                }
            }
            Op::Add(tag) => {
                emit_arith(f, vstack, *tag, ArithOp::Add)?;
                ranges.push(tagged.range);
            }
            Op::Sub(tag) => {
                emit_arith(f, vstack, *tag, ArithOp::Sub)?;
                ranges.push(tagged.range);
            }
            Op::Mul(tag) => {
                emit_arith(f, vstack, *tag, ArithOp::Mul)?;
                ranges.push(tagged.range);
            }
            Op::Div(tag) => {
                emit_arith(f, vstack, *tag, ArithOp::Div)?;
                ranges.push(tagged.range);
            }
            Op::Mod(tag) => {
                emit_arith(f, vstack, *tag, ArithOp::Mod)?;
                ranges.push(tagged.range);
            }
            Op::Eq(tag) => {
                emit_cmp(f, vstack, *tag, CmpOp::Eq)?;
                ranges.push(tagged.range);
            }
            Op::Ne(tag) => {
                emit_cmp(f, vstack, *tag, CmpOp::Ne)?;
                ranges.push(tagged.range);
            }
            Op::Lt(tag) => {
                emit_cmp(f, vstack, *tag, CmpOp::Lt)?;
                ranges.push(tagged.range);
            }
            Op::Le(tag) => {
                emit_cmp(f, vstack, *tag, CmpOp::Le)?;
                ranges.push(tagged.range);
            }
            Op::Gt(tag) => {
                emit_cmp(f, vstack, *tag, CmpOp::Gt)?;
                ranges.push(tagged.range);
            }
            Op::Ge(tag) => {
                emit_cmp(f, vstack, *tag, CmpOp::Ge)?;
                ranges.push(tagged.range);
            }
            Op::If {
                result_ty,
                then_body,
                else_body,
            } => {
                emit_if(
                    f,
                    ranges,
                    vstack,
                    *result_ty,
                    then_body,
                    else_body,
                    func,
                    const_pool,
                    ectx,
                    tagged.range,
                )?;
            }
            Op::Return => {
                // Wasm encodes "return at end" as a bare `end` —
                // the function's last expression on the stack is
                // the result. Phase 2.b pushes `bytes_written`
                // below; the actual `End` is emitted at the very
                // bottom of this function.
            }
            Op::AllocRootRecord { record_local_idx } => {
                // Root record sits at out_ptr+0. Stash 0 into the
                // record-base local so subsequent
                // StoreFieldAtRecord ops uniformly compute
                // `out_ptr + base + offset`.
                let wasm_local = ectx.record_local_base + record_local_idx;
                f.instruction(&Instruction::I32Const(0));
                ranges.push(tagged.range);
                f.instruction(&Instruction::LocalSet(wasm_local));
                ranges.push(tagged.range);
            }
            Op::AllocSubRecord {
                record_local_idx,
                root_size,
                root_align,
            } => {
                // Align `$tail_cursor` up to `root_align`, bounds-
                // check against `out_cap`, store the aligned cursor
                // into the record-base local, then bump the cursor
                // by `root_size`.
                let wasm_local = ectx.record_local_base + record_local_idx;
                emit_align_tail_cursor(f, ranges, *root_align, tagged.range);
                emit_tail_bounds_check(f, ranges, *root_size, tagged.range);
                // local_record = $tail_cursor
                f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
                ranges.push(tagged.range);
                f.instruction(&Instruction::LocalSet(wasm_local));
                ranges.push(tagged.range);
                // $tail_cursor += root_size
                f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
                ranges.push(tagged.range);
                f.instruction(&Instruction::I32Const(*root_size as i32));
                ranges.push(tagged.range);
                f.instruction(&Instruction::I32Add);
                ranges.push(tagged.range);
                f.instruction(&Instruction::LocalSet(TAIL_CURSOR_LOCAL_INDEX));
                ranges.push(tagged.range);
            }
            Op::PushRecordBase { record_local_idx } => {
                let wasm_local = ectx.record_local_base + record_local_idx;
                f.instruction(&Instruction::LocalGet(wasm_local));
                ranges.push(tagged.range);
                vstack.push(IrType::I32);
            }
            Op::StoreFieldAtRecord {
                record_local_idx,
                offset,
                ty,
            } => {
                let popped = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
                if popped.wasm_slot() != ty.wasm_slot() {
                    return Err(CodegenError::MixedNumericTypes);
                }
                emit_store_field_at_record(
                    f,
                    ranges,
                    ectx.record_local_base + record_local_idx,
                    *offset,
                    *ty,
                    tagged.range,
                )?;
            }
            Op::EmitTailRecordFromAbsoluteAddr { ty } => {
                let popped = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
                if popped.wasm_slot() != IrType::I32 {
                    return Err(CodegenError::MixedNumericTypes);
                }
                emit_tail_record_from_absolute(f, ranges, *ty, tagged.range)?;
                // Pushes the buffer-relative offset of the
                // newly-written record.
                vstack.push(IrType::I32);
            }
            Op::Call {
                fn_index,
                arg_count,
                param_tys,
                ret_ty,
            } => {
                // Sanity: declared param_tys count must match the op's
                // arg_count. Lowering keeps these in sync, but a hand-
                // built IR could disagree.
                let param_tys_len = u32::try_from(param_tys.len()).unwrap_or(u32::MAX);
                if param_tys_len != *arg_count {
                    return Err(CodegenError::CallTypeMismatch {
                        fn_index: *fn_index,
                        arg_count: *arg_count,
                        param_tys_len,
                    });
                }
                // Pop arguments off the vstack in reverse declaration
                // order — the last-pushed argument sits on top — and
                // verify each one occupies the callee's matching
                // wasm slot.
                if vstack.len() < param_tys.len() {
                    return Err(CodegenError::CallTypeMismatch {
                        fn_index: *fn_index,
                        arg_count: *arg_count,
                        param_tys_len,
                    });
                }
                for ty in param_tys.iter().rev() {
                    let popped = vstack.pop().ok_or(CodegenError::CallTypeMismatch {
                        fn_index: *fn_index,
                        arg_count: *arg_count,
                        param_tys_len,
                    })?;
                    if popped.wasm_slot() != ty.wasm_slot() {
                        return Err(CodegenError::CallTypeMismatch {
                            fn_index: *fn_index,
                            arg_count: *arg_count,
                            param_tys_len,
                        });
                    }
                }
                f.instruction(&Instruction::Call(*fn_index));
                ranges.push(tagged.range);
                vstack.push(*ret_ty);
            }
            Op::ReadStringLen => {
                let popped = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
                // Receiver must occupy the i32 slot (String / ListInt
                // pointer). Both record layouts open with a `u32 LE`
                // length prefix, so the same op serves both.
                if popped.wasm_slot() != IrType::I32 {
                    return Err(CodegenError::MixedNumericTypes);
                }
                // `i32.load offset=0 align=2` reads the u32 LE length
                // prefix; `i64.extend_i32_u` widens to the I64 return
                // slot the `length` / `list_int_length` stdlib bodies
                // commit to.
                f.instruction(&Instruction::I32Load(MemArg {
                    offset: 0,
                    align: 2,
                    memory_index: 0,
                }));
                ranges.push(tagged.range);
                f.instruction(&Instruction::I64ExtendI32U);
                ranges.push(tagged.range);
                vstack.push(IrType::I64);
            }
            Op::LoadFieldAtAbsolute { offset, ty } => {
                // Pop the absolute base address, then emit a load with
                // `offset = N` baked into the memarg. The base must
                // occupy an i32 slot; mismatches surface as
                // `MixedNumericTypes` so a hand-built IR with the
                // wrong receiver shape fails deterministically.
                let popped = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
                if popped.wasm_slot() != IrType::I32 {
                    return Err(CodegenError::MixedNumericTypes);
                }
                emit_load_field_at_absolute(f, ranges, *offset, *ty, tagged.range)?;
                vstack.push(load_field_stack_type(*ty));
            }
            Op::LoadSchemaPtr { offset } => {
                // Identical wasm shape to `LoadStringPtr` /
                // `LoadListIntPtr`: read the 4-byte buffer-relative
                // pointer at `in_ptr + offset`, add `in_ptr`, push the
                // resulting absolute address. Tagged separately at the
                // IR level so the lowering pass can carry a schema
                // brand for method dispatch.
                emit_load_absolute_pointer(f, ranges, *offset, tagged.range);
                vstack.push(IrType::I32);
            }
            Op::Select { ty } => {
                // Wasm `select t` pops `[val_true, val_false, cond_i32]`
                // and pushes one of the two values. The IR pins both
                // operand slots to the same `ty` so we re-derive the
                // wasm slot expectations from a single tag.
                //
                // Pop order on the vstack is `cond` first (top of
                // stack), then `val_false`, then `val_true`. We
                // validate both operands share the declared slot and
                // the condition occupies an i32 slot before emitting
                // the typed select.
                let cond = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
                if cond.wasm_slot() != IrType::I32 {
                    return Err(CodegenError::MixedNumericTypes);
                }
                let val_false = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
                let val_true = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
                if val_true.wasm_slot() != ty.wasm_slot() || val_false.wasm_slot() != ty.wasm_slot()
                {
                    return Err(CodegenError::MixedNumericTypes);
                }
                f.instruction(&Instruction::TypedSelect(ir_to_val_type(ty)));
                ranges.push(tagged.range);
                vstack.push(*ty);
            }
        }
    }
    Ok(())
}

/// Emit an `if (result <valtype>) <then> else <else> end` block.
///
/// Frame discipline:
///   - Pops one i32 condition from the vstack before emitting the
///     `If`. Lowering already restricted condition type to `Bool`,
///     so we check the slot rather than the exact tag.
///   - Inside each branch we re-emit the matching ops with a fresh
///     view of the vstack so frame-leak (e.g. an inner branch
///     pushing two values where one was promised) surfaces as a
///     `MixedNumericTypes` rather than corrupting the outer frame.
///   - Both branches must end with exactly one value of `result_ty`
///     on top of their local vstack; we then merge them into a
///     single push on the outer vstack.
#[allow(clippy::too_many_arguments)]
fn emit_if(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    vstack: &mut Vec<IrType>,
    result_ty: IrType,
    then_body: &[relon_ir::TaggedOp],
    else_body: &[relon_ir::TaggedOp],
    func: &relon_ir::Func,
    const_pool: &ConstPool,
    ectx: &mut EmitCtx,
    range: TokenRange,
) -> Result<(), CodegenError> {
    let cond = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
    if cond.wasm_slot() != IrType::I32 {
        // Condition must occupy an i32 slot (Bool is the canonical
        // form; the codegen will accept any tag that materialises as
        // an i32 on the wasm stack since the wasm `if` only inspects
        // a single i32).
        return Err(CodegenError::MixedNumericTypes);
    }
    let block_type = BlockType::Result(ir_to_val_type(&result_ty));
    f.instruction(&Instruction::If(block_type));
    ranges.push(range);

    // `then` arm.
    let mut then_stack: Vec<IrType> = Vec::new();
    emit_op_seq(
        f,
        ranges,
        &mut then_stack,
        then_body,
        func,
        const_pool,
        ectx,
    )?;
    let then_top = match then_stack.pop() {
        Some(t) => t,
        None => return Err(CodegenError::MixedNumericTypes),
    };
    if !then_stack.is_empty() {
        return Err(CodegenError::MixedNumericTypes);
    }
    if then_top.wasm_slot() != result_ty.wasm_slot() {
        return Err(CodegenError::IfBranchTypeMismatch {
            then_ty: then_top,
            else_ty: result_ty,
        });
    }

    f.instruction(&Instruction::Else);
    ranges.push(range);

    // `else` arm.
    let mut else_stack: Vec<IrType> = Vec::new();
    emit_op_seq(
        f,
        ranges,
        &mut else_stack,
        else_body,
        func,
        const_pool,
        ectx,
    )?;
    let else_top = match else_stack.pop() {
        Some(t) => t,
        None => return Err(CodegenError::MixedNumericTypes),
    };
    if !else_stack.is_empty() {
        return Err(CodegenError::MixedNumericTypes);
    }
    if else_top.wasm_slot() != result_ty.wasm_slot() {
        return Err(CodegenError::IfBranchTypeMismatch {
            then_ty: result_ty,
            else_ty: else_top,
        });
    }

    f.instruction(&Instruction::End);
    ranges.push(range);
    vstack.push(result_ty);
    Ok(())
}

/// Emit the wasm op sequence to load a pointer-indirect field from
/// the `in_buf` and lift it to an **absolute** linear-memory address.
///
/// The host-side `BufferBuilder` writes the pointer slot as a
/// buffer-relative offset (the byte position of the tail record
/// counted from `in_ptr`). The wasm representation we hand to
/// downstream ops (e.g. a `StoreField` echoing the value into
/// `out_buf`) is an absolute address so it can be consumed uniformly
/// alongside `ConstString` addresses.
///
/// Emitted sequence:
///   `local.get $in_ptr; i32.load offset=N align=2; local.get $in_ptr; i32.add`
fn emit_load_absolute_pointer(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    offset: u32,
    range: TokenRange,
) {
    f.instruction(&Instruction::LocalGet(WASM_LOCAL_IN_PTR));
    ranges.push(range);
    f.instruction(&Instruction::I32Load(MemArg {
        offset: offset as u64,
        // 4-byte alignment for u32 (log2 = 2).
        align: 2,
        memory_index: 0,
    }));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(WASM_LOCAL_IN_PTR));
    ranges.push(range);
    f.instruction(&Instruction::I32Add);
    ranges.push(range);
}

/// Wasm-side stack representation of a loaded field. `Int` / `Float`
/// load as `i64` / `f64`; `Bool` / `Null` / `String` / `ListInt`
/// load as `i32` (a byte tag or a tail-area pointer).
fn load_field_stack_type(ty: IrType) -> IrType {
    match ty {
        IrType::I64 | IrType::F64 => ty,
        IrType::Bool | IrType::Null | IrType::String | IrType::ListInt => IrType::I32,
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
        IrType::I32 | IrType::String | IrType::ListInt => {
            // `String` / `ListInt` LoadField is rare — lowering
            // normally emits the explicit `LoadStringPtr` /
            // `LoadListIntPtr` ops because the IR-level tag carries
            // forward more diagnostic info. But a hand-built IR
            // using `LoadField { ty: String }` falls back to the
            // same 4-byte `i32.load`, so we keep the path open.
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

/// Emit a load whose base address is already on top of the operand
/// stack (an absolute wasm-memory address). Used by
/// [`Op::LoadFieldAtAbsolute`] for schema-method `self.field` access
/// and chained-segment reads. The stack-top base is consumed by the
/// emitted load instruction; no `local.get $in_ptr` is added — that's
/// the caller's responsibility when sourcing from the in_buf.
fn emit_load_field_at_absolute(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    offset: u32,
    ty: IrType,
    range: TokenRange,
) -> Result<(), CodegenError> {
    match ty {
        IrType::Null => {
            // Null reads as constant zero — drop the address operand
            // first since the wasm `i32.const` is independent.
            f.instruction(&Instruction::Drop);
            ranges.push(range);
            f.instruction(&Instruction::I32Const(0));
            ranges.push(range);
        }
        IrType::Bool => {
            f.instruction(&Instruction::I32Load8U(MemArg {
                offset: offset as u64,
                align: 0,
                memory_index: 0,
            }));
            ranges.push(range);
        }
        IrType::I64 => {
            f.instruction(&Instruction::I64Load(MemArg {
                offset: offset as u64,
                align: 3,
                memory_index: 0,
            }));
            ranges.push(range);
        }
        IrType::F64 => {
            f.instruction(&Instruction::F64Load(MemArg {
                offset: offset as u64,
                align: 3,
                memory_index: 0,
            }));
            ranges.push(range);
        }
        IrType::I32 | IrType::String | IrType::ListInt => {
            f.instruction(&Instruction::I32Load(MemArg {
                offset: offset as u64,
                align: 2,
                memory_index: 0,
            }));
            ranges.push(range);
        }
    }
    Ok(())
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
        // Pointer-indirect stores route through
        // `emit_store_pointer_indirect`; falling through here means
        // a hand-built IR called this helper directly with a pointer
        // type — refuse rather than emit a half-formed sequence.
        IrType::String | IrType::ListInt => {
            return Err(CodegenError::UnsupportedStoreFieldType { ty });
        }
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

/// Emit the wasm op sequence for a pointer-indirect store of a
/// `String` / `List<Int>` value into the `out_buf` tail area.
///
/// At entry the top of the wasm stack is an **absolute** linear-
/// memory address of a `[len:u32 LE][payload]` record (either a
/// const data-section address or an in_buf record lifted to
/// absolute by [`emit_load_absolute_pointer`]).
///
/// Emit shape (using `memory.copy` from the bulk-memory proposal,
/// which wasmtime enables by default):
///
/// ```text
/// ;; stack: [src_addr]
/// local.set $memcpy_src
/// local.get $memcpy_src
/// i32.load align=2                           ;; payload length / element count
/// ;; record_size = 4 + payload_len (String)
/// ;;           or = 8 + 8*element_count (List<Int>)
/// <compute record size>
/// local.set $memcpy_len
///
/// ;; align $tail_cursor to 4 (String) or 8 (List<Int>) before the write
/// <align $tail_cursor>
///
/// ;; bounds: tail_cursor + record_size > out_cap → trap
/// local.get $tail_cursor
/// local.get $memcpy_len
/// i32.add
/// local.get $out_cap
/// i32.gt_u
/// if; unreachable; end
///
/// ;; memcpy(out_ptr + tail_cursor, src, record_size)
/// local.get $out_ptr
/// local.get $tail_cursor
/// i32.add
/// local.get $memcpy_src
/// local.get $memcpy_len
/// memory.copy 0 0
///
/// ;; store fixed-area pointer slot: out_buf[N] = tail_cursor
/// local.get $out_ptr
/// local.get $tail_cursor
/// i32.store offset=N align=2
///
/// ;; bump tail cursor: tail_cursor += record_size
/// local.get $tail_cursor
/// local.get $memcpy_len
/// i32.add
/// local.set $tail_cursor
/// ```
fn emit_store_pointer_indirect(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    fixed_offset: u32,
    ty: IrType,
    range: TokenRange,
) -> Result<(), CodegenError> {
    // Spill the src address into the local — we'll need it twice
    // (once to read the length prefix, once for memory.copy).
    f.instruction(&Instruction::LocalSet(MEMCPY_SRC_LOCAL_INDEX));
    ranges.push(range);

    // Load the length prefix (u32 LE) at src+0.
    f.instruction(&Instruction::LocalGet(MEMCPY_SRC_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    }));
    ranges.push(range);

    // Compute record_size from the loaded length / element count.
    match ty {
        IrType::String => {
            // record_size = payload_len + 4
            f.instruction(&Instruction::I32Const(4));
            ranges.push(range);
            f.instruction(&Instruction::I32Add);
            ranges.push(range);
        }
        IrType::ListInt => {
            // record_size = 8 + 8 * element_count
            //            = 8 + (count << 3)
            f.instruction(&Instruction::I32Const(3));
            ranges.push(range);
            f.instruction(&Instruction::I32Shl);
            ranges.push(range);
            f.instruction(&Instruction::I32Const(8));
            ranges.push(range);
            f.instruction(&Instruction::I32Add);
            ranges.push(range);
        }
        _ => return Err(CodegenError::UnsupportedStoreFieldType { ty }),
    }
    f.instruction(&Instruction::LocalSet(MEMCPY_LEN_LOCAL_INDEX));
    ranges.push(range);

    // Align $tail_cursor before writing the record. String needs
    // 4-byte alignment so the len prefix is naturally aligned; List<Int>
    // needs 8-byte alignment so `payload_start = align_up(record_start
    // + 4, 8) = record_start + 8` matches our in-record layout.
    let align_mask: i32 = match ty {
        IrType::String => -4,
        IrType::ListInt => -8,
        _ => -4,
    };
    let align_add: i32 = match ty {
        IrType::String => 3,
        IrType::ListInt => 7,
        _ => 3,
    };
    f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::I32Const(align_add));
    ranges.push(range);
    f.instruction(&Instruction::I32Add);
    ranges.push(range);
    f.instruction(&Instruction::I32Const(align_mask));
    ranges.push(range);
    f.instruction(&Instruction::I32And);
    ranges.push(range);
    f.instruction(&Instruction::LocalSet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);

    // Bounds: tail_cursor + record_size > out_cap → trap.
    f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(MEMCPY_LEN_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::I32Add);
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(WASM_LOCAL_OUT_CAP));
    ranges.push(range);
    f.instruction(&Instruction::I32GtU);
    ranges.push(range);
    f.instruction(&Instruction::If(BlockType::Empty));
    ranges.push(range);
    f.instruction(&Instruction::Unreachable);
    ranges.push(range);
    f.instruction(&Instruction::End);
    ranges.push(range);

    // memory.copy(dst=out_ptr+tail_cursor, src=$memcpy_src, n=$memcpy_len).
    // Bulk-memory proposal; wasmtime keeps it enabled by default. We
    // avoid the byte-by-byte loop because the proposal is broadly
    // supported (Node, browsers, wasmtime) and a single op saves a
    // dozen instructions per record.
    f.instruction(&Instruction::LocalGet(WASM_LOCAL_OUT_PTR));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::I32Add);
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(MEMCPY_SRC_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(MEMCPY_LEN_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::MemoryCopy {
        src_mem: 0,
        dst_mem: 0,
    });
    ranges.push(range);

    // Store the buffer-relative pointer (= tail_cursor) into the
    // fixed-area slot at `fixed_offset`.
    f.instruction(&Instruction::LocalGet(WASM_LOCAL_OUT_PTR));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::I32Store(MemArg {
        offset: fixed_offset as u64,
        align: 2,
        memory_index: 0,
    }));
    ranges.push(range);

    // Bump tail_cursor by record_size for the next pointer-indirect
    // write (Phase 3.b dict literal outputs reuse this slot).
    f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(MEMCPY_LEN_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::I32Add);
    ranges.push(range);
    f.instruction(&Instruction::LocalSet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);

    Ok(())
}

/// Phase 3.b: align `$tail_cursor` up to `align` bytes. Skips the
/// instructions entirely when the alignment is `1` (no padding
/// needed) or `0` (defensive — should never happen for a real
/// schema).
fn emit_align_tail_cursor(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    align: u32,
    range: TokenRange,
) {
    if align <= 1 {
        return;
    }
    // `$tail_cursor = ($tail_cursor + (align - 1)) & ~(align - 1)`.
    // Works for any power-of-two align ≤ 8 — the only values the
    // layout pass emits.
    let mask = !(align as i32 - 1);
    f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::I32Const(align as i32 - 1));
    ranges.push(range);
    f.instruction(&Instruction::I32Add);
    ranges.push(range);
    f.instruction(&Instruction::I32Const(mask));
    ranges.push(range);
    f.instruction(&Instruction::I32And);
    ranges.push(range);
    f.instruction(&Instruction::LocalSet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);
}

/// Phase 3.b: trap when `$tail_cursor + size > $out_cap`. Used by
/// [`Op::AllocSubRecord`] before bumping the cursor.
fn emit_tail_bounds_check(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    size: u32,
    range: TokenRange,
) {
    f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::I32Const(size as i32));
    ranges.push(range);
    f.instruction(&Instruction::I32Add);
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(WASM_LOCAL_OUT_CAP));
    ranges.push(range);
    f.instruction(&Instruction::I32GtU);
    ranges.push(range);
    f.instruction(&Instruction::If(BlockType::Empty));
    ranges.push(range);
    f.instruction(&Instruction::Unreachable);
    ranges.push(range);
    f.instruction(&Instruction::End);
    ranges.push(range);
}

/// Phase 3.b: pop a value (already typed via `ty`) and store it at
/// `out_ptr + $record_local + offset`. Mirrors [`emit_store_field`]
/// but the destination address is record-base relative.
fn emit_store_field_at_record(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    record_wasm_local: u32,
    offset: u32,
    ty: IrType,
    range: TokenRange,
) -> Result<(), CodegenError> {
    // Sequence the operands so the wasm stack ends up as
    // `[dest_addr, value]` for the store instruction. The value is
    // already on top of the stack at entry; we spill it, push the
    // dest address (out_ptr + record_local + offset), then push the
    // value back.
    //
    // Inline scalar stores (`I64` / `F64` / `Bool` / `Null`) use the
    // STORE_TMP local (typed to the value's slot). Pointer-indirect
    // store (`String` / `ListInt`) uses the i32-typed
    // RECORD_STORE_TMP since they all ride i32 wasm slots.
    match ty {
        IrType::I64 => {
            f.instruction(&Instruction::LocalSet(STORE_TMP_LOCAL_INDEX));
            ranges.push(range);
            emit_record_dest_addr(f, ranges, record_wasm_local, offset, range);
            f.instruction(&Instruction::LocalGet(STORE_TMP_LOCAL_INDEX));
            ranges.push(range);
            f.instruction(&Instruction::I64Store(MemArg {
                offset: 0,
                align: 3,
                memory_index: 0,
            }));
            ranges.push(range);
        }
        IrType::F64 => {
            f.instruction(&Instruction::LocalSet(STORE_TMP_LOCAL_INDEX));
            ranges.push(range);
            emit_record_dest_addr(f, ranges, record_wasm_local, offset, range);
            f.instruction(&Instruction::LocalGet(STORE_TMP_LOCAL_INDEX));
            ranges.push(range);
            f.instruction(&Instruction::F64Store(MemArg {
                offset: 0,
                align: 3,
                memory_index: 0,
            }));
            ranges.push(range);
        }
        IrType::Bool | IrType::Null => {
            f.instruction(&Instruction::LocalSet(RECORD_STORE_TMP_LOCAL_INDEX));
            ranges.push(range);
            emit_record_dest_addr(f, ranges, record_wasm_local, offset, range);
            f.instruction(&Instruction::LocalGet(RECORD_STORE_TMP_LOCAL_INDEX));
            ranges.push(range);
            f.instruction(&Instruction::I32Store8(MemArg {
                offset: 0,
                align: 0,
                memory_index: 0,
            }));
            ranges.push(range);
        }
        IrType::String | IrType::ListInt | IrType::I32 => {
            // Pointer slot — store the i32 (which is either a
            // pointer offset produced by EmitTailRecordFromAbsoluteAddr
            // / PushRecordBase, or an arbitrary i32).
            f.instruction(&Instruction::LocalSet(RECORD_STORE_TMP_LOCAL_INDEX));
            ranges.push(range);
            emit_record_dest_addr(f, ranges, record_wasm_local, offset, range);
            f.instruction(&Instruction::LocalGet(RECORD_STORE_TMP_LOCAL_INDEX));
            ranges.push(range);
            f.instruction(&Instruction::I32Store(MemArg {
                offset: 0,
                align: 2,
                memory_index: 0,
            }));
            ranges.push(range);
        }
    }
    Ok(())
}

/// Push `out_ptr + $record_local + offset` onto the stack as an i32.
/// Helper used by [`emit_store_field_at_record`] so each store can
/// share the same address sequence.
fn emit_record_dest_addr(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    record_wasm_local: u32,
    offset: u32,
    range: TokenRange,
) {
    f.instruction(&Instruction::LocalGet(WASM_LOCAL_OUT_PTR));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(record_wasm_local));
    ranges.push(range);
    f.instruction(&Instruction::I32Add);
    ranges.push(range);
    if offset != 0 {
        f.instruction(&Instruction::I32Const(offset as i32));
        ranges.push(range);
        f.instruction(&Instruction::I32Add);
        ranges.push(range);
    }
}

/// Phase 3.b: pop an absolute address pointing at a
/// `[len:u32 LE][payload]` record, memcpy it into `out_buf` at
/// `$tail_cursor`, bump `$tail_cursor` past the record, and push the
/// buffer-relative offset (= the pre-bump cursor) on the stack.
///
/// Mirrors the in-record alignment expectations
/// [`emit_store_pointer_indirect`] keeps for the simple-return path:
/// `String` records are 4-byte aligned, `List<Int>` records 8-byte
/// aligned (so the `[len:4][pad:4][i64 elements]` payload sits at an
/// 8-byte boundary).
fn emit_tail_record_from_absolute(
    f: &mut Function,
    ranges: &mut Vec<TokenRange>,
    ty: IrType,
    range: TokenRange,
) -> Result<(), CodegenError> {
    // Spill source so we can use it twice.
    f.instruction(&Instruction::LocalSet(MEMCPY_SRC_LOCAL_INDEX));
    ranges.push(range);
    // Load length prefix.
    f.instruction(&Instruction::LocalGet(MEMCPY_SRC_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    }));
    ranges.push(range);
    // Compute record_size from the length / element count.
    match ty {
        IrType::String => {
            f.instruction(&Instruction::I32Const(4));
            ranges.push(range);
            f.instruction(&Instruction::I32Add);
            ranges.push(range);
        }
        IrType::ListInt => {
            // record_size = 8 + 8 * count = 8 + (count << 3)
            f.instruction(&Instruction::I32Const(3));
            ranges.push(range);
            f.instruction(&Instruction::I32Shl);
            ranges.push(range);
            f.instruction(&Instruction::I32Const(8));
            ranges.push(range);
            f.instruction(&Instruction::I32Add);
            ranges.push(range);
        }
        _ => return Err(CodegenError::UnsupportedStoreFieldType { ty }),
    }
    f.instruction(&Instruction::LocalSet(MEMCPY_LEN_LOCAL_INDEX));
    ranges.push(range);

    // Align $tail_cursor before the write. Same alignment rules as
    // the simple-return path: 4 for String, 8 for ListInt.
    let align: u32 = match ty {
        IrType::String => 4,
        IrType::ListInt => 8,
        _ => 4,
    };
    emit_align_tail_cursor(f, ranges, align, range);

    // Bounds: tail_cursor + record_size > out_cap → trap.
    f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(MEMCPY_LEN_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::I32Add);
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(WASM_LOCAL_OUT_CAP));
    ranges.push(range);
    f.instruction(&Instruction::I32GtU);
    ranges.push(range);
    f.instruction(&Instruction::If(BlockType::Empty));
    ranges.push(range);
    f.instruction(&Instruction::Unreachable);
    ranges.push(range);
    f.instruction(&Instruction::End);
    ranges.push(range);

    // Push the pre-bump tail cursor (= the buffer-relative offset
    // of the record about to be written) onto the stack. We grab
    // this BEFORE bumping the cursor and BEFORE calling memcpy.
    f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);
    // Stash that into RECORD_STORE_TMP so we can do the memcpy with
    // a fresh address computation and then reload the offset for the
    // outer code's stack push.
    f.instruction(&Instruction::LocalSet(RECORD_STORE_TMP_LOCAL_INDEX));
    ranges.push(range);

    // memory.copy(dst = out_ptr + tail_cursor, src = $memcpy_src, n = $memcpy_len)
    f.instruction(&Instruction::LocalGet(WASM_LOCAL_OUT_PTR));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::I32Add);
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(MEMCPY_SRC_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(MEMCPY_LEN_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::MemoryCopy {
        src_mem: 0,
        dst_mem: 0,
    });
    ranges.push(range);

    // $tail_cursor += $memcpy_len
    f.instruction(&Instruction::LocalGet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::LocalGet(MEMCPY_LEN_LOCAL_INDEX));
    ranges.push(range);
    f.instruction(&Instruction::I32Add);
    ranges.push(range);
    f.instruction(&Instruction::LocalSet(TAIL_CURSOR_LOCAL_INDEX));
    ranges.push(range);

    // Push the saved pre-bump offset as the result.
    f.instruction(&Instruction::LocalGet(RECORD_STORE_TMP_LOCAL_INDEX));
    ranges.push(range);

    Ok(())
}

/// Wasm value type used for the scratch local in `emit_store_field`.
/// `Int` stores need an i64 slot; `Float` an f64 slot; `Bool` / `Null`
/// an i32 slot. The slot is preallocated by `emit_function_body`
/// based on the first `StoreField` op in the body — see the call site
/// for the single-StoreField assumption rationale.
/// Recursively scan `body` for the first `StoreField` op and return
/// the wasm value type its spill local should have. Returns `None`
/// when the body never stores — `emit_function_body` defaults to
/// `i64` in that case (any wasm valtype keeps the local declaration
/// well-formed; a never-used local has zero runtime cost).
fn find_store_field_local_ty(body: &[relon_ir::TaggedOp]) -> Option<ValType> {
    for tagged in body {
        match &tagged.op {
            Op::StoreField { ty, .. } => return Some(store_field_local_valtype(*ty)),
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                if let Some(t) = find_store_field_local_ty(then_body) {
                    return Some(t);
                }
                if let Some(t) = find_store_field_local_ty(else_body) {
                    return Some(t);
                }
            }
            _ => {}
        }
    }
    None
}

fn store_field_local_valtype(ty: IrType) -> ValType {
    match ty {
        IrType::I64 => ValType::I64,
        IrType::F64 => ValType::F64,
        // String / ListInt would only ever appear here if a later
        // phase started writing variable-length data out of `#main`.
        // Phase 2.c keeps the return surface to Int / Float / Bool
        // / Null, but the arm stays exhaustive for forward compat.
        IrType::I32 | IrType::Bool | IrType::Null | IrType::String | IrType::ListInt => {
            ValType::I32
        }
    }
}

enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
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
        // Arithmetic on I32 / Bool / Null / String / ListInt is not
        // part of the surface — the lowering pass rejects bodies
        // with these tags. A hand-crafted IR landing here gets the
        // same treatment as a mixed-type body.
        (IrType::I32, _)
        | (IrType::Bool, _)
        | (IrType::Null, _)
        | (IrType::String, _)
        | (IrType::ListInt, _) => {
            return Err(CodegenError::MixedNumericTypes);
        }
    };
    f.instruction(&instr);
    vstack.push(tag);
    Ok(())
}

/// Emit one of the six comparison ops (`==`, `!=`, `<`, `<=`, `>`,
/// `>=`). Pops two operands of the tagged type; pushes a `Bool`
/// (occupying an i32 wasm slot).
fn emit_cmp(
    f: &mut Function,
    vstack: &mut Vec<IrType>,
    tag: IrType,
    op: CmpOp,
) -> Result<(), CodegenError> {
    let rhs = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
    let lhs = vstack.pop().ok_or(CodegenError::MixedNumericTypes)?;
    if lhs.wasm_slot() != tag.wasm_slot() || rhs.wasm_slot() != tag.wasm_slot() {
        return Err(CodegenError::MixedNumericTypes);
    }
    let instr = match (tag, &op) {
        // Int comparisons — signed.
        (IrType::I64, CmpOp::Eq) => Instruction::I64Eq,
        (IrType::I64, CmpOp::Ne) => Instruction::I64Ne,
        (IrType::I64, CmpOp::Lt) => Instruction::I64LtS,
        (IrType::I64, CmpOp::Le) => Instruction::I64LeS,
        (IrType::I64, CmpOp::Gt) => Instruction::I64GtS,
        (IrType::I64, CmpOp::Ge) => Instruction::I64GeS,
        // Float comparisons.
        (IrType::F64, CmpOp::Eq) => Instruction::F64Eq,
        (IrType::F64, CmpOp::Ne) => Instruction::F64Ne,
        (IrType::F64, CmpOp::Lt) => Instruction::F64Lt,
        (IrType::F64, CmpOp::Le) => Instruction::F64Le,
        (IrType::F64, CmpOp::Gt) => Instruction::F64Gt,
        (IrType::F64, CmpOp::Ge) => Instruction::F64Ge,
        // Bool equality / inequality via i32 compare. Ordering on
        // Bool is rejected — wasm has no defined `<` between Bool
        // values that matches the surface semantics.
        (IrType::Bool, CmpOp::Eq) => Instruction::I32Eq,
        (IrType::Bool, CmpOp::Ne) => Instruction::I32Ne,
        (IrType::Bool, _) => {
            return Err(CodegenError::InvalidComparisonOperandType { ty: IrType::Bool });
        }
        // `Null == Null` always true / `Null != Null` always false.
        // Pop the two operands that are already on the wasm stack
        // (their values are unused) by emitting `i32.eq` over them —
        // both are zero so the result naturally agrees.
        (IrType::Null, CmpOp::Eq) => Instruction::I32Eq,
        (IrType::Null, CmpOp::Ne) => Instruction::I32Ne,
        (IrType::Null, _) => {
            return Err(CodegenError::InvalidComparisonOperandType { ty: IrType::Null });
        }
        // String / ListInt / I32 comparisons aren't part of the
        // Phase 2.c surface — lowering rejects upstream; we reject
        // here too so hand-built IR can't sneak in pointer compares.
        (IrType::String, _) => {
            return Err(CodegenError::InvalidComparisonOperandType { ty: IrType::String });
        }
        (IrType::ListInt, _) => {
            return Err(CodegenError::InvalidComparisonOperandType {
                ty: IrType::ListInt,
            });
        }
        (IrType::I32, _) => {
            return Err(CodegenError::InvalidComparisonOperandType { ty: IrType::I32 });
        }
    };
    f.instruction(&instr);
    vstack.push(IrType::Bool);
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
        // via `i32.load8_u` / `i32.const`). String / ListInt are
        // tail-area pointers and ride an i32 slot too — they enter
        // the wasm stack via `i32.load offset=N`.
        IrType::Bool | IrType::Null | IrType::String | IrType::ListInt => ValType::I32,
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
