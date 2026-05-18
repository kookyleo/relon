//! IR -> Cranelift IR lowering.
//!
//! v5-beta-1 deliberately narrows the supported IR surface to keep
//! the cranelift pipeline focused on the HelloWorld-tier scenarios:
//!
//! * Integer arithmetic (`Add` / `Sub` / `Mul` / `Div` / `Mod`) on `I64`.
//! * Six comparisons (`Eq` / `Ne` / `Lt` / `Le` / `Gt` / `Ge`).
//! * `ConstI64` / `ConstI32` / `ConstBool` literals plus `Return`.
//! * `LocalGet` and `LetGet` / `LetSet` for parameter / let-binding
//!   access.
//! * `If` for conditional control flow.
//! * `Call` for the narrow stdlib subset hard-wired in `evaluator.rs`
//!   (`length` of a constant String, `abs(Int)`).
//! * `ConstString` + `ReadStringLen` to validate the bounds-check
//!   path against constant String pointers.
//! * `CallNative` + `CheckCap` so the capability gate has an end-to-end
//!   exercise.
//!
//! Everything outside that envelope surfaces as
//! [`crate::CraneliftError::Codegen`] / [`CraneliftError::UnsupportedSignature`]
//! so the auto-tier wrapper can cleanly fall back to the wasm-AOT or
//! tree-walking backend without crashing the host.
//!
//! The lowering is intentionally one-pass and produces typed cranelift
//! values directly; no virtual-stack abstraction is needed because the
//! IR's stack discipline is shallow and well-typed by lowering time.
//! The cranelift verifier catches the few corner cases the lowering
//! pass might still mis-handle (type leaks across branches, etc.).

use std::collections::HashMap;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{
    AbiParam, BlockArg, BlockCall, Function, GlobalValue, Inst, InstBuilder, JumpTableData,
    MemFlags, SigRef, Signature, TrapCode, UserFuncName, Value as CValue,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as CodegenContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{DataDescription, DataId, Linkage, Module as CrModule};

use relon_ir::ir::{IrType, Module as IrModule, Op, TaggedOp};

use crate::error::CraneliftError;
use crate::sandbox::{
    SandboxConfig, TrapKind, STATE_OFFSET_ARENA_BASE, STATE_OFFSET_ARENA_LEN,
    STATE_OFFSET_SCRATCH_BASE, STATE_OFFSET_SCRATCH_CURSOR, STATE_OFFSET_TAIL_CURSOR,
};
use crate::vtable::{VtableSlot, VTABLE_BYTES, VTABLE_SYMBOL};

/// Output of a successful compile: a JIT module plus the entry's
/// function ID so the host can resolve a raw function pointer through
/// `JITModule::get_finalized_function` later.
pub struct CompiledModule {
    pub module: JITModule,
    pub entry_fn_id: cranelift_module::FuncId,
    /// Number of `Int` parameters the entry expects (after the
    /// implicit sandbox-state pointer). Used by the runtime
    /// trampoline to materialise the `extern "C"` invocation.
    pub entry_arity: usize,
    /// Source range of the lowered `#main` directive — used by the
    /// runtime to attach trap diagnostics.
    pub entry_range: relon_parser::TokenRange,
    /// Calling convention shape the host trampoline must match.
    pub entry_shape: EntryShape,
    /// Const-data bytes the entry references through `ConstString` /
    /// `ConstList*`. The host trampoline copies these into the arena
    /// prefix before each invocation; the cranelift code refers to
    /// them through hardcoded `[len:u32 LE][payload]` record
    /// offsets emitted at compile time.
    pub const_data: Vec<u8>,
    /// Stage 5 Phase C.4: per-module closure table. Each entry is the
    /// `FuncId` of a lambda the lowering pass emitted; the host
    /// resolves each id through `get_finalized_function` after JIT
    /// finalize and installs the resulting `Vec<usize>` into the
    /// `SandboxState`. The `Op::CallClosure` lowering reads the host-
    /// fn pointer through that table, indexed by the closure handle's
    /// `fn_table_idx` field.
    pub closure_func_ids: Vec<cranelift_module::FuncId>,
    /// v5-γ stage 2: data symbol holding the `__relon_capability_vtable`
    /// slot array. The JIT pipeline populates it post-finalize via
    /// `JITModule::get_finalized_data(vtable_data_id)`; the
    /// `cranelift-object` pipeline emits the symbol as `Linkage::Export`
    /// so the host's `dlsym` round-trip resolves it after `dlopen`.
    pub vtable_data_id: cranelift_module::DataId,
}

/// Per-module const-pool layout. Maps each IR-level `idx` referenced
/// by `Op::ConstString` / `Op::ConstList*` to its byte offset inside
/// the const-data blob shipped on the [`CompiledModule`].
#[derive(Debug, Default, Clone)]
struct ConstPool {
    /// String pool: `idx -> byte offset within `bytes`.
    string_offsets: HashMap<u32, u32>,
    /// List<Int> pool.
    list_int_offsets: HashMap<u32, u32>,
    /// List<Float> pool.
    list_float_offsets: HashMap<u32, u32>,
    /// List<Bool> pool.
    list_bool_offsets: HashMap<u32, u32>,
    /// Materialised bytes in record order. Cranelift code emits
    /// `i32.const <offset>` so the value at runtime is the buffer-
    /// relative address.
    bytes: Vec<u8>,
    /// Lazily-laid-out Unicode case-fold tables. Each entry is set
    /// when the body references `Op::CaseFoldTableAddr { upper }`.
    case_fold_upper_offset: Option<u32>,
    case_fold_lower_offset: Option<u32>,
    /// Lazily-laid-out combining-mark + whitespace ranges tables.
    combining_marks_offset: Option<u32>,
    whitespace_offset: Option<u32>,
    /// Unicode normalization tables (NFD / NFKD decompositions,
    /// Canonical_Combining_Class, canonical composition pairs).
    decomp_nfd_offset: Option<u32>,
    decomp_nfkd_offset: Option<u32>,
    ccc_offset: Option<u32>,
    composition_offset: Option<u32>,
    /// Full multi-codepoint case-folding tables (UAX #21).
    full_case_fold_upper_offset: Option<u32>,
    full_case_fold_lower_offset: Option<u32>,
    cased_ranges_offset: Option<u32>,
    case_ignorable_ranges_offset: Option<u32>,
    /// Locale-aware Turkish / Azerbaijani override tables.
    turkish_upper_offset: Option<u32>,
    turkish_lower_offset: Option<u32>,
}

impl ConstPool {
    /// Build the pool from a scan of the entry's IR body. Each unique
    /// `idx` ends up with a `[len:u32 LE][payload]` record laid out
    /// in declaration order, aligned to 8 to match the wasm side.
    fn from_module(module: &IrModule) -> Result<Self, CraneliftError> {
        let mut pool = ConstPool::default();
        for func in &module.funcs {
            pool.collect_body(&func.body)?;
        }
        Ok(pool)
    }

    fn collect_body(&mut self, body: &[TaggedOp]) -> Result<(), CraneliftError> {
        for tagged in body {
            self.collect_op(&tagged.op)?;
        }
        Ok(())
    }

    fn collect_op(&mut self, op: &Op) -> Result<(), CraneliftError> {
        match op {
            Op::ConstString { idx, value } => {
                if self.string_offsets.contains_key(idx) {
                    return Ok(());
                }
                self.align_to(4);
                let off = u32::try_from(self.bytes.len())
                    .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
                let len = u32::try_from(value.len()).map_err(|_| {
                    CraneliftError::Codegen("ConstString length exceeds u32 range".into())
                })?;
                self.bytes.extend_from_slice(&len.to_le_bytes());
                self.bytes.extend_from_slice(value.as_bytes());
                self.string_offsets.insert(*idx, off);
            }
            Op::ConstListInt { idx, elements } => {
                if self.list_int_offsets.contains_key(idx) {
                    return Ok(());
                }
                self.align_to(8);
                let off = u32::try_from(self.bytes.len())
                    .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
                let len = u32::try_from(elements.len()).map_err(|_| {
                    CraneliftError::Codegen("ConstListInt length exceeds u32 range".into())
                })?;
                self.bytes.extend_from_slice(&len.to_le_bytes());
                self.bytes.extend_from_slice(&[0u8; 4]); // pad to 8
                for e in elements {
                    self.bytes.extend_from_slice(&e.to_le_bytes());
                }
                self.list_int_offsets.insert(*idx, off);
            }
            Op::ConstListFloat { idx, elements } => {
                if self.list_float_offsets.contains_key(idx) {
                    return Ok(());
                }
                self.align_to(8);
                let off = u32::try_from(self.bytes.len())
                    .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
                let len = u32::try_from(elements.len()).map_err(|_| {
                    CraneliftError::Codegen("ConstListFloat length exceeds u32 range".into())
                })?;
                self.bytes.extend_from_slice(&len.to_le_bytes());
                self.bytes.extend_from_slice(&[0u8; 4]); // pad to 8
                for e in elements {
                    self.bytes.extend_from_slice(&e.to_le_bytes());
                }
                self.list_float_offsets.insert(*idx, off);
            }
            Op::ConstListBool { idx, elements } => {
                if self.list_bool_offsets.contains_key(idx) {
                    return Ok(());
                }
                self.align_to(4);
                let off = u32::try_from(self.bytes.len())
                    .map_err(|_| CraneliftError::Codegen("const pool exceeds u32 range".into()))?;
                let len = u32::try_from(elements.len()).map_err(|_| {
                    CraneliftError::Codegen("ConstListBool length exceeds u32 range".into())
                })?;
                self.bytes.extend_from_slice(&len.to_le_bytes());
                for e in elements {
                    self.bytes.push(if *e { 1 } else { 0 });
                }
                self.list_bool_offsets.insert(*idx, off);
            }
            Op::CaseFoldTableAddr { upper } => {
                let slot = if *upper {
                    &mut self.case_fold_upper_offset
                } else {
                    &mut self.case_fold_lower_offset
                };
                if slot.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let table: &[(u32, u32)] = if *upper {
                        relon_ir::case_folding::simple_upper_folding()
                    } else {
                        relon_ir::case_folding::simple_lower_folding()
                    };
                    let bytes = relon_ir::case_folding::encode_table_bytes(table);
                    self.bytes.extend_from_slice(&bytes);
                    if *upper {
                        self.case_fold_upper_offset = Some(off);
                    } else {
                        self.case_fold_lower_offset = Some(off);
                    }
                }
            }
            Op::CombiningMarkRangesAddr => {
                if self.combining_marks_offset.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let table = relon_ir::combining_marks::combining_mark_ranges();
                    let bytes = relon_ir::combining_marks::encode_ranges_bytes(table);
                    self.bytes.extend_from_slice(&bytes);
                    self.combining_marks_offset = Some(off);
                }
            }
            Op::WhitespaceRangesAddr => {
                if self.whitespace_offset.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let table = relon_ir::whitespace::non_ascii_whitespace_ranges();
                    let bytes = relon_ir::whitespace::encode_ranges_bytes(table);
                    self.bytes.extend_from_slice(&bytes);
                    self.whitespace_offset = Some(off);
                }
            }
            Op::DecompTableAddr { compatibility } => {
                let slot = if *compatibility {
                    &mut self.decomp_nfkd_offset
                } else {
                    &mut self.decomp_nfd_offset
                };
                if slot.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let (index, payload) = if *compatibility {
                        (
                            relon_ir::normalization_data::NFKD_INDEX,
                            relon_ir::normalization_data::NFKD_POOL,
                        )
                    } else {
                        (
                            relon_ir::normalization_data::NFD_INDEX,
                            relon_ir::normalization_data::NFD_POOL,
                        )
                    };
                    let bytes = relon_ir::normalization::encode_decomp_table_bytes(index, payload);
                    self.bytes.extend_from_slice(&bytes);
                    if *compatibility {
                        self.decomp_nfkd_offset = Some(off);
                    } else {
                        self.decomp_nfd_offset = Some(off);
                    }
                }
            }
            Op::CccTableAddr => {
                if self.ccc_offset.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let bytes = relon_ir::normalization::encode_ccc_table_bytes(
                        relon_ir::normalization_data::CCC_TABLE,
                    );
                    self.bytes.extend_from_slice(&bytes);
                    self.ccc_offset = Some(off);
                }
            }
            Op::CompositionTableAddr => {
                if self.composition_offset.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let bytes = relon_ir::normalization::encode_composition_table_bytes(
                        relon_ir::normalization_data::COMPOSITION_PAIRS,
                    );
                    self.bytes.extend_from_slice(&bytes);
                    self.composition_offset = Some(off);
                }
            }
            Op::FullCaseFoldTableAddr { upper } => {
                let slot = if *upper {
                    &mut self.full_case_fold_upper_offset
                } else {
                    &mut self.full_case_fold_lower_offset
                };
                if slot.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let table = if *upper {
                        relon_ir::full_case_folding::full_upper_folding()
                    } else {
                        relon_ir::full_case_folding::full_lower_folding()
                    };
                    let bytes = relon_ir::full_case_folding::encode_full_table_bytes(table);
                    self.bytes.extend_from_slice(&bytes);
                    if *upper {
                        self.full_case_fold_upper_offset = Some(off);
                    } else {
                        self.full_case_fold_lower_offset = Some(off);
                    }
                }
            }
            Op::CasedRangesAddr => {
                if self.cased_ranges_offset.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let table = relon_ir::full_case_folding::cased_ranges();
                    let bytes = relon_ir::full_case_folding::encode_ranges_bytes(table);
                    self.bytes.extend_from_slice(&bytes);
                    self.cased_ranges_offset = Some(off);
                }
            }
            Op::CaseIgnorableRangesAddr => {
                if self.case_ignorable_ranges_offset.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let table = relon_ir::full_case_folding::case_ignorable_ranges();
                    let bytes = relon_ir::full_case_folding::encode_ranges_bytes(table);
                    self.bytes.extend_from_slice(&bytes);
                    self.case_ignorable_ranges_offset = Some(off);
                }
            }
            Op::TurkishCaseFoldTableAddr { upper } => {
                let slot = if *upper {
                    &mut self.turkish_upper_offset
                } else {
                    &mut self.turkish_lower_offset
                };
                if slot.is_none() {
                    self.align_to(4);
                    let off = u32::try_from(self.bytes.len()).map_err(|_| {
                        CraneliftError::Codegen("const pool exceeds u32 range".into())
                    })?;
                    let table = if *upper {
                        relon_ir::full_case_folding::turkish_upper_folding()
                    } else {
                        relon_ir::full_case_folding::turkish_lower_folding()
                    };
                    let bytes = relon_ir::full_case_folding::encode_simple_view_bytes(table);
                    self.bytes.extend_from_slice(&bytes);
                    if *upper {
                        self.turkish_upper_offset = Some(off);
                    } else {
                        self.turkish_lower_offset = Some(off);
                    }
                }
            }
            // Recurse into structured bodies so nested ConstStrings
            // (e.g. inside If arms or Block / Loop bodies) get
            // picked up too.
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                self.collect_body(then_body)?;
                self.collect_body(else_body)?;
            }
            Op::Block { body, .. } | Op::Loop { body, .. } => {
                self.collect_body(body)?;
            }
            Op::Call { fn_index, .. } => {
                // The cranelift backend inlines bundled stdlib bodies.
                // Recurse into the callee so its `ConstString` /
                // `CaseFoldTableAddr` references contribute to the
                // pool before the entry body is lowered.
                let stdlib = relon_ir::stdlib::builtin_stdlib();
                if let Some(callee) = stdlib.get(*fn_index as usize) {
                    self.collect_body(&callee.body)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn align_to(&mut self, align: usize) {
        let rem = self.bytes.len() % align;
        if rem != 0 {
            self.bytes.resize(self.bytes.len() + (align - rem), 0);
        }
    }
}

/// How the host trampoline talks to the JIT entry.
///
/// v5-β-2 lands two shapes side-by-side:
///
/// * `LegacyI64Args` — the original v5-β-1 envelope: every IR param
///   is `I64`, return is `I64`. Used by direct-IR callers and the
///   existing codegen unit tests.
/// * `BufferProtocol` — matches the wasm-AOT `run_main` signature
///   (`fn run_main(in_ptr: i32, in_len: i32, out_ptr: i32, out_cap:
///   i32, caps: i64) -> i32`). Selected when the IR's entry
///   parameters match `[I32, I32, I32, I32, I64]` — the canonical
///   shape `lower_workspace_single` emits for every user source.
///
/// Selecting the shape from the IR rather than a separate flag keeps
/// the API surface narrow: the lowering pass is the source of truth
/// on whether the body speaks buffer protocol or raw i64s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryShape {
    /// Legacy: `(*state, i64...) -> i64`. v5-β-1 shape.
    LegacyI64Args,
    /// Buffer protocol: `(*state, i32 in_ptr, i32 in_len, i32 out_ptr,
    /// i32 out_cap, i64 caps) -> i32`. v5-β-2 shape that matches the
    /// wasm-AOT side. Loads + stores against the in/out buffer go
    /// through the `arena_base + buf_ptr + offset` formula.
    BufferProtocol,
}

/// IR param signature that triggers [`EntryShape::BufferProtocol`].
/// Mirrors the locals layout `lower_workspace_single` synthesises for
/// every user `#main` source.
fn is_buffer_protocol_signature(params: &[IrType], ret: IrType) -> bool {
    matches!(
        params,
        [
            IrType::I32,
            IrType::I32,
            IrType::I32,
            IrType::I32,
            IrType::I64
        ]
    ) && matches!(ret, IrType::I32)
}

/// Trap codes the cranelift lowering emits via `trap` /
/// `trapnz` / `trapz`. Aligned with [`TrapKind`] so the host
/// translates without a translation table.
///
/// v5-beta-1 uses cranelift's intrinsic `trap` instruction only for
/// guaranteed-fatal paths (Unreachable). Every guard reachable by
/// the lowered code (divide-by-zero, bounds, capability, resource)
/// routes through the `raise_trap` host helper + early-return
/// sequence instead, because:
///
/// 1. `cranelift_codegen::ir::trap` emits a `ud2` (SIGILL) on x86
///    Linux which Rust's panic runtime cannot intercept through
///    `catch_unwind`. Routing the trap through a host helper lets
///    us record the trap code in `SandboxState::trap_code` and
///    return a sentinel zero, which the trampoline interprets as
///    "trap fired — translate via the recorded code".
/// 2. Real `sigsetjmp` support is on the v5-beta-2 roadmap; until
///    then this is the cleanest path that preserves the typed
///    `RuntimeError` surface on every supported target.
#[allow(dead_code)]
fn trap_code(kind: TrapKind) -> TrapCode {
    TrapCode::user(kind as u8).expect("TrapKind discriminant is non-zero")
}

/// Build a cranelift JIT module and lower the IR's entry function
/// into it. v5-beta-1 only emits one function (the `#main` entry);
/// auxiliary stdlib bodies the IR references are lowered as inline
/// helpers via the `Call` path.
#[cfg(test)]
pub fn compile_module(
    ir: &IrModule,
    sandbox: &SandboxConfig,
) -> Result<CompiledModule, CraneliftError> {
    compile_module_with(ir, sandbox, /* return_root_size= */ 0)
}

/// Same as [`compile_module`], but with an explicit `return_root_size`
/// hint. The hint is consumed by the buffer-protocol epilogue when the
/// body emits no pointer-indirect stores (in that case the JIT returns
/// `return_root_size` as `bytes_written` so the host trampoline reads
/// the full fixed-area record). Callers that don't have schema
/// metadata pass `0`; the trampoline already reads `max(bw,
/// return_root_size)` so a zero hint only affects pointer-indirect-
/// returning bodies, which the from_ir_direct path doesn't use.
pub fn compile_module_with(
    ir: &IrModule,
    sandbox: &SandboxConfig,
    return_root_size: u32,
) -> Result<CompiledModule, CraneliftError> {
    let entry_idx = ir
        .entry_func_index
        .ok_or_else(|| CraneliftError::Codegen("module has no entry function".into()))?;
    let entry = &ir.funcs[entry_idx];

    // Scan the entry body for ConstString / ConstList* ops and build
    // the per-module const-data pool. We pass the resolved
    // `idx -> offset` map into the Codegen so `ConstString { idx }`
    // can lower to a plain `iconst(I32, offset)`. The const-data
    // bytes themselves ride along on `CompiledModule.const_data` —
    // the host trampoline copies them into the arena prefix before
    // each invocation.
    let const_pool = ConstPool::from_module(ir)?;

    // Detect the entry shape. v5-β-2 supports two:
    //   - Legacy `(I64, ..., I64) -> I64` — direct-IR test path.
    //   - Buffer-protocol `(I32, I32, I32, I32, I64) -> I32` — what
    //     `lower_workspace_single` synthesises for every user source.
    // Anything else falls back to the legacy-shape gate and surfaces
    // as `UnsupportedSignature` so the host can pick a different
    // backend.
    let entry_shape = if is_buffer_protocol_signature(&entry.params, entry.ret) {
        EntryShape::BufferProtocol
    } else {
        // Legacy validation: every param must be I64, return must be
        // I64.
        for (i, param) in entry.params.iter().enumerate() {
            if !matches!(param, IrType::I64) {
                return Err(CraneliftError::UnsupportedSignature(format!(
                    "cranelift-native: param #{i} is {param:?} (expected I64 or buffer-protocol shape)"
                )));
            }
        }
        if !matches!(entry.ret, IrType::I64) {
            return Err(CraneliftError::UnsupportedSignature(format!(
                "cranelift-native: return is {:?} (expected I64 or buffer-protocol I32)",
                entry.ret
            )));
        }
        EntryShape::LegacyI64Args
    };

    // Cranelift ISA + flag setup. We pin `is_pic = false` because the
    // JIT loads code into heap-allocated executable pages and never
    // links via the system dynamic loader; PIC would cost an extra
    // `mov` per global access without buying anything.
    let mut flag_builder = settings::builder();
    flag_builder
        .set("is_pic", "false")
        .map_err(|e| CraneliftError::JitSetup(format!("is_pic flag: {e}")))?;
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| CraneliftError::JitSetup(format!("opt_level flag: {e}")))?;
    // Enable verifier in debug builds so accidentally malformed IR
    // surfaces with a useful message instead of producing
    // miscompiled code that segfaults at run time.
    #[cfg(debug_assertions)]
    flag_builder
        .set("enable_verifier", "true")
        .map_err(|e| CraneliftError::JitSetup(format!("enable_verifier flag: {e}")))?;
    let flags = settings::Flags::new(flag_builder);

    let isa_builder = cranelift_native::builder()
        .map_err(|e| CraneliftError::HostTarget(format!("cranelift-native: {e}")))?;
    let isa = isa_builder
        .finish(flags)
        .map_err(|e| CraneliftError::JitSetup(format!("isa finish: {e}")))?;

    // Build a JIT module with the default symbol set. v5-γ stage 2:
    // we no longer register host helper symbols by address here.
    // Instead, every helper call indirects through the
    // `__relon_capability_vtable` data symbol (see crate::vtable); the
    // post-finalize step (in `evaluator.rs`) writes the live host fn
    // pointers into the table.
    //
    // v6-γ M2/M3: in addition we pre-register the four trace JIT
    // runtime helpers (`__relon_trace_save_deopt`,
    // `__relon_trace_resolve_call`, `__relon_trace_inline_cache_lookup`
    // and the codegen-native-side `__relon_jump_to_recorder`) so that
    // (a) HotCounter prologues injected into entry functions can call
    // into the recorder helper and (b) JIT-installed trace fns can
    // call the trace runtime helpers without a separate symbol
    // resolution step.
    let mut jit_builder =
        JITBuilder::with_isa(isa.clone(), cranelift_module::default_libcall_names());
    crate::trace_install::register_trace_runtime_symbols(&mut jit_builder);
    let mut module = JITModule::new(jit_builder);

    let LoweredArtifacts {
        entry_fn_id,
        vtable_data_id,
        closure_func_ids,
    } = lower_module_into(
        &mut module,
        ir,
        entry,
        entry_shape,
        sandbox,
        return_root_size,
        &const_pool,
    )?;

    module
        .finalize_definitions()
        .map_err(|e| CraneliftError::ModuleDefine(format!("finalize: {e}")))?;

    Ok(CompiledModule {
        module,
        entry_fn_id,
        entry_arity: entry.params.len(),
        entry_range: entry.range,
        entry_shape,
        const_data: const_pool.bytes,
        closure_func_ids,
        vtable_data_id,
    })
}

/// Output of [`compile_module_to_object_bytes`].
pub struct ObjectArtifact {
    /// ET_REL ELF bytes ready for `relon-object-link::link_to_dyn`.
    pub et_rel_bytes: Vec<u8>,
    /// Entry shape detected from the IR — the loader uses this to
    /// pick the right calling-convention shim.
    pub entry_shape: EntryShape,
    /// Entry arity (number of IR-declared `#main` params; doesn't
    /// count the implicit sandbox-state pointer).
    pub entry_arity: usize,
    /// Source range of the lowered `#main` directive — used by the
    /// runtime to attach trap diagnostics.
    pub entry_range: relon_parser::TokenRange,
    /// Const-data bytes the entry references through `ConstString` /
    /// `ConstList*`. The host trampoline copies these into the arena
    /// prefix before each invocation (identical to the JIT path).
    pub const_data: Vec<u8>,
    /// Symbol name the host `dlsym`s to find the entry function. The
    /// `lower_module_into` driver always declares this as
    /// `Linkage::Export run_main`.
    pub entry_symbol: &'static str,
    /// Symbol name the host `dlsym`s to find the capability vtable
    /// data slot. The host writes its function pointers into the
    /// vtable after `dlopen` returns.
    pub vtable_symbol: &'static str,
    /// `__closure_<N>` symbol names paired with their original IR
    /// `closure_table` index. The host `dlsym`s each one after
    /// `dlopen` so `SandboxState::closure_table_base` resolves to the
    /// loaded ET_DYN's function pointers.
    pub closure_symbols: Vec<String>,
}

/// v5-γ stage 2: emit the full module via `cranelift-object` for the
/// dlopen-execution cache path. Mirrors [`compile_module_with`] but
/// targets a `cranelift_object::ObjectModule` so the output is an
/// ET_REL ready for `relon-object-link::link_to_dyn`. The dlopen'd
/// ET_DYN imports only the [`crate::vtable::VTABLE_SYMBOL`] data
/// slot; every host helper call indirects through that table.
pub fn compile_module_to_object_bytes(
    ir: &IrModule,
    sandbox: &SandboxConfig,
    return_root_size: u32,
) -> Result<ObjectArtifact, CraneliftError> {
    use cranelift_object::{ObjectBuilder, ObjectModule};

    let entry_idx = ir
        .entry_func_index
        .ok_or_else(|| CraneliftError::Codegen("module has no entry function".into()))?;
    let entry = &ir.funcs[entry_idx];
    let const_pool = ConstPool::from_module(ir)?;

    let entry_shape = if is_buffer_protocol_signature(&entry.params, entry.ret) {
        EntryShape::BufferProtocol
    } else {
        for (i, param) in entry.params.iter().enumerate() {
            if !matches!(param, IrType::I64) {
                return Err(CraneliftError::UnsupportedSignature(format!(
                    "cranelift-native: param #{i} is {param:?} (expected I64 or buffer-protocol shape)"
                )));
            }
        }
        if !matches!(entry.ret, IrType::I64) {
            return Err(CraneliftError::UnsupportedSignature(format!(
                "cranelift-native: return is {:?} (expected I64 or buffer-protocol I32)",
                entry.ret
            )));
        }
        EntryShape::LegacyI64Args
    };

    // `is_pic = true` is required for ELF SHARED objects — the dynamic
    // linker `ld.so` refuses to load non-PIC `.so` files. The verifier
    // stays on in debug builds for the same reason as the JIT path.
    let mut flag_builder = settings::builder();
    flag_builder
        .set("is_pic", "true")
        .map_err(|e| CraneliftError::JitSetup(format!("is_pic flag: {e}")))?;
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| CraneliftError::JitSetup(format!("opt_level flag: {e}")))?;
    #[cfg(debug_assertions)]
    flag_builder
        .set("enable_verifier", "true")
        .map_err(|e| CraneliftError::JitSetup(format!("enable_verifier flag: {e}")))?;
    let flags = settings::Flags::new(flag_builder);

    let isa_builder = cranelift_native::builder()
        .map_err(|e| CraneliftError::HostTarget(format!("cranelift-native: {e}")))?;
    let isa = isa_builder
        .finish(flags)
        .map_err(|e| CraneliftError::JitSetup(format!("isa finish: {e}")))?;

    let obj_builder = ObjectBuilder::new(
        isa,
        "relon-native-cache",
        cranelift_module::default_libcall_names(),
    )
    .map_err(|e| CraneliftError::JitSetup(format!("object builder: {e}")))?;
    let mut module = ObjectModule::new(obj_builder);

    let LoweredArtifacts {
        entry_fn_id: _,
        vtable_data_id: _,
        closure_func_ids,
    } = lower_module_into(
        &mut module,
        ir,
        entry,
        entry_shape,
        sandbox,
        return_root_size,
        &const_pool,
    )?;

    // Collect the closure symbol names so the host can `dlsym` each
    // after `dlopen`. The lambda declarations inside
    // `lower_module_into` use the deterministic `__closure_<N>` name
    // scheme; we just regenerate the list here so the loader doesn't
    // have to parse the ET_DYN's `.dynsym` table.
    let closure_symbols = (0..closure_func_ids.len())
        .map(|i| format!("__closure_{i}"))
        .collect::<Vec<_>>();

    let product = module.finish();
    let et_rel_bytes = product
        .emit()
        .map_err(|e| CraneliftError::Codegen(format!("object emit: {e}")))?;

    Ok(ObjectArtifact {
        et_rel_bytes,
        entry_shape,
        entry_arity: entry.params.len(),
        entry_range: entry.range,
        const_data: const_pool.bytes,
        entry_symbol: "run_main",
        vtable_symbol: VTABLE_SYMBOL,
        closure_symbols,
    })
}

/// Artefacts returned by [`lower_module_into`]. The caller owns the
/// `Module`-flavoured finalize step (`JITModule::finalize_definitions`
/// vs `ObjectModule::finish().emit()`) so this struct only carries
/// the IDs the runtime resolves post-finalize.
struct LoweredArtifacts {
    entry_fn_id: cranelift_module::FuncId,
    vtable_data_id: DataId,
    closure_func_ids: Vec<cranelift_module::FuncId>,
}

/// v5-γ stage 2: shared lowering pass for both `JITModule` (live
/// in-process JIT) and `ObjectModule` (cranelift-object emit ->
/// dlopen). Declares the vtable data symbol, the entry function, and
/// every closure-table lambda; lowers each body via the same
/// [`Codegen`] state machine; defines the cranelift IR into the
/// module. The caller drives the per-backend finalize step.
fn lower_module_into<M: CrModule>(
    module: &mut M,
    ir: &IrModule,
    entry: &relon_ir::ir::Func,
    entry_shape: EntryShape,
    sandbox: &SandboxConfig,
    return_root_size: u32,
    const_pool: &ConstPool,
) -> Result<LoweredArtifacts, CraneliftError> {
    let vtable_data_id = declare_vtable_data(module)?;

    // Pre-compute the three host-fn signatures the codegen indirects
    // through. The signatures match the slot ABI documented in
    // `crate::vtable::VtableSlot`.
    let raise_trap_sig = make_raise_trap_signature(module.target_config().pointer_type());
    let now_sig = make_now_signature(module.target_config().pointer_type());
    let cap_lookup_sig = make_cap_lookup_signature(module.target_config().pointer_type());

    // Build the entry signature. The exact shape depends on
    // `entry_shape`: legacy IR carries `I64...` user args, while the
    // buffer-protocol IR carries the four wasm handshake i32 slots +
    // the i64 capabilities argument.
    let pointer_ty = module.target_config().pointer_type();
    let mut entry_sig = Signature::new(CallConv::SystemV);
    entry_sig.params.push(AbiParam::new(pointer_ty)); // state pointer
    match entry_shape {
        EntryShape::LegacyI64Args => {
            for _ in &entry.params {
                entry_sig.params.push(AbiParam::new(I64));
            }
            entry_sig.returns.push(AbiParam::new(I64));
        }
        EntryShape::BufferProtocol => {
            for p in &entry.params {
                let ty = match p {
                    IrType::I32 => I32,
                    IrType::I64 => I64,
                    other => {
                        return Err(CraneliftError::Codegen(format!(
                            "buffer-protocol entry param {other:?} unsupported"
                        )));
                    }
                };
                entry_sig.params.push(AbiParam::new(ty));
            }
            entry_sig.returns.push(AbiParam::new(I32));
        }
    }

    let entry_fn_id = module
        .declare_function("run_main", Linkage::Export, &entry_sig)
        .map_err(|e| CraneliftError::ModuleDefine(format!("declare run_main: {e}")))?;

    // Stage 5 Phase C.4: declare every lambda func referenced by the
    // module's `closure_table` *before* lowering the entry body so the
    // entry's `Op::MakeClosure` lowering can capture each lambda's
    // `FuncId` for the runtime closure-table population step. Each
    // lambda has the cranelift signature
    //   (state, captures_ptr: i32, params...) -> ret_ty
    // — the captures_ptr is prepended to the IR-declared param list
    // and points at the captures struct the call site materialised in
    // the scratch arena.
    let mut closure_func_ids: Vec<cranelift_module::FuncId> = Vec::new();
    let mut closure_signatures: Vec<Signature> = Vec::new();
    for (slot, &func_idx) in ir.closure_table.iter().enumerate() {
        let lambda = ir.funcs.get(func_idx as usize).ok_or_else(|| {
            CraneliftError::Codegen(format!(
                "closure_table[{slot}] -> funcs[{func_idx}] out of range (module has {} funcs)",
                ir.funcs.len()
            ))
        })?;
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(pointer_ty)); // state pointer
        sig.params.push(AbiParam::new(I32)); // captures_ptr
        for p in &lambda.params {
            sig.params.push(AbiParam::new(ir_ty_to_cl(*p)?));
        }
        if !matches!(lambda.ret, IrType::Null) {
            sig.returns.push(AbiParam::new(ir_ty_to_cl(lambda.ret)?));
        }
        let name = format!("__closure_{slot}");
        let id = module
            .declare_function(&name, Linkage::Local, &sig)
            .map_err(|e| CraneliftError::ModuleDefine(format!("declare {name}: {e}")))?;
        closure_func_ids.push(id);
        closure_signatures.push(sig);
    }

    // Emit the function body.
    let mut ctx = CodegenContext::new();
    ctx.func = Function::with_name_signature(UserFuncName::user(0, 0), entry_sig);

    let mut builder_ctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        // Pull the sandbox state pointer + Int args out of the entry
        // block parameters.
        let block_params: Vec<_> = builder.block_params(entry_block).to_vec();
        let state_ptr = block_params[0];
        let arg_values: Vec<CValue> = block_params[1..].to_vec();

        // v6-γ M2: optionally emit a HotCounter prologue. The helper
        // creates two new blocks (`hot_block` / `normal_block`),
        // branches between them, fills the hot path with a
        // `__relon_jump_to_recorder` call + sentinel return, and
        // leaves the builder positioned on `normal_block` so the rest
        // of the entry codegen flows unchanged.
        if let Some(fn_id) = sandbox.trace_jit_fn_id {
            emit_hot_counter_inject(
                &mut builder,
                pointer_ty,
                entry_shape,
                fn_id,
                &arg_values,
            );
        }

        // v5-γ stage 2: import the capability vtable as a GlobalValue
        // on the current function. Every host-helper call indirects
        // through `load(vtable_base + slot_offset) -> fn_ptr` followed
        // by `call_indirect(sig, fn_ptr, args)` — see
        // `Codegen::emit_host_fn_call`.
        let vtable_gv = module.declare_data_in_func(vtable_data_id, builder.func);
        let raise_trap_sig_ref = builder.import_signature(raise_trap_sig.clone());
        let now_sig_ref = builder.import_signature(now_sig.clone());
        let cap_lookup_sig_ref = builder.import_signature(cap_lookup_sig.clone());

        // Pre-allocate the trap block + a block param that carries
        // the i64 trap code. Every guard branches here with its
        // TrapKind code as an argument; cranelift handles phi nodes
        // automatically when the block has a parameter. We fill the
        // block's body at the very end (after the function body has
        // emitted all its guard branches) so the FunctionBuilder
        // never sees a half-filled block on a `switch_to_block`
        // call.
        let trap_block = builder.create_block();
        builder.append_block_param(trap_block, I64);

        let mut codegen = Codegen {
            builder: &mut builder,
            sandbox,
            state_ptr,
            vtable_gv,
            raise_trap_sig_ref,
            now_sig_ref,
            cap_lookup_sig_ref,
            pointer_ty,
            frontend_config: module.target_config(),
            entry_shape,
            locals: HashMap::new(),
            let_locals: HashMap::new(),
            arg_values: &arg_values,
            stack: Vec::new(),
            ir,
            trap_block: Some(trap_block),
            label_stack: Vec::new(),
            inline_frames: Vec::new(),
            const_pool,
            record_locals: HashMap::new(),
            needs_tail_cursor: matches!(entry_shape, EntryShape::BufferProtocol)
                && body_needs_tail_cursor(&entry.body),
            return_root_size,
            captures_ptr: None,
            lambda_param_tys: None,
        };

        codegen.emit_prologue();
        codegen.emit_body(&entry.body)?;

        // Now fill the trap block body. Every guard branched in with
        // its `TrapKind as i64` as the block param; we call
        // `relon_raise_trap(state, code)` (via vtable indirection) and
        // return a sentinel zero of the entry's return type so the
        // host trampoline can detect the trap via `state.trap_code()`.
        builder.switch_to_block(trap_block);
        let code = builder.block_params(trap_block)[0];
        emit_indirect_host_call(
            &mut builder,
            vtable_gv,
            pointer_ty,
            VtableSlot::RelonRaiseTrap,
            raise_trap_sig_ref,
            &[state_ptr, code],
        );
        let zero = match entry_shape {
            EntryShape::LegacyI64Args => builder.ins().iconst(I64, 0),
            EntryShape::BufferProtocol => builder.ins().iconst(I32, 0),
        };
        builder.ins().return_(&[zero]);
        builder.seal_block(trap_block);

        // The lowering for `Return` already wired the `return`
        // instruction. If the body never emits a return, the cranelift
        // verifier will surface that as an error.

        builder.finalize();
    }

    module
        .define_function(entry_fn_id, &mut ctx)
        .map_err(|e| CraneliftError::ModuleDefine(format!("define run_main: {e}")))?;

    // Stage 5 Phase C.4: compile each lambda function. Each one uses
    // the cranelift signature `(state, captures_ptr, params...) -> ret`
    // — the captures_ptr is the first user-visible local (slot 0 in
    // the cranelift block-param sense, but the IR's `LocalGet` slots
    // start at 1 because the IR pass numbers user params from 1 onward
    // when a captures arg precedes them... actually the IR pass keeps
    // user params at `LocalGet 0..N`, so we need to shift the
    // cranelift slot map at the body entry to "skip" the captures
    // slot when resolving `LocalGet(idx)`).
    for (slot, (func_id, sig)) in closure_func_ids
        .iter()
        .copied()
        .zip(closure_signatures.iter())
        .enumerate()
    {
        let lambda_idx = ir.closure_table[slot] as usize;
        let lambda = &ir.funcs[lambda_idx];
        let mut lambda_ctx = CodegenContext::new();
        lambda_ctx.func =
            Function::with_name_signature(UserFuncName::user(0, (slot as u32) + 1), sig.clone());
        let mut lambda_builder_ctx = FunctionBuilderContext::new();
        {
            let mut builder = FunctionBuilder::new(&mut lambda_ctx.func, &mut lambda_builder_ctx);
            let entry_block = builder.create_block();
            builder.append_block_params_for_function_params(entry_block);
            builder.switch_to_block(entry_block);
            builder.seal_block(entry_block);

            let block_params: Vec<_> = builder.block_params(entry_block).to_vec();
            let lambda_state_ptr = block_params[0];
            let captures_ptr = block_params[1];
            let lambda_arg_values: Vec<CValue> = block_params[2..].to_vec();

            // v5-γ stage 2: import the capability vtable as a
            // GlobalValue on this lambda. Each helper call indirects
            // through `vtable_base + slot_offset` (see
            // `Codegen::emit_host_fn_call`).
            let vtable_gv = module.declare_data_in_func(vtable_data_id, builder.func);
            let raise_trap_sig_ref = builder.import_signature(raise_trap_sig.clone());
            let now_sig_ref = builder.import_signature(now_sig.clone());
            let cap_lookup_sig_ref = builder.import_signature(cap_lookup_sig.clone());

            let trap_block = builder.create_block();
            builder.append_block_param(trap_block, I64);

            // Lambdas use the same entry shape as the entry function
            // for the purposes of `LocalGet` typing — but since each
            // lambda's params are IR-declared independently, we
            // override the entry-shape-derived local typing through
            // `lambda_param_tys`. The Codegen looks up `LocalGet(idx)`
            // against `arg_values` first; we've already routed the
            // captures_ptr to a dedicated slot so the IR-side
            // `LocalGet(idx)` resolves to `arg_values[idx]` which is
            // the user param at position `idx + 1` in the cranelift
            // block-params (we sliced past the captures_ptr).
            let mut codegen = Codegen {
                builder: &mut builder,
                sandbox,
                state_ptr: lambda_state_ptr,
                vtable_gv,
                raise_trap_sig_ref,
                now_sig_ref,
                cap_lookup_sig_ref,
                pointer_ty,
                frontend_config: module.target_config(),
                // Lambdas use the LegacyI64Args entry shape for
                // `LocalGet` typing because their params are
                // IR-declared (i64 / i32 / ...) rather than the
                // buffer-handshake fixed shape. The `lambda_param_tys`
                // field carries the per-param typing so the
                // `LocalGet` resolution matches.
                entry_shape: EntryShape::LegacyI64Args,
                locals: HashMap::new(),
                let_locals: HashMap::new(),
                arg_values: &lambda_arg_values,
                stack: Vec::new(),
                ir,
                trap_block: Some(trap_block),
                label_stack: Vec::new(),
                inline_frames: Vec::new(),
                const_pool,
                record_locals: HashMap::new(),
                needs_tail_cursor: false,
                return_root_size: 0,
                captures_ptr: Some(captures_ptr),
                lambda_param_tys: Some(&lambda.params),
            };

            codegen.emit_prologue();
            codegen.emit_body(&lambda.body)?;

            builder.switch_to_block(trap_block);
            let code = builder.block_params(trap_block)[0];
            emit_indirect_host_call(
                &mut builder,
                vtable_gv,
                pointer_ty,
                VtableSlot::RelonRaiseTrap,
                raise_trap_sig_ref,
                &[lambda_state_ptr, code],
            );
            // Lambdas always return a typed value (the IR-declared
            // ret_ty). On trap-block exit we emit a typed zero so the
            // verifier accepts the synthetic return.
            let zero_v = if matches!(lambda.ret, IrType::I64) {
                builder.ins().iconst(I64, 0)
            } else if matches!(lambda.ret, IrType::F64) {
                builder.ins().f64const(0.0)
            } else {
                builder.ins().iconst(I32, 0)
            };
            builder.ins().return_(&[zero_v]);
            builder.seal_block(trap_block);

            builder.finalize();
        }

        module
            .define_function(func_id, &mut lambda_ctx)
            .map_err(|e| CraneliftError::ModuleDefine(format!("define __closure_{slot}: {e}")))?;
    }

    Ok(LoweredArtifacts {
        entry_fn_id,
        vtable_data_id,
        closure_func_ids,
    })
}

/// v6-γ M2: emit a HotCounter prologue at the current entry block.
///
/// On entry the builder must already be positioned at a freshly-built
/// entry block whose function-param values have been extracted. On
/// return the builder is positioned at a sealed `normal_block` that
/// continues the original entry-block control flow; the hot path
/// branches to a sealed `hot_block` that calls
/// `__relon_jump_to_recorder` and returns a sentinel zero.
///
/// IR shape (`pointer_ty == I64`):
///
/// ```text
/// entry_block:
///     %base    = iconst.i64 <hot_counters_base()>
///     %slot    = iadd_imm %base, fn_id * 4
///     %v       = load.i32 trusted %slot
///     %v1      = iadd_imm.i32 %v, 1
///                store.i32 trusted %v1, %slot
///     %hot     = icmp_imm.i32 uge %v1, RELON_HOT_THRESHOLD
///     brif %hot, hot_block, normal_block
///
/// hot_block:
///     %fn_id_const = iconst.i32 fn_id
///     %args_ptr    = iconst.i64 0    ; v6-γ M2: helper ignores arg ptr
///     call_indirect (sig=jump_sig) %jump_ptr (%fn_id_const, %args_ptr)
///     return  <zero of entry return ty>
///
/// normal_block:
///     ;; existing entry-block continuation
/// ```
fn emit_hot_counter_inject(
    builder: &mut FunctionBuilder<'_>,
    pointer_ty: cranelift_codegen::ir::Type,
    entry_shape: EntryShape,
    fn_id: u32,
    _arg_values: &[CValue],
) {
    let hot_block = builder.create_block();
    let normal_block = builder.create_block();

    // Counter slot address = base + fn_id * sizeof(u32).
    let base_addr = crate::trace_install::hot_counters_base() as i64;
    let slot_offset = (fn_id as i64) * 4;
    let counter_addr = base_addr.wrapping_add(slot_offset);
    let counter_ptr = builder.ins().iconst(pointer_ty, counter_addr);

    // load.i32 / iadd_imm.i32 / store.i32 (non-atomic per design).
    let cur = builder
        .ins()
        .load(I32, MemFlags::trusted(), counter_ptr, 0);
    let inc = builder.ins().iadd_imm(cur, 1);
    builder
        .ins()
        .store(MemFlags::trusted(), inc, counter_ptr, 0);

    // icmp uge against the threshold; branch on the result.
    let hot = builder.ins().icmp_imm(
        IntCC::UnsignedGreaterThanOrEqual,
        inc,
        crate::trace_install::RELON_HOT_THRESHOLD as i64,
    );
    let empty: [BlockArg; 0] = [];
    builder
        .ins()
        .brif(hot, hot_block, empty.iter(), normal_block, empty.iter());

    // Fill the hot block: call the recorder jump helper, then return a
    // sentinel zero of the entry's return type. The helper is invoked
    // by raw fn pointer (iconst -> call_indirect) so we don't have to
    // declare an external symbol on the per-fn cranelift module.
    builder.switch_to_block(hot_block);
    builder.seal_block(hot_block);
    let fn_id_val = builder.ins().iconst(I32, fn_id as i64);
    let args_ptr_val = builder.ins().iconst(pointer_ty, 0); // null — helper ignores
    let mut jump_sig = Signature::new(CallConv::SystemV);
    jump_sig.params.push(AbiParam::new(I32));
    jump_sig.params.push(AbiParam::new(pointer_ty));
    let jump_sig_ref = builder.import_signature(jump_sig);
    let jump_target = builder.ins().iconst(
        pointer_ty,
        crate::trace_install::__relon_jump_to_recorder as *const () as i64,
    );
    builder
        .ins()
        .call_indirect(jump_sig_ref, jump_target, &[fn_id_val, args_ptr_val]);
    let zero = match entry_shape {
        EntryShape::LegacyI64Args => builder.ins().iconst(I64, 0),
        EntryShape::BufferProtocol => builder.ins().iconst(I32, 0),
    };
    builder.ins().return_(&[zero]);

    // Continue with the normal block.
    builder.switch_to_block(normal_block);
    builder.seal_block(normal_block);
}

/// Build the cranelift signature for the `RelonRaiseTrap` vtable
/// slot: `extern "C" fn(state: *const SandboxState, code: i64)`.
fn make_raise_trap_signature(pointer_ty: cranelift_codegen::ir::Type) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params.push(AbiParam::new(I64));
    sig
}

/// Build the cranelift signature for the `RelonNow` vtable slot:
/// `extern "C" fn(state: *const SandboxState) -> i64`.
fn make_now_signature(pointer_ty: cranelift_codegen::ir::Type) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.returns.push(AbiParam::new(I64));
    sig
}

/// Build the cranelift signature for the `RelonCapLookup` vtable
/// slot: `extern "C" fn(state: *const SandboxState, cap_bit: i32) ->
/// *const u8`.
fn make_cap_lookup_signature(pointer_ty: cranelift_codegen::ir::Type) -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params.push(AbiParam::new(I32));
    sig.returns.push(AbiParam::new(pointer_ty));
    sig
}

/// Declare the `__relon_capability_vtable` data symbol on the given
/// module. Reserves [`VTABLE_BYTES`] of zero-initialised space so the
/// host can populate the slots post-finalize (JIT) or post-dlopen
/// (cranelift-object).
///
/// Linkage rules:
/// - `JITModule`: `Linkage::Local` — the JIT resolves the symbol by
///   `DataId` rather than by name, so the linkage is advisory.
/// - `ObjectModule`: `Linkage::Export` — the ELF needs the symbol in
///   `.dynsym` so `dlsym` can find it from the host side.
///
/// We pick `Export` here because both backends accept it; the JIT's
/// `get_finalized_data` works either way.
fn declare_vtable_data<M: CrModule>(module: &mut M) -> Result<DataId, CraneliftError> {
    // `writable = true` because the host populates the slots
    // post-link. `tls = false` — single-process shared vtable.
    let data_id = module
        .declare_data(
            VTABLE_SYMBOL,
            Linkage::Export,
            /*writable=*/ true,
            /*tls=*/ false,
        )
        .map_err(|e| CraneliftError::ModuleDefine(format!("declare vtable data: {e}")))?;
    let mut desc = DataDescription::new();
    desc.define_zeroinit(VTABLE_BYTES);
    module
        .define_data(data_id, &desc)
        .map_err(|e| CraneliftError::ModuleDefine(format!("define vtable data: {e}")))?;
    Ok(data_id)
}

/// Emit an indirect host-helper call: load the function pointer from
/// the vtable slot, then `call_indirect` with the supplied signature.
///
/// Used both inside `Codegen` (for body-level helper calls) and in
/// the `compile_module_with` driver (to lower the trap_block tail).
/// Centralising the load sequence keeps the codegen output uniform
/// across entry / lambda / trap-block call sites.
fn emit_indirect_host_call(
    builder: &mut FunctionBuilder<'_>,
    vtable_gv: GlobalValue,
    pointer_ty: cranelift_codegen::ir::Type,
    slot: VtableSlot,
    sig_ref: SigRef,
    args: &[CValue],
) -> Inst {
    // Materialise the vtable base address in the function.
    let vtable_base = builder.ins().global_value(pointer_ty, vtable_gv);
    // Load the slot's host fn pointer.
    let fn_ptr = builder.ins().load(
        pointer_ty,
        MemFlags::trusted(),
        vtable_base,
        slot.offset_bytes(),
    );
    builder.ins().call_indirect(sig_ref, fn_ptr, args)
}

/// Map a generic IR type to its cranelift slot type. Used by the
/// inline `Op::Call` lowering to size the exit block-param of an
/// inlined callee.
fn ir_ty_to_cl(ty: IrType) -> Result<cranelift_codegen::ir::Type, CraneliftError> {
    Ok(match ty {
        IrType::I64 => I64,
        IrType::F64 => cranelift_codegen::ir::types::F64,
        IrType::I32 | IrType::Bool | IrType::Null => I32,
        // Pointer-indirect leaves carry an i32 buffer-relative
        // offset in the IR's wasm-shaped slot model. Cranelift
        // mirrors that as a plain i32.
        IrType::String
        | IrType::ListInt
        | IrType::ListFloat
        | IrType::ListBool
        | IrType::ListString
        | IrType::ListSchema
        | IrType::Closure => I32,
    })
}

/// Map an IR `LoadField` / `StoreField` `ty` to the cranelift load
/// type, byte width, and stack tag.
///
/// Returns `(cranelift_load_type, byte_width, virtual_stack_ty)`.
/// `cranelift_load_type` is what cranelift's `load`/`store` opcode
/// width key cares about; `byte_width` is consumed by the bounds
/// check; `virtual_stack_ty` documents what the IR-side stack
/// expects after the load.
fn field_load_shape(
    ty: IrType,
) -> Result<(cranelift_codegen::ir::Type, u32, IrType), CraneliftError> {
    match ty {
        IrType::I64 => Ok((I64, 8, IrType::I64)),
        IrType::F64 => Ok((cranelift_codegen::ir::types::F64, 8, IrType::F64)),
        IrType::I32 => Ok((I32, 4, IrType::I32)),
        IrType::Bool | IrType::Null => Ok((cranelift_codegen::ir::types::I8, 1, IrType::Bool)),
        // Pointer-indirect leaves: the fixed-area slot holds a single
        // i32 buffer-relative offset. Loads / stores against the slot
        // therefore use an `i32` access width — the IR-visible value
        // is treated as `IrType::I32` so subsequent ops (Add / memcpy
        // arithmetic / etc.) can manipulate it as a pointer.
        IrType::String
        | IrType::ListInt
        | IrType::ListFloat
        | IrType::ListBool
        | IrType::ListString
        | IrType::ListSchema
        | IrType::Closure => Ok((I32, 4, IrType::I32)),
    }
}

/// Walk the body to decide whether it allocates anything inside the
/// `out_buf` tail area (pointer-indirect StoreField, dict-construction
/// ops, `EmitTailRecordFromAbsoluteAddr`).
///
/// When `true`, the entry prologue must initialise `state.tail_cursor`
/// to `return_root_size` so the first tail allocation lands
/// immediately past the fixed area; the epilogue then returns the
/// post-bump cursor as `bytes_written`. When `false`, the cursor stays
/// at 0 and the epilogue returns `return_root_size` (the host
/// trampoline reads at least that many bytes either way, so the value
/// only matters when the body actually wrote past the fixed area).
fn body_needs_tail_cursor(body: &[TaggedOp]) -> bool {
    for tagged in body {
        match &tagged.op {
            Op::StoreField {
                ty:
                    IrType::String
                    | IrType::ListInt
                    | IrType::ListFloat
                    | IrType::ListBool
                    | IrType::ListString
                    | IrType::ListSchema,
                ..
            } => return true,
            Op::AllocRootRecord { .. }
            | Op::AllocSubRecord { .. }
            | Op::EmitTailRecordFromAbsoluteAddr { .. } => return true,
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                if body_needs_tail_cursor(then_body) || body_needs_tail_cursor(else_body) {
                    return true;
                }
            }
            Op::Block { body, .. } | Op::Loop { body, .. } => {
                if body_needs_tail_cursor(body) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Alignment + tag a pointer-indirect record needs when copied into
/// the tail area.
///
/// Mirrors `relon_codegen_wasm`'s record-size / alignment table:
///
/// * `String` / `ListBool` — 4-byte aligned `[len:4][bytes]`.
/// * `ListInt` / `ListFloat` — 8-byte aligned `[len:4][pad:4][i64/f64 ×n]`.
/// * `ListString` / `ListSchema` — pointer-array shapes that need
///   per-entry relocation. We refuse them on this path; codegen
///   surfaces `UnsupportedStoreFieldType` so the harness reports
///   `CraneliftUnsupported` rather than miscompiling.
fn pointer_indirect_record_align(ty: IrType) -> Result<u32, CraneliftError> {
    match ty {
        IrType::String | IrType::ListBool => Ok(4),
        IrType::ListInt | IrType::ListFloat => Ok(8),
        IrType::ListString | IrType::ListSchema | IrType::Closure => Err(CraneliftError::Codegen(
            format!("pointer-indirect record alignment for {ty:?} not yet supported"),
        )),
        _ => Err(CraneliftError::Codegen(format!(
            "type {ty:?} is not pointer-indirect"
        ))),
    }
}

/// Per-function lowering state. Owns the cranelift builder and tracks
/// the running operand stack alongside variable bindings.
struct Codegen<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    sandbox: &'a SandboxConfig,
    state_ptr: CValue,
    /// v5-γ stage 2: GlobalValue for the `__relon_capability_vtable`
    /// data symbol. Every host-helper call indirects through this
    /// base + a per-slot byte offset; see [`VtableSlot`].
    vtable_gv: GlobalValue,
    /// Pre-built cranelift signature for `relon_raise_trap`. Imported
    /// into the current function once during `compile_module_with`
    /// and reused for every `Op::RaiseTrap` lowering.
    ///
    /// Reserved for future op coverage. v5-beta-1 doesn't emit
    /// `raise_trap` directly — every guard uses cranelift's intrinsic
    /// `trap` / `trapnz`, which delivers the trap-code byte through
    /// the runtime's panic path — but holding the SigRef ready
    /// avoids a second pass for v5-beta-2 to wire in.
    #[allow(dead_code)]
    raise_trap_sig_ref: SigRef,
    /// Pre-built cranelift signature for `relon_now`.
    now_sig_ref: SigRef,
    /// Pre-built cranelift signature for `relon_cap_lookup`.
    cap_lookup_sig_ref: SigRef,
    pointer_ty: cranelift_codegen::ir::Type,
    /// Target frontend config (pointer width / default call conv).
    /// Threaded through so helpers that call `call_memcpy` get the
    /// right libcall signature without re-deriving it from primitives.
    frontend_config: cranelift_codegen::isa::TargetFrontendConfig,
    /// Calling-convention shape picked at compile time. Drives the
    /// `LocalGet` type (i32 vs i64), `Return` epilogue, and the
    /// buffer-protocol load / store address computation.
    entry_shape: EntryShape,
    /// `LocalGet` slot index -> cranelift `Variable`.
    locals: HashMap<u32, Variable>,
    /// `LetGet/LetSet` slot index -> cranelift `Variable`.
    let_locals: HashMap<u32, Variable>,
    arg_values: &'a [CValue],
    /// The IR's virtual operand stack, kept as live cranelift values
    /// so each `Add`/`Sub`/... pop maps to a typed `Value` directly.
    stack: Vec<CValue>,
    /// Reference back to the IR module so `Call` can look up the
    /// referenced function (in v5-beta-1 we inline stdlib bodies
    /// rather than emit per-callee cranelift functions).
    #[allow(dead_code)]
    ir: &'a IrModule,
    /// Pre-allocated "trap-and-return" block. Guards branch here
    /// when they fire; the block holds a single block param carrying
    /// the `TrapKind` code, calls `raise_trap`, and returns 0. The
    /// block is allocated unconditionally and may end up unreachable
    /// when `SandboxConfig` disables every guard, in which case
    /// cranelift's dead-block elimination removes it.
    trap_block: Option<cranelift_codegen::ir::Block>,

    /// v5-β-2 widen: label stack so `Op::Br { label_depth }` /
    /// `Op::BrIf` / `Op::BrTable` can resolve to the matching
    /// cranelift target block.
    ///
    /// Each entry carries the `(target_block, is_loop)` pair where
    /// `target_block` is:
    ///   * for `Op::Block { ... }`: the **exit** block (forward
    ///     branch — `Br N` jumps past the matching End).
    ///   * for `Op::Loop { ... }`: the **header** block (back
    ///     branch — `Br N` re-enters the loop, equivalent to
    ///     `continue`).
    ///
    /// `label_depth = 0` selects the innermost (top of stack)
    /// label; higher depths walk outwards.
    label_stack: Vec<LabelFrame>,

    /// Inline-frame stack for stdlib `Op::Call` lowering. When we
    /// inline a callee body, we push a frame here so the callee's
    /// `LocalGet(idx)` / `LetGet/LetSet` / `Op::Return` resolve
    /// against the call site rather than the entry function. See
    /// [`InlineFrame`] for fields.
    inline_frames: Vec<InlineFrame>,
    /// Pre-computed offset table for const-data records the entry
    /// references through `Op::ConstString` / `Op::ConstList*`.
    /// Cranelift emits `iconst(I32, offset)` for each reference; the
    /// const-data bytes live in the host arena's prefix (the host
    /// trampoline copies them in before each call).
    const_pool: &'a ConstPool,

    /// Cranelift `Variable` per `record_local_idx` allocated by
    /// `Op::AllocRootRecord` / `Op::AllocSubRecord`. Each variable
    /// holds an `i32` out_ptr-relative offset; subsequent
    /// `Op::StoreFieldAtRecord` / `Op::PushRecordBase` ops read it to
    /// compute the in-construction record's destination address.
    record_locals: HashMap<u32, Variable>,
    /// `true` when the entry's body touches the tail-cursor (either
    /// emits pointer-indirect StoreField or uses the
    /// AllocSubRecord / EmitTailRecordFromAbsoluteAddr dict-construction
    /// ops). Drives the prologue init (`tail_cursor = return_root_size`)
    /// and the epilogue return shape (`bytes_written = tail_cursor`
    /// vs constant `return_root_size`).
    needs_tail_cursor: bool,
    /// Pre-computed fixed-area size of the entry's return record.
    /// When `needs_tail_cursor` is `false` and the entry is buffer-
    /// protocol, the epilogue returns this as `bytes_written`. The
    /// prologue uses the same value to bias `tail_cursor` to the
    /// first byte past the fixed area when tail records are present.
    return_root_size: u32,
    /// Stage 5 Phase C.4: when this Codegen is lowering a *lambda*
    /// body (not the entry function), `captures_ptr` carries the
    /// cranelift `i32` block-param the lambda received as its
    /// captures argument. `Op::LoadField` against an offset inside
    /// the captures struct resolves through this pointer (added to
    /// `arena_base`); `Op::LocalGet` continues to address the
    /// IR-declared params via `arg_values`.
    captures_ptr: Option<CValue>,
    /// When set (lambda mode), supplies the per-param IR types so
    /// `LocalGet(idx)` resolves to the correct cranelift slot type.
    /// `None` when lowering the entry function (which derives types
    /// from `entry_shape`).
    lambda_param_tys: Option<&'a [IrType]>,
}

/// One inline-frame entry for a stdlib body lowered through
/// `Op::Call`. See `Codegen::inline_frames` for usage.
struct InlineFrame {
    /// Cranelift values bound to the callee's declared parameters.
    /// `LocalGet(idx)` reads from this slice while the frame is
    /// active.
    params: Vec<CValue>,
    /// Block the callee's `Op::Return` jumps to. The exit block has
    /// one block-param carrying the typed return value.
    exit_block: cranelift_codegen::ir::Block,
    /// Result type of the callee. Informational today (block-param
    /// already carries the cranelift type); kept for the future
    /// trace-recorder hook that wants the IR-side tag for guard
    /// emission.
    #[allow(dead_code)]
    ret_ty: IrType,
    /// Caller's `let_locals` size at the moment the inline frame
    /// was pushed. The callee's `LetSet { idx }` rewrites to
    /// `let_offset + idx`, keeping each inlined frame's let
    /// bindings in a private namespace.
    let_offset: u32,
}

/// One label frame for the `Op::Br` / `Op::BrIf` / `Op::BrTable`
/// target resolution.
struct LabelFrame {
    /// The cranelift block this label resolves to (loop header for
    /// `Op::Loop`, exit block for `Op::Block`).
    target_block: cranelift_codegen::ir::Block,
    /// `true` for `Op::Loop` (back-edge); `false` for `Op::Block`
    /// (forward-edge). Used by [`Codegen::emit_loop_back_resource_check`]
    /// to recognise loop back-edges as the right site for inserting
    /// the [`crate::sandbox::RESOURCE_CHECK_INTERVAL`] cadence guard.
    is_loop: bool,
    /// When the labelled construct yields a typed value (`Op::Loop`
    /// or `Op::Block` with `result_ty = Some(_)`), this slot holds
    /// the cranelift type the matching block-param accepts. `Br` /
    /// `BrIf` / `BrTable` targeting this frame pops one operand from
    /// the virtual stack and forwards it as the block-param.
    ///
    /// For `Op::Loop` with a yield, the block-param sits on the loop
    /// header and represents the loop-carried accumulator (each back-
    /// edge supplies the next iteration's value); the loop exits by
    /// falling through to the continuation block which inherits the
    /// final value.
    ///
    /// For `Op::Block` with a yield, the block-param sits on the
    /// continuation block. `Br N` inside the body pops the yield
    /// value and forwards it as the continuation arg.
    result_cl_ty: Option<cranelift_codegen::ir::Type>,
    /// When the frame is a `Op::Loop` with `result_ty != None`, this
    /// is the continuation block that receives the loop's final
    /// value via fallthrough. `None` for blocks / yield-less loops.
    loop_cont_block: Option<cranelift_codegen::ir::Block>,
    /// Per-loop back-edge counter variable used to space the
    /// resource-deadline guard at [`crate::sandbox::RESOURCE_CHECK_INTERVAL`]
    /// cadence inside long-running loops. `None` for blocks (which
    /// have no back-edge) and when the sandbox's deadline check is
    /// disabled. The counter is an `I64` increment-and-mask Variable;
    /// `emit_loop_back_resource_check` reads / updates it on every
    /// back-edge.
    back_edge_counter: Option<Variable>,
}

impl<'a, 'b> Codegen<'a, 'b> {
    /// v5-γ stage 2: emit an indirect call to the host helper at
    /// `slot`. Loads the function pointer from
    /// `__relon_capability_vtable[slot]` and `call_indirect`s with the
    /// matching pre-built signature.
    fn emit_host_fn_call(&mut self, slot: VtableSlot, args: &[CValue]) -> Inst {
        let sig_ref = match slot {
            VtableSlot::RelonNow => self.now_sig_ref,
            VtableSlot::RelonRaiseTrap => self.raise_trap_sig_ref,
            VtableSlot::RelonCapLookup => self.cap_lookup_sig_ref,
        };
        emit_indirect_host_call(
            self.builder,
            self.vtable_gv,
            self.pointer_ty,
            slot,
            sig_ref,
            args,
        )
    }

    /// Emit the entry prologue: resource-limit check (one wall-clock
    /// read + comparison) plus any other one-shot setup. For buffer-
    /// protocol entries whose body emits pointer-indirect stores or
    /// dict-construction ops, also initialise `state.tail_cursor` to
    /// `return_root_size` so the first tail allocation lands
    /// immediately past the fixed area.
    fn emit_prologue(&mut self) {
        if self.sandbox.deadline_check {
            self.emit_resource_check();
        }
        if self.needs_tail_cursor {
            let init = self
                .builder
                .ins()
                .iconst(I32, i64::from(self.return_root_size));
            self.builder.ins().store(
                MemFlags::trusted(),
                init,
                self.state_ptr,
                STATE_OFFSET_TAIL_CURSOR,
            );
        }
    }

    /// Conditional trap: when `cond` is non-zero, jump to the trap
    /// block with the supplied `TrapKind` code as the block param.
    /// Replaces the cranelift intrinsic `trapnz`-via-`ud2` path
    /// that produced SIGILL on x86 Linux, which `catch_unwind`
    /// cannot intercept on stable Rust.
    fn cond_trap(&mut self, cond: CValue, kind: TrapKind) {
        let trap_block = self
            .trap_block
            .expect("trap_block must be pre-allocated by compile_module");
        let continue_block = self.builder.create_block();
        let code_val = self.builder.ins().iconst(I64, i64::from(kind as u8));
        self.builder
            .ins()
            .brif(cond, trap_block, &[code_val.into()], continue_block, &[]);
        self.builder.seal_block(continue_block);
        self.builder.switch_to_block(continue_block);
    }

    /// Insert a deadline guard at the current insertion point. Reads
    /// `state.epoch.elapsed().as_nanos()` via the host helper and
    /// traps when the result is past `state.deadline_ns`.
    fn emit_resource_check(&mut self) {
        // call relon_now(state) -> i64 via the capability vtable.
        let inst = self.emit_host_fn_call(VtableSlot::RelonNow, &[self.state_ptr]);
        let elapsed = self.builder.inst_results(inst)[0];

        // Load deadline_ns from state. The offset lives in
        // `STATE_OFFSET_DEADLINE_NS`; the codegen and sandbox must
        // agree on it.
        let deadline = self.builder.ins().load(
            I64,
            MemFlags::trusted(),
            self.state_ptr,
            crate::sandbox::STATE_OFFSET_DEADLINE_NS,
        );

        // Trap when elapsed >= deadline.
        let cmp = self
            .builder
            .ins()
            .icmp(IntCC::SignedGreaterThanOrEqual, elapsed, deadline);
        self.cond_trap(cmp, TrapKind::ResourceExhausted);
    }

    /// Buffer-protocol mode: compute the absolute host address for a
    /// `(buf_local_idx, byte_offset, slot_size)` triple, after a
    /// bounds check against `state.arena_len`. Returns the absolute
    /// pointer-typed cranelift value, suitable for direct
    /// `load`/`store` with `MemFlags::trusted()` and zero immediate
    /// offset.
    ///
    /// `buf_local_idx` is the IR's wasm-local slot — 0 for `in_ptr`,
    /// 2 for `out_ptr` — read through `get_local`. `slot_size` is
    /// the byte width the caller is about to touch; the bounds check
    /// verifies `buf_ptr + byte_offset + slot_size <= arena_len`.
    fn buffer_field_addr(
        &mut self,
        buf_local_idx: u32,
        byte_offset: u32,
        slot_size: u32,
    ) -> Result<CValue, CraneliftError> {
        // buf_ptr is i32 (the wasm handshake slot).
        let buf_ptr_i32 = self.get_local(buf_local_idx)?;
        // Widen to pointer-sized arithmetic so we never lose bits on
        // 64-bit hosts. `uextend` because the wasm-side semantics
        // treat the i32 as an unsigned byte offset.
        let buf_ptr = self.builder.ins().uextend(self.pointer_ty, buf_ptr_i32);

        // arena_base: load pointer-sized field from state.
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        let arena_len_i32 = self.builder.ins().load(
            I32,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_LEN,
        );

        // Bounds: required_end = byte_offset + slot_size; trap when
        // (buf_ptr + required_end) > arena_len. Doing the add as i32
        // mirrors the wasm-side semantics where the in/out pointer
        // is itself an i32 offset.
        if self.sandbox.bounds_check {
            let required_end = byte_offset
                .checked_add(slot_size)
                .ok_or_else(|| CraneliftError::Codegen("buffer field offset overflow".into()))?;
            let req_v = self.builder.ins().iconst(I32, i64::from(required_end));
            let end_i32 = self.builder.ins().iadd(buf_ptr_i32, req_v);
            let cmp = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, end_i32, arena_len_i32);
            self.cond_trap(cmp, TrapKind::BoundsViolation);
        }

        // Compute absolute address = arena_base + buf_ptr + offset.
        let abs0 = self.builder.ins().iadd(arena_base, buf_ptr);
        let off_v = self
            .builder
            .ins()
            .iconst(self.pointer_ty, i64::from(byte_offset));
        let abs = self.builder.ins().iadd(abs0, off_v);
        Ok(abs)
    }

    /// Lower `Op::LoadField { offset, ty }`. Reads from
    /// `in_ptr + offset` (wasm slot 0) and pushes the value onto the
    /// virtual stack.
    ///
    /// In lambda mode (Stage 5 Phase C.4 closure body), the base
    /// pointer is the captures struct base (`captures_ptr` block-
    /// param) rather than `in_ptr` — this matches the wasm-side
    /// closure ABI which reuses `LoadField` for "read this captured
    /// value at this offset".
    fn emit_load_field(&mut self, offset: u32, ty: IrType) -> Result<(), CraneliftError> {
        let (cr_ty, size, push_ty) = field_load_shape(ty)?;
        let addr = if let Some(captures_ptr) = self.captures_ptr {
            // Lambda mode: arena_base + captures_ptr + offset.
            let off_v = self.builder.ins().iconst(I32, i64::from(offset));
            let composed = self.builder.ins().iadd(captures_ptr, off_v);
            self.arena_addr(composed, size)?
        } else {
            if !matches!(self.entry_shape, EntryShape::BufferProtocol) {
                return Err(CraneliftError::Codegen(
                    "LoadField outside buffer-protocol entry shape".into(),
                ));
            }
            self.buffer_field_addr(0 /* in_ptr */, offset, size)?
        };
        let loaded = self.builder.ins().load(cr_ty, MemFlags::trusted(), addr, 0);
        // For `Bool` / `Null` the IR's virtual stack expects an i32
        // slot — widen the loaded byte to i32 zero-extended.
        let val = match ty {
            IrType::Bool | IrType::Null => self.builder.ins().uextend(I32, loaded),
            _ => loaded,
        };
        let _ = push_ty;
        self.push(val);
        Ok(())
    }

    /// Lower `Op::StoreField { offset, ty }`. Pops the top of the
    /// virtual stack and writes it into `out_ptr + offset` (wasm slot
    /// 2). Scalar (I64 / F64 / I32 / Bool / Null) stores go through a
    /// direct cranelift `store`. Pointer-indirect stores (String /
    /// ListInt / ListFloat / ListBool) route through
    /// [`emit_store_pointer_indirect`], which mirrors the wasm-side
    /// tail-cursor protocol: pop the source pointer, memcpy the
    /// `[len:4][payload]` record into `out_ptr + tail_cursor`, store
    /// `tail_cursor` (the new buffer-relative offset) into the fixed-
    /// area slot, and bump `tail_cursor`. ListString / ListSchema
    /// stay unsupported because they need per-entry relocation.
    fn emit_store_field(&mut self, offset: u32, ty: IrType) -> Result<(), CraneliftError> {
        if !matches!(self.entry_shape, EntryShape::BufferProtocol) {
            return Err(CraneliftError::Codegen(
                "StoreField outside buffer-protocol entry shape".into(),
            ));
        }
        if matches!(
            ty,
            IrType::String | IrType::ListInt | IrType::ListFloat | IrType::ListBool
        ) {
            return self.emit_store_pointer_indirect(offset, ty);
        }
        if matches!(ty, IrType::ListString | IrType::ListSchema) {
            return Err(CraneliftError::Codegen(format!(
                "StoreField pointer-indirect type {ty:?} (pointer-array) not yet supported",
            )));
        }
        let (cr_ty, size, _push_ty) = field_load_shape(ty)?;
        let value = self.pop()?;
        // For `Bool` / `Null` the stack slot is i32 but the store
        // width is i8.
        let store_val = match ty {
            IrType::Bool | IrType::Null => self
                .builder
                .ins()
                .ireduce(cranelift_codegen::ir::types::I8, value),
            _ => value,
        };
        let store_ty = match ty {
            IrType::Bool | IrType::Null => cranelift_codegen::ir::types::I8,
            _ => cr_ty,
        };
        let addr = self.buffer_field_addr(2 /* out_ptr */, offset, size)?;
        let _ = store_ty; // cranelift `store` infers width from value type
        self.builder
            .ins()
            .store(MemFlags::trusted(), store_val, addr, 0);
        Ok(())
    }

    /// Bump-allocate `size` bytes inside the output buffer's tail
    /// region.
    ///
    /// Mirrors the wasm-side `emit_tail_alloc` helper:
    ///
    /// 1. Align `state.tail_cursor` up to `align` (must be a power of
    ///    two — typical values are 4 / 8).
    /// 2. Bounds-check `aligned_cursor + size <= arena_len -
    ///    out_ptr`. We fold `out_ptr` into the comparison by
    ///    comparing `out_ptr + aligned_cursor + size` against
    ///    `arena_len`.
    /// 3. Record the new cursor in `state.tail_cursor`.
    /// 4. Return the **pre-bump** cursor — the slot the caller will
    ///    write into. The caller adds `out_ptr` (and optionally
    ///    `arena_base`) to materialise an absolute address.
    ///
    /// Returns the pre-bump cursor as an `i32` cranelift value (i.e.
    /// the buffer-relative offset of the freshly reserved region).
    /// The bump cursor is written back to `state.tail_cursor` so the
    /// next `emit_tail_alloc` (or the trampoline reading
    /// `tail_cursor()`) sees the post-bump value.
    fn emit_tail_alloc(&mut self, size: CValue, align: u32) -> Result<CValue, CraneliftError> {
        // Read current cursor.
        let cur = self.builder.ins().load(
            I32,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_TAIL_CURSOR,
        );
        // Align up to `align`. `align <= 1` keeps the cursor as-is.
        let aligned = if align <= 1 {
            cur
        } else {
            let add = self.builder.ins().iconst(I32, i64::from(align as i32 - 1));
            let mask = self
                .builder
                .ins()
                .iconst(I32, i64::from(!(align as i32 - 1)));
            let sum = self.builder.ins().iadd(cur, add);
            self.builder.ins().band(sum, mask)
        };
        // Bounds-check: out_ptr + aligned + size <= arena_len.
        // The out_ptr we use is the wasm-side handshake slot (local
        // 2), holding the absolute offset into the arena where the
        // out_buf starts.
        if self.sandbox.bounds_check {
            let out_ptr = self.get_local(2)?;
            let arena_len = self.builder.ins().load(
                I32,
                MemFlags::trusted(),
                self.state_ptr,
                STATE_OFFSET_ARENA_LEN,
            );
            let end0 = self.builder.ins().iadd(out_ptr, aligned);
            let end = self.builder.ins().iadd(end0, size);
            let cmp = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, end, arena_len);
            self.cond_trap(cmp, TrapKind::BoundsViolation);
        }
        // Bump and persist the new cursor.
        let new_cur = self.builder.ins().iadd(aligned, size);
        self.builder.ins().store(
            MemFlags::trusted(),
            new_cur,
            self.state_ptr,
            STATE_OFFSET_TAIL_CURSOR,
        );
        Ok(aligned)
    }

    /// Lower `Op::StoreField { ty }` for a pointer-indirect type
    /// (`String` / `ListInt` / `ListFloat` / `ListBool`). Pops the
    /// source pointer (an arena-relative i32 offset where a
    /// `[len:u32 LE][payload]` record lives), memcpies the record into
    /// `out_ptr + tail_cursor`, writes `tail_cursor` (the buffer-
    /// relative offset of the just-written record) into the fixed-
    /// area slot at `offset`, and bumps `tail_cursor`.
    fn emit_store_pointer_indirect(
        &mut self,
        offset: u32,
        ty: IrType,
    ) -> Result<(), CraneliftError> {
        let src_off_i32 = self.pop()?;
        // Compute record_size from the in-record length prefix.
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        let src_off_p = self.builder.ins().uextend(self.pointer_ty, src_off_i32);
        let src_abs = self.builder.ins().iadd(arena_base, src_off_p);
        // Load element / byte count from src+0.
        let len_i32 = self
            .builder
            .ins()
            .load(I32, MemFlags::trusted(), src_abs, 0);
        let record_size = match ty {
            IrType::String => {
                // record_size = payload_len + 4
                let four = self.builder.ins().iconst(I32, 4);
                self.builder.ins().iadd(len_i32, four)
            }
            IrType::ListInt | IrType::ListFloat => {
                // record_size = 8 + 8 * element_count
                let three = self.builder.ins().iconst(I32, 3);
                let shifted = self.builder.ins().ishl(len_i32, three);
                let eight = self.builder.ins().iconst(I32, 8);
                self.builder.ins().iadd(shifted, eight)
            }
            IrType::ListBool => {
                // record_size = 4 + element_count
                let four = self.builder.ins().iconst(I32, 4);
                self.builder.ins().iadd(len_i32, four)
            }
            _ => {
                return Err(CraneliftError::Codegen(format!(
                    "emit_store_pointer_indirect: unsupported {ty:?}"
                )));
            }
        };
        let align = pointer_indirect_record_align(ty)?;
        // Reserve the tail slot.
        let pre_cursor = self.emit_tail_alloc(record_size, align)?;
        // Compute absolute dest = arena_base + out_ptr + pre_cursor.
        let out_ptr_i32 = self.get_local(2)?;
        let out_ptr = self.builder.ins().uextend(self.pointer_ty, out_ptr_i32);
        let pre_cursor_p = self.builder.ins().uextend(self.pointer_ty, pre_cursor);
        let dest0 = self.builder.ins().iadd(arena_base, out_ptr);
        let dest = self.builder.ins().iadd(dest0, pre_cursor_p);
        // memcpy(dest, src_abs, record_size).
        let size_p = self.builder.ins().uextend(self.pointer_ty, record_size);
        self.builder
            .call_memcpy(self.frontend_config, dest, src_abs, size_p);
        // Store pre_cursor (the buffer-relative offset) at the fixed-
        // area slot `out_ptr + offset`.
        let slot_addr = self.buffer_field_addr(2 /* out_ptr */, offset, 4)?;
        self.builder
            .ins()
            .store(MemFlags::trusted(), pre_cursor, slot_addr, 0);
        Ok(())
    }

    /// Lower `Op::EmitTailRecordFromAbsoluteAddr { ty }`. Pops an
    /// arena-relative source pointer (an `i32` offset where a
    /// `[len:u32 LE][payload]` record lives), memcpies the record
    /// into the output buffer's tail area at `tail_cursor`, bumps the
    /// cursor past the record, and pushes the pre-bump cursor (= the
    /// buffer-relative offset of the just-written record) onto the
    /// virtual stack as an `i32`. The pushed value is what subsequent
    /// `Op::StoreFieldAtRecord { ty: String / ListInt / ... }` stores
    /// into a parent record's pointer slot.
    fn emit_tail_record_from_absolute(&mut self, ty: IrType) -> Result<(), CraneliftError> {
        let src_off_i32 = self.pop()?;
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        let src_off_p = self.builder.ins().uextend(self.pointer_ty, src_off_i32);
        let src_abs = self.builder.ins().iadd(arena_base, src_off_p);
        let len_i32 = self
            .builder
            .ins()
            .load(I32, MemFlags::trusted(), src_abs, 0);
        let record_size = match ty {
            IrType::String => {
                let four = self.builder.ins().iconst(I32, 4);
                self.builder.ins().iadd(len_i32, four)
            }
            IrType::ListInt | IrType::ListFloat => {
                let three = self.builder.ins().iconst(I32, 3);
                let shifted = self.builder.ins().ishl(len_i32, three);
                let eight = self.builder.ins().iconst(I32, 8);
                self.builder.ins().iadd(shifted, eight)
            }
            IrType::ListBool => {
                let four = self.builder.ins().iconst(I32, 4);
                self.builder.ins().iadd(len_i32, four)
            }
            IrType::ListString | IrType::ListSchema => {
                return Err(CraneliftError::Codegen(format!(
                    "EmitTailRecordFromAbsoluteAddr {ty:?} (pointer-array) not yet supported"
                )));
            }
            _ => {
                return Err(CraneliftError::Codegen(format!(
                    "EmitTailRecordFromAbsoluteAddr unsupported {ty:?}"
                )));
            }
        };
        let align = pointer_indirect_record_align(ty)?;
        let pre_cursor = self.emit_tail_alloc(record_size, align)?;
        let out_ptr_i32 = self.get_local(2)?;
        let out_ptr = self.builder.ins().uextend(self.pointer_ty, out_ptr_i32);
        let pre_cursor_p = self.builder.ins().uextend(self.pointer_ty, pre_cursor);
        let dest0 = self.builder.ins().iadd(arena_base, out_ptr);
        let dest = self.builder.ins().iadd(dest0, pre_cursor_p);
        let size_p = self.builder.ins().uextend(self.pointer_ty, record_size);
        self.builder
            .call_memcpy(self.frontend_config, dest, src_abs, size_p);
        // Push the pre-bump cursor.
        self.push(pre_cursor);
        Ok(())
    }

    /// Resolve / create the cranelift variable that backs an
    /// `Op::AllocRootRecord` / `Op::AllocSubRecord` record-local
    /// index. Each variable holds an `i32` out_ptr-relative offset.
    fn get_or_create_record_local(&mut self, idx: u32) -> Variable {
        if let Some(v) = self.record_locals.get(&idx).copied() {
            return v;
        }
        let v = self.builder.declare_var(I32);
        self.record_locals.insert(idx, v);
        v
    }

    /// Lower `Op::AllocRootRecord { record_local_idx }`. The root
    /// record sits at `out_ptr + 0` so we just bind the record-local
    /// to a constant `i32 0`. Subsequent `Op::StoreFieldAtRecord` /
    /// `Op::PushRecordBase` ops uniformly compute `out_ptr +
    /// record_local + offset`.
    fn emit_alloc_root_record(&mut self, idx: u32) {
        let var = self.get_or_create_record_local(idx);
        let zero = self.builder.ins().iconst(I32, 0);
        self.builder.def_var(var, zero);
    }

    /// Lower `Op::AllocSubRecord { record_local_idx, root_size,
    /// root_align }`. Aligns `tail_cursor` up to `root_align`,
    /// bounds-checks against `arena_len - out_ptr`, stores the
    /// aligned cursor into the record-local, then bumps
    /// `tail_cursor` by `root_size`.
    fn emit_alloc_sub_record(
        &mut self,
        idx: u32,
        root_size: u32,
        root_align: u32,
    ) -> Result<(), CraneliftError> {
        let size_v = self.builder.ins().iconst(I32, i64::from(root_size));
        let pre_cursor = self.emit_tail_alloc(size_v, root_align)?;
        let var = self.get_or_create_record_local(idx);
        self.builder.def_var(var, pre_cursor);
        Ok(())
    }

    /// Lower `Op::PushRecordBase { record_local_idx }`. Reads the
    /// record-local and pushes its current value onto the stack so
    /// the surrounding parent record can store the sub-record's
    /// base offset into its pointer slot.
    fn emit_push_record_base(&mut self, idx: u32) -> Result<(), CraneliftError> {
        let var = *self.record_locals.get(&idx).ok_or_else(|| {
            CraneliftError::Codegen(format!(
                "PushRecordBase({idx}) before matching AllocRootRecord / AllocSubRecord"
            ))
        })?;
        let v = self.builder.use_var(var);
        self.push(v);
        Ok(())
    }

    /// Lower `Op::StoreFieldAtRecord { record_local_idx, offset, ty
    /// }`. Pops the top of the virtual stack and writes it into
    /// `out_ptr + record_local + offset`.
    fn emit_store_field_at_record(
        &mut self,
        idx: u32,
        offset: u32,
        ty: IrType,
    ) -> Result<(), CraneliftError> {
        let value = self.pop()?;
        let var = *self.record_locals.get(&idx).ok_or_else(|| {
            CraneliftError::Codegen(format!(
                "StoreFieldAtRecord({idx}) before matching AllocRootRecord / AllocSubRecord"
            ))
        })?;
        let record_base_i32 = self.builder.use_var(var);
        // Compute absolute dest = arena_base + out_ptr + record_base
        // + offset. Bounds-check via the same arena_len comparison
        // `buffer_field_addr` uses, but parameterised by
        // `record_base + offset` instead of a fixed compile-time
        // offset.
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        let out_ptr_i32 = self.get_local(2)?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let slot_off_i32 = self.builder.ins().iadd(record_base_i32, off_v);
        // Slot size for the bounds check: scalar -> {1, 4, 8};
        // pointer-indirect -> 4 (the slot stores an i32 offset).
        let slot_size = match ty {
            IrType::I64 | IrType::F64 => 8,
            IrType::I32
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::Closure => 4,
            IrType::Bool | IrType::Null => 1,
        };
        if self.sandbox.bounds_check {
            let arena_len = self.builder.ins().load(
                I32,
                MemFlags::trusted(),
                self.state_ptr,
                STATE_OFFSET_ARENA_LEN,
            );
            let size_v = self.builder.ins().iconst(I32, i64::from(slot_size));
            let off_total = self.builder.ins().iadd(out_ptr_i32, slot_off_i32);
            let end = self.builder.ins().iadd(off_total, size_v);
            let cmp = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, end, arena_len);
            self.cond_trap(cmp, TrapKind::BoundsViolation);
        }
        // Build absolute pointer.
        let out_ptr = self.builder.ins().uextend(self.pointer_ty, out_ptr_i32);
        let slot_off_p = self.builder.ins().uextend(self.pointer_ty, slot_off_i32);
        let dest0 = self.builder.ins().iadd(arena_base, out_ptr);
        let dest = self.builder.ins().iadd(dest0, slot_off_p);
        // Emit the store. For `Bool` / `Null`, the stack slot is i32
        // but the underlying record stores i8. For pointer-indirect
        // types the value is already an i32 buffer-relative offset.
        match ty {
            IrType::I64 | IrType::F64 => {
                self.builder
                    .ins()
                    .store(MemFlags::trusted(), value, dest, 0);
            }
            IrType::I32
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::Closure => {
                self.builder
                    .ins()
                    .store(MemFlags::trusted(), value, dest, 0);
            }
            IrType::Bool | IrType::Null => {
                let v8 = self
                    .builder
                    .ins()
                    .ireduce(cranelift_codegen::ir::types::I8, value);
                self.builder.ins().store(MemFlags::trusted(), v8, dest, 0);
            }
        }
        Ok(())
    }

    /// Lower `Op::ReadStringLen`. Pops an i32 arena-relative pointer
    /// (a String or List* record's base), loads the leading 4-byte
    /// length prefix, and pushes it widened to i64. The bounds check
    /// verifies the 4 bytes fit inside the arena.
    fn emit_read_string_len(&mut self) -> Result<(), CraneliftError> {
        let ptr_i32 = self.pop()?;
        // Widen ptr to host pointer width.
        let ptr = self.builder.ins().uextend(self.pointer_ty, ptr_i32);
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        let arena_len_i32 = self.builder.ins().load(
            I32,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_LEN,
        );
        // Bounds: ptr + 4 <= arena_len.
        if self.sandbox.bounds_check {
            let four = self.builder.ins().iconst(I32, 4);
            let end_i32 = self.builder.ins().iadd(ptr_i32, four);
            let cmp = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, end_i32, arena_len_i32);
            self.cond_trap(cmp, TrapKind::BoundsViolation);
        }
        let abs = self.builder.ins().iadd(arena_base, ptr);
        let len_i32 = self.builder.ins().load(I32, MemFlags::trusted(), abs, 0);
        let len_i64 = self.builder.ins().uextend(I64, len_i32);
        self.push(len_i64);
        Ok(())
    }

    /// Emit the function's `Return`:
    ///   * Inline frame active — pop the top of the virtual stack
    ///     and `jump exit_block(v)`, finishing the callee body.
    ///   * LegacyI64Args (no inline) — pop the top of the virtual
    ///     stack and emit `return v: i64`.
    ///   * BufferProtocol (no inline) — the wasm-side semantics
    ///     push `i32 bytes_written` (the tail cursor when the body
    ///     emitted pointer-indirect stores, else `return_root_size`)
    ///     and end the function.
    fn emit_return(&mut self) -> Result<(), CraneliftError> {
        if let Some(exit) = self.inline_frames.last().map(|f| f.exit_block) {
            // Inline-frame return: jump to the exit block with the
            // popped value as the block param. The caller's
            // `emit_call_stdlib` continues from there.
            let v = self.pop()?;
            self.builder.ins().jump(exit, &[v.into()]);
            // After the unconditional jump, the rest of the basic
            // block is unreachable. Provide a dummy block so any
            // subsequent ops emitted before the inline frame is
            // popped land somewhere valid.
            let dummy = self.builder.create_block();
            self.builder.seal_block(dummy);
            self.builder.switch_to_block(dummy);
            return Ok(());
        }
        match self.entry_shape {
            EntryShape::LegacyI64Args => {
                // In lambda mode (lambda_param_tys is set) the return
                // value's IR type isn't always I64 — the lambda's
                // `ret` could be I64 / Bool / String / etc. We just
                // pop one operand and return it; cranelift's verifier
                // catches type-width mismatches before finalize.
                let v = self.pop()?;
                self.builder.ins().return_(&[v]);
            }
            EntryShape::BufferProtocol => {
                // Mirrors the wasm-side epilogue: for bodies that
                // touched the tail-cursor (pointer-indirect stores /
                // dict construction) return the post-bump cursor;
                // otherwise return the precomputed `return_root_size`
                // so the host trampoline reads the full fixed area.
                let value = if self.needs_tail_cursor {
                    self.builder.ins().load(
                        I32,
                        MemFlags::trusted(),
                        self.state_ptr,
                        STATE_OFFSET_TAIL_CURSOR,
                    )
                } else {
                    self.builder
                        .ins()
                        .iconst(I32, i64::from(self.return_root_size))
                };
                self.builder.ins().return_(&[value]);
            }
        }
        // After the explicit return, the current block is filled.
        // Switch to a fresh dummy block so any subsequent ops the
        // body emits land somewhere valid; cranelift's DCE prunes
        // the now-dead dummy. Mirrors the post-Br / post-BrTable
        // dummy pattern.
        let dummy = self.builder.create_block();
        self.builder.seal_block(dummy);
        self.builder.switch_to_block(dummy);
        Ok(())
    }

    /// Translate a stdlib `Op::Call` by inlining the callee's body.
    ///
    /// The IR's `Op::Call { fn_index, arg_count, param_tys, ret_ty }`
    /// is the surface for stdlib dispatch (and, in the future,
    /// user-function dispatch). The wasm backend resolves `fn_index`
    /// against the bundled stdlib + user functions and emits a wasm
    /// `call` instruction. The cranelift backend has no separate
    /// callee compilation unit yet, so v5-β-2 inlines the body in
    /// place: pop `arg_count` cranelift values off the operand
    /// stack, bind them to the callee's `params` slots, lower the
    /// callee body with an active `InlineFrame`, and continue at the
    /// exit block carrying the typed return value.
    fn emit_call_stdlib(
        &mut self,
        fn_index: u32,
        arg_count: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), CraneliftError> {
        // Resolve the callee. The IR pass uses `fn_index = stdlib idx`
        // for bundled stdlib calls and `fn_index = N + user_fn_idx`
        // for user-defined. v5-β-2 only inlines bundled stdlib bodies
        // — fn_index that exceeds the bundled stdlib's length surfaces
        // as Codegen failure so the harness routes the case to
        // `CraneliftUnsupported`.
        let stdlib = relon_ir::stdlib::builtin_stdlib();
        let callee = stdlib.get(fn_index as usize).ok_or_else(|| {
            CraneliftError::Codegen(format!(
                "Op::Call fn_index {fn_index} outside bundled stdlib (max {})",
                stdlib.len()
            ))
        })?;

        // Sanity-check arity + param shapes against the IR's tag.
        if callee.params.len() != arg_count as usize {
            return Err(CraneliftError::Codegen(format!(
                "Op::Call to `{}` declares {} args but callee has {}",
                callee.name,
                arg_count,
                callee.params.len()
            )));
        }
        for (i, (declared, expected)) in callee.params.iter().zip(param_tys.iter()).enumerate() {
            if declared != expected {
                return Err(CraneliftError::Codegen(format!(
                    "Op::Call to `{}` arg #{i}: callee expects {declared:?}, IR tags {expected:?}",
                    callee.name
                )));
            }
        }

        // Pop the arguments off the operand stack. The IR pushes
        // them in declaration order, so the last-pushed value is the
        // last param.
        let mut args = Vec::with_capacity(arg_count as usize);
        for _ in 0..arg_count {
            args.push(self.pop()?);
        }
        args.reverse();

        // Allocate the exit block + result-carrier param.
        let exit_block = self.builder.create_block();
        let exit_ty = ir_ty_to_cl(ret_ty)?;
        self.builder.append_block_param(exit_block, exit_ty);

        // Capture the let_locals "next free slot" snapshot. Stdlib
        // bodies don't typically declare let bindings, but the
        // namespace separation is cheap and future-proofs the
        // inlining once larger callees come online. We use the max
        // currently-used index + 1; if the caller has no let
        // bindings yet, the offset is 0 and the callee's `LetSet 0`
        // maps to caller slot 0 — collision-free because no caller
        // op has run yet that touches let_locals at this nesting.
        let let_offset = self
            .let_locals
            .keys()
            .copied()
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);

        // Push the inline frame and lower the callee body. We clone
        // the body out of the stdlib vector because `emit_body`
        // takes &self mut and we can't simultaneously hold a borrow
        // into stdlib.
        let body = callee.body.clone();
        self.inline_frames.push(InlineFrame {
            params: args,
            exit_block,
            ret_ty,
            let_offset,
        });
        let result = self.emit_body(&body);
        let frame = self.inline_frames.pop().expect("we just pushed one");
        result?;

        // Switch to the exit block; its block-param is the typed
        // return value, push it onto the caller's stack.
        self.builder.seal_block(frame.exit_block);
        self.builder.switch_to_block(frame.exit_block);
        let ret_val = self.builder.block_params(frame.exit_block)[0];
        self.push(ret_val);
        Ok(())
    }

    /// Materialise a cranelift `Variable` for a `LocalGet` slot the
    /// IR references. Slot 0 corresponds to `arg_values[0]`, slot 1
    /// to `arg_values[1]`, and so on. The variable's type tracks the
    /// entry's calling convention:
    ///
    /// * `LegacyI64Args` — every local is `i64`.
    /// * `BufferProtocol` — locals 0..=3 are `i32` (the handshake
    ///   slots `in_ptr`, `in_len`, `out_ptr`, `out_cap`), local 4 is
    ///   `i64` (`caps_arg`).
    ///
    /// When an inline frame is active (we're lowering the body of a
    /// stdlib callee inlined through `Op::Call`), `LocalGet(idx)`
    /// resolves to the matching slot of the topmost inline frame
    /// instead of the entry's locals — preserving the wasm semantics
    /// where the callee sees its own `params` as locals `0..N`.
    fn get_local(&mut self, idx: u32) -> Result<CValue, CraneliftError> {
        if let Some(frame) = self.inline_frames.last() {
            let arg_idx = idx as usize;
            if arg_idx >= frame.params.len() {
                return Err(CraneliftError::Codegen(format!(
                    "LocalGet({idx}) out of range — inlined frame has {} params",
                    frame.params.len()
                )));
            }
            return Ok(frame.params[arg_idx]);
        }
        if let Some(var) = self.locals.get(&idx).copied() {
            return Ok(self.builder.use_var(var));
        }
        let arg_idx = idx as usize;
        if arg_idx >= self.arg_values.len() {
            return Err(CraneliftError::Codegen(format!(
                "LocalGet({idx}) out of range — entry has {} args",
                self.arg_values.len()
            )));
        }
        let cr_ty = if let Some(param_tys) = self.lambda_param_tys {
            // Lambda mode: types come from the IR-declared param list.
            let ir_ty = param_tys.get(arg_idx).copied().ok_or_else(|| {
                CraneliftError::Codegen(format!(
                    "LocalGet({idx}) out of range — lambda has {} declared params",
                    param_tys.len()
                ))
            })?;
            ir_ty_to_cl(ir_ty)?
        } else {
            match self.entry_shape {
                EntryShape::LegacyI64Args => I64,
                EntryShape::BufferProtocol => match idx {
                    0..=3 => I32,
                    4 => I64,
                    _ => {
                        return Err(CraneliftError::Codegen(format!(
                            "LocalGet({idx}) out of range for buffer-protocol entry (5 locals)"
                        )));
                    }
                },
            }
        };
        // Mirror the arg value into a Variable so future LocalSet
        // (if we ever support it) writes go through SSA cleanly.
        let var = self.builder.declare_var(cr_ty);
        self.builder.def_var(var, self.arg_values[arg_idx]);
        self.locals.insert(idx, var);
        Ok(self.builder.use_var(var))
    }

    /// Translate a callee `LetGet/LetSet` index into the caller's
    /// flat let-locals namespace. Each inline frame reserves a
    /// fresh window `let_offset..` so concurrent inlined frames
    /// don't clobber each other's bindings.
    fn remap_let_idx(&self, idx: u32) -> u32 {
        match self.inline_frames.last() {
            Some(frame) => frame.let_offset + idx,
            None => idx,
        }
    }

    /// Resolve / create a `let`-binding slot.
    fn get_let(&mut self, idx: u32, ty: IrType) -> Result<CValue, CraneliftError> {
        let var = match self.let_locals.get(&idx).copied() {
            Some(v) => v,
            None => {
                return Err(CraneliftError::Codegen(format!(
                    "LetGet({idx}) read before LetSet"
                )))
            }
        };
        let _ = ty; // typing handled when the Variable was declared
        Ok(self.builder.use_var(var))
    }

    fn set_let(&mut self, idx: u32, ty: IrType, value: CValue) {
        let var = if let Some(v) = self.let_locals.get(&idx).copied() {
            v
        } else {
            let cr_ty = match ty {
                IrType::I64 => I64,
                IrType::I32 | IrType::Bool | IrType::Null => I32,
                _ => I64, // pointers (String/List/...) map to i64 on x86_64; v5-beta-1
                          // only ever hits this with I64 in practice.
            };
            let v = self.builder.declare_var(cr_ty);
            self.let_locals.insert(idx, v);
            v
        };
        self.builder.def_var(var, value);
    }

    fn push(&mut self, v: CValue) {
        self.stack.push(v);
    }

    fn pop(&mut self) -> Result<CValue, CraneliftError> {
        self.stack
            .pop()
            .ok_or_else(|| CraneliftError::Codegen("stack underflow".into()))
    }

    fn emit_body(&mut self, body: &[TaggedOp]) -> Result<(), CraneliftError> {
        for tagged in body {
            self.emit_op(&tagged.op)?;
        }
        Ok(())
    }

    fn emit_op(&mut self, op: &Op) -> Result<(), CraneliftError> {
        match op {
            Op::ConstI64(v) => {
                let val = self.builder.ins().iconst(I64, *v);
                self.push(val);
            }
            Op::ConstI32(v) => {
                let val = self.builder.ins().iconst(I32, i64::from(*v));
                self.push(val);
            }
            Op::ConstBool(b) => {
                let val = self.builder.ins().iconst(I32, i64::from(*b as i32));
                self.push(val);
            }
            Op::LocalGet(idx) => {
                let v = self.get_local(*idx)?;
                self.push(v);
            }
            Op::LetGet { idx, ty } => {
                let mapped = self.remap_let_idx(*idx);
                let v = self.get_let(mapped, *ty)?;
                self.push(v);
            }
            Op::LetSet { idx, ty } => {
                let mapped = self.remap_let_idx(*idx);
                let v = self.pop()?;
                self.set_let(mapped, *ty, v);
            }
            Op::Add(IrType::I64) => {
                let b = self.pop()?;
                let a = self.pop()?;
                // Use sadd_overflow + cond_trap so signed overflow
                // surfaces as `NumericOverflow` (matching the tree-
                // walker's strict semantics). The wasm-AOT backend
                // wraps silently — cranelift differs deliberately to
                // close the differential corpus.
                let (r, of) = self.builder.ins().sadd_overflow(a, b);
                self.cond_trap(of, TrapKind::NumericOverflow);
                self.push(r);
            }
            Op::Sub(IrType::I64) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let (r, of) = self.builder.ins().ssub_overflow(a, b);
                self.cond_trap(of, TrapKind::NumericOverflow);
                self.push(r);
            }
            Op::Mul(IrType::I64) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let (r, of) = self.builder.ins().smul_overflow(a, b);
                self.cond_trap(of, TrapKind::NumericOverflow);
                self.push(r);
            }
            Op::Div(IrType::I64) => {
                let b = self.pop()?;
                let a = self.pop()?;
                if self.sandbox.div_check {
                    // Trap when divisor == 0. The cond_trap helper
                    // routes through `raise_trap` + early return so
                    // the trap is observable through the typed
                    // `RuntimeError` channel rather than SIGFPE/SIGILL.
                    let zero = self.builder.ins().iconst(I64, 0);
                    let cmp = self.builder.ins().icmp(IntCC::Equal, b, zero);
                    self.cond_trap(cmp, TrapKind::DivisionByZero);
                }
                let r = self.builder.ins().sdiv(a, b);
                self.push(r);
            }
            Op::Mod(IrType::I64) => {
                let b = self.pop()?;
                let a = self.pop()?;
                if self.sandbox.div_check {
                    let zero = self.builder.ins().iconst(I64, 0);
                    let cmp = self.builder.ins().icmp(IntCC::Equal, b, zero);
                    self.cond_trap(cmp, TrapKind::DivisionByZero);
                }
                let r = self.builder.ins().srem(a, b);
                self.push(r);
            }
            Op::Eq(IrType::I64) => self.emit_cmp(IntCC::Equal)?,
            Op::Ne(IrType::I64) => self.emit_cmp(IntCC::NotEqual)?,
            Op::Lt(IrType::I64) => self.emit_cmp(IntCC::SignedLessThan)?,
            Op::Le(IrType::I64) => self.emit_cmp(IntCC::SignedLessThanOrEqual)?,
            Op::Gt(IrType::I64) => self.emit_cmp(IntCC::SignedGreaterThan)?,
            Op::Ge(IrType::I64) => self.emit_cmp(IntCC::SignedGreaterThanOrEqual)?,
            Op::Eq(IrType::Bool) | Op::Eq(IrType::I32) => self.emit_cmp_i32(IntCC::Equal)?,
            Op::Ne(IrType::Bool) | Op::Ne(IrType::I32) => self.emit_cmp_i32(IntCC::NotEqual)?,
            Op::Return => self.emit_return()?,
            Op::LoadField { offset, ty } => self.emit_load_field(*offset, *ty)?,
            Op::StoreField { offset, ty } => self.emit_store_field(*offset, *ty)?,
            Op::Call {
                fn_index,
                arg_count,
                param_tys,
                ret_ty,
            } => self.emit_call_stdlib(*fn_index, *arg_count, param_tys, *ret_ty)?,

            // Const-data records: each `Op::ConstString` / `Op::ConstList*`
            // pushes the arena-relative i32 offset the host placed the
            // record at. The pool was scanned + laid out at compile
            // time; here we just resolve the `idx` to its offset and
            // push a constant.
            Op::ConstString { idx, .. } => {
                let off = self
                    .const_pool
                    .string_offsets
                    .get(idx)
                    .copied()
                    .ok_or_else(|| {
                        CraneliftError::Codegen(format!(
                            "ConstString idx {idx} not in pre-computed pool"
                        ))
                    })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::ConstListInt { idx, .. } => {
                let off = self
                    .const_pool
                    .list_int_offsets
                    .get(idx)
                    .copied()
                    .ok_or_else(|| {
                        CraneliftError::Codegen(format!(
                            "ConstListInt idx {idx} not in pre-computed pool"
                        ))
                    })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::ConstListFloat { idx, .. } => {
                let off = self
                    .const_pool
                    .list_float_offsets
                    .get(idx)
                    .copied()
                    .ok_or_else(|| {
                        CraneliftError::Codegen(format!(
                            "ConstListFloat idx {idx} not in pre-computed pool"
                        ))
                    })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::ConstListBool { idx, .. } => {
                let off = self
                    .const_pool
                    .list_bool_offsets
                    .get(idx)
                    .copied()
                    .ok_or_else(|| {
                        CraneliftError::Codegen(format!(
                            "ConstListBool idx {idx} not in pre-computed pool"
                        ))
                    })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }

            // Pop an i32 arena-relative pointer, push the leading
            // `[len: u32 LE]` widened to i64. Mirrors the wasm side's
            // `i32.load offset=0 align=2; i64.extend_i32_u` pair.
            Op::ReadStringLen => self.emit_read_string_len()?,
            Op::If {
                result_ty,
                then_body,
                else_body,
            } => self.emit_if(*result_ty, then_body, else_body)?,
            Op::CheckCap { cap_bit } => self.emit_check_cap(*cap_bit)?,
            Op::CallNative {
                import_idx,
                param_tys,
                ret_ty,
                cap_bit,
            } => self.emit_call_native(*import_idx, param_tys, *ret_ty, *cap_bit)?,
            Op::MakeClosure {
                fn_table_idx,
                captures,
                captures_size,
            } => self.emit_make_closure(*fn_table_idx, captures, *captures_size)?,
            Op::CallClosure { param_tys, ret_ty } => self.emit_call_closure(param_tys, *ret_ty)?,

            // v5-β-2 widen: `select` for the simple stdlib bodies
            // (`abs` / `min` / `max`) and any user expression the
            // lowering pass emits via a ternary. Stack discipline
            // mirrors wasm: pop `[val_true, val_false, cond]`,
            // push `val_true` when `cond` is non-zero, else
            // `val_false`. cranelift's `select` takes
            // `(cond, val_if_true, val_if_false)` so the operand
            // order is straightforward.
            Op::Select { ty } => {
                let cond = self.pop()?;
                let val_false = self.pop()?;
                let val_true = self.pop()?;
                // Sanity: the IR pass guarantees both arms share the
                // same wasm slot; we don't need to inspect the tag
                // beyond a sanity-check trap if a future bug feeds
                // mismatched widths.
                let _ = ty;
                let r = self.builder.ins().select(cond, val_true, val_false);
                self.push(r);
            }

            // v5-β-2 widen: structured block forms. cranelift's
            // CFG is flat blocks + terminators, but the wasm-style
            // `Block` / `Loop` here only ever appear in stdlib
            // bodies the cranelift backend will inline; emit them
            // as nested cranelift blocks with a basic label depth
            // stack so `Br` / `BrIf` find the right target. For
            // now we route them through helpers that the next
            // tranche (stdlib body inlining) will exercise.
            Op::Block { result_ty, body } => self.emit_block(*result_ty, body, false)?,
            Op::Loop { result_ty, body } => self.emit_block(*result_ty, body, true)?,
            Op::Br { label_depth } => self.emit_br(*label_depth, /*conditional=*/ false)?,
            Op::BrIf { label_depth } => self.emit_br(*label_depth, /*conditional=*/ true)?,
            Op::BrTable { default, targets } => self.emit_br_table(*default, targets)?,

            // v5-β-2 widen: arithmetic on `I32` slot (used by stdlib
            // bodies for pointer / length arithmetic against the
            // wasm linear-memory model). Same semantics as the I64
            // variants but on cranelift's `I32` type.
            Op::Add(IrType::I32) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().iadd(a, b);
                self.push(r);
            }
            Op::Sub(IrType::I32) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().isub(a, b);
                self.push(r);
            }
            Op::Mul(IrType::I32) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().imul(a, b);
                self.push(r);
            }
            Op::BitAnd(IrType::I32) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().band(a, b);
                self.push(r);
            }
            Op::Div(IrType::I32) => {
                let b = self.pop()?;
                let a = self.pop()?;
                if self.sandbox.div_check {
                    let zero = self.builder.ins().iconst(I32, 0);
                    let cmp = self.builder.ins().icmp(IntCC::Equal, b, zero);
                    self.cond_trap(cmp, TrapKind::DivisionByZero);
                }
                let r = self.builder.ins().sdiv(a, b);
                self.push(r);
            }
            Op::Mod(IrType::I32) => {
                let b = self.pop()?;
                let a = self.pop()?;
                if self.sandbox.div_check {
                    let zero = self.builder.ins().iconst(I32, 0);
                    let cmp = self.builder.ins().icmp(IntCC::Equal, b, zero);
                    self.cond_trap(cmp, TrapKind::DivisionByZero);
                }
                let r = self.builder.ins().srem(a, b);
                self.push(r);
            }
            Op::BitAnd(IrType::I64) => {
                let b = self.pop()?;
                let a = self.pop()?;
                let r = self.builder.ins().band(a, b);
                self.push(r);
            }
            Op::Lt(IrType::I32) => self.emit_cmp_i32(IntCC::SignedLessThan)?,
            Op::Le(IrType::I32) => self.emit_cmp_i32(IntCC::SignedLessThanOrEqual)?,
            Op::Gt(IrType::I32) => self.emit_cmp_i32(IntCC::SignedGreaterThan)?,
            Op::Ge(IrType::I32) => self.emit_cmp_i32(IntCC::SignedGreaterThanOrEqual)?,

            // v5-β-2 stage 3: dict-return / tail-cursor protocol.
            // Each op runs against the per-function record-local map
            // and the shared `state.tail_cursor` slot.
            Op::AllocRootRecord { record_local_idx } => {
                self.emit_alloc_root_record(*record_local_idx);
            }
            Op::AllocSubRecord {
                record_local_idx,
                root_size,
                root_align,
            } => {
                self.emit_alloc_sub_record(*record_local_idx, *root_size, *root_align)?;
            }
            Op::PushRecordBase { record_local_idx } => {
                self.emit_push_record_base(*record_local_idx)?;
            }
            Op::StoreFieldAtRecord {
                record_local_idx,
                offset,
                ty,
            } => {
                self.emit_store_field_at_record(*record_local_idx, *offset, *ty)?;
            }
            Op::EmitTailRecordFromAbsoluteAddr { ty } => {
                self.emit_tail_record_from_absolute(*ty)?;
            }

            // v5-β-2 stage 3: memory stdlib + scratch primitives.
            Op::AllocScratch { size_bytes } => {
                self.emit_alloc_scratch_static(*size_bytes)?;
            }
            Op::AllocScratchDyn => {
                self.emit_alloc_scratch_dyn()?;
            }
            Op::LoadI32AtAbsolute { offset } => {
                self.emit_load_i32_at_absolute(*offset)?;
            }
            Op::LoadI64AtAbsolute { offset } => {
                self.emit_load_i64_at_absolute(*offset)?;
            }
            Op::LoadF64AtAbsolute { offset } => {
                self.emit_load_f64_at_absolute(*offset)?;
            }
            Op::LoadI8UAtAbsolute { offset } => {
                self.emit_load_i8u_at_absolute(*offset)?;
            }
            Op::StoreI32AtAbsolute { offset } => {
                self.emit_store_i32_at_absolute(*offset)?;
            }
            Op::StoreI64AtAbsolute { offset } => {
                self.emit_store_i64_at_absolute(*offset)?;
            }
            Op::StoreF64AtAbsolute { offset } => {
                self.emit_store_f64_at_absolute(*offset)?;
            }
            Op::StoreI8AtAbsolute { offset } => {
                self.emit_store_i8_at_absolute(*offset)?;
            }
            Op::MemcpyAtAbsolute => {
                self.emit_memcpy_at_absolute()?;
            }
            Op::CaseFoldTableAddr { upper } => {
                let off = if *upper {
                    self.const_pool.case_fold_upper_offset
                } else {
                    self.const_pool.case_fold_lower_offset
                };
                let off = off.ok_or_else(|| {
                    CraneliftError::Codegen("CaseFoldTableAddr missing from const pool".into())
                })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::CombiningMarkRangesAddr => {
                let off = self.const_pool.combining_marks_offset.ok_or_else(|| {
                    CraneliftError::Codegen(
                        "CombiningMarkRangesAddr missing from const pool".into(),
                    )
                })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::WhitespaceRangesAddr => {
                let off = self.const_pool.whitespace_offset.ok_or_else(|| {
                    CraneliftError::Codegen("WhitespaceRangesAddr missing from const pool".into())
                })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::DecompTableAddr { compatibility } => {
                let off = if *compatibility {
                    self.const_pool.decomp_nfkd_offset
                } else {
                    self.const_pool.decomp_nfd_offset
                };
                let off = off.ok_or_else(|| {
                    CraneliftError::Codegen("DecompTableAddr missing from const pool".into())
                })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::CccTableAddr => {
                let off = self.const_pool.ccc_offset.ok_or_else(|| {
                    CraneliftError::Codegen("CccTableAddr missing from const pool".into())
                })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::CompositionTableAddr => {
                let off = self.const_pool.composition_offset.ok_or_else(|| {
                    CraneliftError::Codegen("CompositionTableAddr missing from const pool".into())
                })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::FullCaseFoldTableAddr { upper } => {
                let off = if *upper {
                    self.const_pool.full_case_fold_upper_offset
                } else {
                    self.const_pool.full_case_fold_lower_offset
                };
                let off = off.ok_or_else(|| {
                    CraneliftError::Codegen("FullCaseFoldTableAddr missing from const pool".into())
                })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::CasedRangesAddr => {
                let off = self.const_pool.cased_ranges_offset.ok_or_else(|| {
                    CraneliftError::Codegen("CasedRangesAddr missing from const pool".into())
                })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::CaseIgnorableRangesAddr => {
                let off = self
                    .const_pool
                    .case_ignorable_ranges_offset
                    .ok_or_else(|| {
                        CraneliftError::Codegen(
                            "CaseIgnorableRangesAddr missing from const pool".into(),
                        )
                    })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::TurkishCaseFoldTableAddr { upper } => {
                let off = if *upper {
                    self.const_pool.turkish_upper_offset
                } else {
                    self.const_pool.turkish_lower_offset
                };
                let off = off.ok_or_else(|| {
                    CraneliftError::Codegen(
                        "TurkishCaseFoldTableAddr missing from const pool".into(),
                    )
                })?;
                let v = self.builder.ins().iconst(I32, i64::from(off));
                self.push(v);
            }
            Op::Trap { kind } => {
                // `relon_ir::TrapKind` covers stdlib-domain failures
                // (`IndexOutOfBounds`, `EmptyList`, `InvalidUtf8`).
                // Map them all into the sandbox-side BoundsViolation
                // / Unreachable surface until v6-γ widens the trap
                // taxonomy. The harness's `trap_equivalent` already
                // accepts the converged shape.
                let mapped = match kind {
                    relon_ir::TrapKind::IndexOutOfBounds | relon_ir::TrapKind::EmptyList => {
                        TrapKind::BoundsViolation
                    }
                    relon_ir::TrapKind::InvalidUtf8 => TrapKind::Unreachable,
                };
                self.emit_trap(mapped)?;
            }

            // v5-β-2: every other op still surfaces as Codegen
            // failure. Items #1-#6 in the v5-β-2 plan (LoadField,
            // StoreField, scratch alloc, stdlib inlining, full
            // CallNative dispatch, real sigsetjmp) widen this list
            // incrementally — each widening is paired with a
            // corpus tier transition from CraneliftUnsupported
            // to MatchOk.
            other => {
                return Err(CraneliftError::Codegen(format!(
                    "unsupported op in v5-beta-2 stage 3: {other:?}"
                )))
            }
        }
        Ok(())
    }

    fn emit_cmp(&mut self, cc: IntCC) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().icmp(cc, a, b);
        // cranelift `icmp` produces an i8 in some versions, an i32 in
        // others; we normalise to i32 to match the IR's `Bool` slot.
        let r = self.builder.ins().uextend(I32, r);
        self.push(r);
        Ok(())
    }

    fn emit_cmp_i32(&mut self, cc: IntCC) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().icmp(cc, a, b);
        let r = self.builder.ins().uextend(I32, r);
        self.push(r);
        Ok(())
    }

    /// Lower a wasm `Block` (forward exit) or `Loop` (back edge) into
    /// cranelift's flat-CFG block form.
    ///
    /// For both shapes we create a cranelift block ahead of the body
    /// and push a `LabelFrame` onto `label_stack`; `Op::Br` /
    /// `Op::BrIf` / `Op::BrTable` resolve to that block by depth-
    /// counting from the top of the stack.
    ///
    /// * `is_loop = false` (wasm `Block`): the `target_block` is the
    ///   **continuation** block reached after the body terminates;
    ///   `Br 0` jumps forward past the body's End. When `result_ty =
    ///   Some(t)`, the continuation has one block-param of type `t`;
    ///   fallthrough at body end pops the top of the operand stack
    ///   and forwards it as the continuation arg.
    /// * `is_loop = true` (wasm `Loop`): the `target_block` is the
    ///   loop **header**; `Br 0` jumps back to re-enter the loop
    ///   (equivalent to `continue`). When `result_ty = Some(t)`, the
    ///   header has one block-param of type `t` representing the
    ///   loop-carried accumulator. Each back-edge re-supplies the
    ///   next iteration's value; the loop "exits" through fall-
    ///   through to a continuation block which inherits the final
    ///   value. The loop's seed value is popped off the operand
    ///   stack before entering the header (wasm semantics).
    ///
    /// v5-β-2 stage 5 widens this to handle `result_ty != None` via
    /// cranelift block-parameter threading. Stdlib bodies in practice
    /// still use `result_ty = None`, but the yield-shape variant
    /// surfaces clean via `Op::Loop { result_ty: Some(_) }` for hand-
    /// rolled IR + the BrTable test suite.
    fn emit_block(
        &mut self,
        result_ty: Option<IrType>,
        body: &[TaggedOp],
        is_loop: bool,
    ) -> Result<(), CraneliftError> {
        let result_cl_ty = match result_ty {
            None => None,
            Some(ty) => Some(ir_ty_to_cl(ty)?),
        };

        if is_loop {
            // Loop: branch into a fresh header block, lower the
            // body inside it. The body's terminator (Br / fallthrough
            // / Return) decides whether the loop exits or re-enters.
            let header = self.builder.create_block();
            // Loop header carries the loop-carried accumulator as a
            // block parameter when the loop yields a value. The seed
            // value is the top of the operand stack at loop entry.
            let seed = if let Some(cl_ty) = result_cl_ty {
                self.builder.append_block_param(header, cl_ty);
                Some(self.pop()?)
            } else {
                None
            };
            let seed_args: Vec<BlockArg> = seed.into_iter().map(BlockArg::from).collect();
            self.builder.ins().jump(header, &seed_args);
            self.builder.switch_to_block(header);
            // Push the header block-param onto the operand stack so
            // the body's first op consumes the loop-carried value
            // (wasm-Loop semantics: the yield value re-enters the
            // operand stack each iteration). The body is responsible
            // for stashing / using / re-yielding it before the back-
            // edge.
            if result_cl_ty.is_some() {
                let v = self.builder.block_params(header)[0];
                self.push(v);
            }
            // For yielding loops, prepare a continuation block. The
            // loop's normal fallthrough lands there carrying the
            // final accumulator as a block-param. The frame remembers
            // the continuation so back-edges can re-enter while non-
            // looping `Br N` past the loop's enclosing label still
            // lands at the right place.
            let loop_cont_block = if result_cl_ty.is_some() {
                Some(self.builder.create_block())
            } else {
                None
            };
            if let (Some(cl_ty), Some(cont)) = (result_cl_ty, loop_cont_block) {
                self.builder.append_block_param(cont, cl_ty);
            }
            // Allocate a back-edge counter if the sandbox deadline
            // check is on. Initialised to 0 here; each back-edge
            // bumps + checks at the `RESOURCE_CHECK_INTERVAL` cadence.
            let back_edge_counter = if self.sandbox.deadline_check {
                let var = self.builder.declare_var(I64);
                let zero = self.builder.ins().iconst(I64, 0);
                self.builder.def_var(var, zero);
                Some(var)
            } else {
                None
            };
            // Loops with no other entry edge get sealed once the body
            // lowers — cranelift seals retroactively for blocks with
            // forward branches, so we leave it unsealed during the
            // body walk and seal at the end.
            self.label_stack.push(LabelFrame {
                target_block: header,
                is_loop: true,
                result_cl_ty,
                loop_cont_block,
                back_edge_counter,
            });
            self.emit_body(body)?;
            let frame = self.label_stack.pop().expect("just pushed");
            self.builder.seal_block(header);
            if let Some(cont) = frame.loop_cont_block {
                // Fall through to cont with the current top-of-stack
                // as the final loop value. Skip the fall-through
                // jump when the body already terminated (the body
                // always Br-back-edged); cranelift's DCE handles
                // the dead exit path.
                if !self.builder.is_unreachable() {
                    let cont_arg = if let Some(cl_ty) = result_cl_ty {
                        if self.stack.is_empty() {
                            self.placeholder_for(cl_ty)
                        } else {
                            self.pop()?
                        }
                    } else {
                        self.builder.ins().iconst(I32, 0)
                    };
                    self.builder.ins().jump(cont, &[cont_arg.into()]);
                }
                self.builder.seal_block(cont);
                self.builder.switch_to_block(cont);
                // The continuation block-param is the loop's result;
                // push it onto the operand stack.
                let v = self.builder.block_params(cont)[0];
                self.push(v);
            }
        } else {
            // Block (forward exit): allocate a continuation block,
            // lower the body, then switch to the continuation. A
            // `Br 0` inside the body jumps forward to `cont`.
            let cont = self.builder.create_block();
            if let Some(cl_ty) = result_cl_ty {
                self.builder.append_block_param(cont, cl_ty);
            }
            self.label_stack.push(LabelFrame {
                target_block: cont,
                is_loop: false,
                result_cl_ty,
                loop_cont_block: None,
                back_edge_counter: None,
            });
            self.emit_body(body)?;
            self.label_stack.pop();
            // Fallthrough to cont when the body doesn't explicitly
            // branch out. We forward the top-of-stack value as the
            // continuation block-param when the block yields. Skip
            // the jump entirely if the body already terminated (the
            // current block is unreachable / already filled by a Br
            // / BrTable / Return / Trap).
            if !self.builder.is_unreachable() {
                let fall_args: Vec<BlockArg> = if let Some(cl_ty) = result_cl_ty {
                    let v = if !self.stack.is_empty() {
                        self.pop()?
                    } else {
                        self.placeholder_for(cl_ty)
                    };
                    vec![v.into()]
                } else {
                    Vec::new()
                };
                self.builder.ins().jump(cont, &fall_args);
            }
            self.builder.seal_block(cont);
            self.builder.switch_to_block(cont);
            // When the block yields, expose the block-param to the
            // surrounding code via the operand stack.
            if result_cl_ty.is_some() {
                let v = self.builder.block_params(cont)[0];
                self.push(v);
            }
        }
        Ok(())
    }

    /// Emit the periodic deadline guard at a loop back-edge. Bumps
    /// the frame's per-loop counter; when `(counter &
    /// (RESOURCE_CHECK_INTERVAL - 1)) == 0`, emits a resource-check
    /// guard (one host clock read + comparison). `RESOURCE_CHECK_INTERVAL`
    /// is a power of two so the modulus is cheap.
    fn emit_loop_back_resource_check(&mut self, counter_var: Variable) {
        let cur = self.builder.use_var(counter_var);
        let one = self.builder.ins().iconst(I64, 1);
        let next = self.builder.ins().iadd(cur, one);
        self.builder.def_var(counter_var, next);
        let mask = self
            .builder
            .ins()
            .iconst(I64, (crate::sandbox::RESOURCE_CHECK_INTERVAL as i64) - 1);
        let masked = self.builder.ins().band(next, mask);
        // Branch to a fresh deadline-check block when masked == 0;
        // otherwise just skip. Use brif + a tiny block layout so the
        // common (non-zero) case stays branch-predicted.
        let check_block = self.builder.create_block();
        let after_block = self.builder.create_block();
        let zero = self.builder.ins().iconst(I64, 0);
        let cmp = self.builder.ins().icmp(IntCC::Equal, masked, zero);
        self.builder
            .ins()
            .brif(cmp, check_block, &[], after_block, &[]);
        self.builder.seal_block(check_block);
        self.builder.switch_to_block(check_block);
        self.emit_resource_check();
        self.builder.ins().jump(after_block, &[]);
        self.builder.seal_block(after_block);
        self.builder.switch_to_block(after_block);
    }

    /// Lower `Op::Br { label_depth }` (unconditional) or
    /// `Op::BrIf { label_depth }` (conditional, popping the
    /// condition off the stack).
    fn emit_br(&mut self, label_depth: u32, conditional: bool) -> Result<(), CraneliftError> {
        let depth = label_depth as usize;
        if depth >= self.label_stack.len() {
            return Err(CraneliftError::Codegen(format!(
                "Br/BrIf label_depth {label_depth} out of range — only {} frame(s) on stack",
                self.label_stack.len()
            )));
        }
        let frame_idx = self.label_stack.len() - 1 - depth;
        let target = self.label_stack[frame_idx].target_block;
        let result_cl_ty = self.label_stack[frame_idx].result_cl_ty;
        let is_loop = self.label_stack[frame_idx].is_loop;
        let back_edge_counter = self.label_stack[frame_idx].back_edge_counter;

        // For yielded targets, pop the top-of-stack and forward as
        // the block-arg. We do this once for both branch shapes.
        let block_args: Vec<BlockArg> = if let Some(cl_ty) = result_cl_ty {
            let v = if !self.builder.is_unreachable() && !self.stack.is_empty() {
                self.pop()?
            } else {
                self.placeholder_for(cl_ty)
            };
            vec![v.into()]
        } else {
            Vec::new()
        };

        if conditional {
            // Pop the i32 condition. cranelift `brif(cond, then,
            // else)` needs both arms; for the "fallthrough" arm we
            // create a fresh block and switch into it after the
            // branch so subsequent ops land somewhere valid.
            let cond = self.pop()?;
            let fallthrough = self.builder.create_block();
            // RESOURCE_CHECK_INTERVAL cadence: if this branch is a
            // loop back-edge, emit the periodic deadline check
            // before the brif. We use the "then" arm (jump-to-loop-
            // header) as the loop continuation, so the check fires
            // only when the loop actually iterates.
            if is_loop {
                if let Some(counter_var) = back_edge_counter {
                    // The check needs to run conditionally — we need
                    // a separate then-arm block that holds the
                    // counter bump + check, then jumps to the loop
                    // header. The else-arm falls through unchanged.
                    let take_branch = self.builder.create_block();
                    self.builder
                        .ins()
                        .brif(cond, take_branch, &[], fallthrough, &[]);
                    self.builder.seal_block(take_branch);
                    self.builder.switch_to_block(take_branch);
                    self.emit_loop_back_resource_check(counter_var);
                    self.builder.ins().jump(target, &block_args);
                    self.builder.seal_block(fallthrough);
                    self.builder.switch_to_block(fallthrough);
                    return Ok(());
                }
            }
            self.builder
                .ins()
                .brif(cond, target, &block_args, fallthrough, &[]);
            self.builder.seal_block(fallthrough);
            self.builder.switch_to_block(fallthrough);
        } else {
            // Unconditional back-edge: also gets the cadence guard.
            if is_loop {
                if let Some(counter_var) = back_edge_counter {
                    self.emit_loop_back_resource_check(counter_var);
                }
            }
            self.builder.ins().jump(target, &block_args);
            // After an unconditional branch, the rest of the basic
            // block is unreachable. Create a fresh dummy block so
            // subsequent op emission lands somewhere; cranelift's
            // dead-block elimination will prune it.
            let dummy = self.builder.create_block();
            self.builder.seal_block(dummy);
            self.builder.switch_to_block(dummy);
        }
        Ok(())
    }

    /// Lower `Op::BrTable { default, targets }`. Pops one `i32` index
    /// from the stack; when `index < targets.len()` jumps to
    /// `targets[index]`, otherwise jumps to `default`. The label
    /// depths resolve against the same `label_stack` as `Br` / `BrIf`.
    ///
    /// Yield-typed targets are supported only when every target (incl.
    /// default) shares the same `result_cl_ty`. The IR shape
    /// guarantees this: `Op::BrTable` is a single discriminant
    /// dispatch where every arm produces a value of the same type.
    /// Mismatch surfaces as Codegen.
    fn emit_br_table(&mut self, default: u32, targets: &[u32]) -> Result<(), CraneliftError> {
        // Pop the discriminant; we'll feed it directly to br_table.
        let idx_val = self.pop()?;
        let default_depth = default as usize;
        if default_depth >= self.label_stack.len() {
            return Err(CraneliftError::Codegen(format!(
                "BrTable default depth {default} out of range — only {} frame(s) on stack",
                self.label_stack.len()
            )));
        }
        let default_frame_idx = self.label_stack.len() - 1 - default_depth;
        let default_target = self.label_stack[default_frame_idx].target_block;
        let yield_ty = self.label_stack[default_frame_idx].result_cl_ty;
        // Validate every target depth + cross-check yield types.
        for (i, depth) in targets.iter().enumerate() {
            let d = *depth as usize;
            if d >= self.label_stack.len() {
                return Err(CraneliftError::Codegen(format!(
                    "BrTable target #{i} depth {depth} out of range — only {} frame(s) on stack",
                    self.label_stack.len()
                )));
            }
            let frame = &self.label_stack[self.label_stack.len() - 1 - d];
            if frame.result_cl_ty != yield_ty {
                return Err(CraneliftError::Codegen(format!(
                    "BrTable target #{i} yield type {:?} disagrees with default {:?}",
                    frame.result_cl_ty, yield_ty
                )));
            }
        }
        // If any of the targets is a loop back-edge with a back-edge
        // counter, we can't directly weave the resource check into
        // br_table — the cadence guard belongs on the actual taken
        // arm. v5-β-2 stage 5 takes the safe approach: emit the
        // br_table without per-arm cadence, and rely on the prologue
        // + outer-loop guard for the bound. (Inner `BrIf` back-edges
        // still benefit from the cadence.) The single-deadline
        // safety net stays intact because the prologue's check
        // runs on every entry call.
        // Pop the yield value (if any) — every arm receives the same
        // value (wasm semantics: the operand stack at the BrTable
        // point is shared by every arm).
        let yield_arg: Option<CValue> = if let Some(cl_ty) = yield_ty {
            Some(
                if !self.builder.is_unreachable() && !self.stack.is_empty() {
                    self.pop()?
                } else {
                    self.placeholder_for(cl_ty)
                },
            )
        } else {
            None
        };

        // Build the BlockCalls + JumpTable. Each call carries the
        // optional yield value as its block-arg.
        let yield_args_slice: Vec<BlockArg> = yield_arg.iter().map(|v| (*v).into()).collect();
        let default_call = self
            .builder
            .func
            .dfg
            .block_call(default_target, &yield_args_slice);
        let target_calls: Vec<BlockCall> = targets
            .iter()
            .map(|depth| {
                let d = *depth as usize;
                let tgt = self.label_stack[self.label_stack.len() - 1 - d].target_block;
                self.builder.func.dfg.block_call(tgt, &yield_args_slice)
            })
            .collect();
        let jt_data = JumpTableData::new(default_call, &target_calls);
        let jt = self.builder.create_jump_table(jt_data);
        self.builder.ins().br_table(idx_val, jt);
        // After br_table the rest of the block is unreachable. Create
        // a dummy fallthrough so subsequent op emission lands somewhere.
        let dummy = self.builder.create_block();
        self.builder.seal_block(dummy);
        self.builder.switch_to_block(dummy);
        Ok(())
    }

    fn emit_if(
        &mut self,
        result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
    ) -> Result<(), CraneliftError> {
        let cond = self.pop()?;
        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let join_block = self.builder.create_block();

        let cr_ty = match result_ty {
            IrType::I64 => I64,
            IrType::I32 | IrType::Bool | IrType::Null => I32,
            _ => {
                return Err(CraneliftError::Codegen(format!(
                    "If result_ty {:?} unsupported in v5-beta-1",
                    result_ty
                )))
            }
        };
        self.builder.append_block_param(join_block, cr_ty);

        self.builder
            .ins()
            .brif(cond, then_block, &[], else_block, &[]);
        self.builder.seal_block(then_block);
        self.builder.seal_block(else_block);

        // Then-arm. Push the join block as a label frame so a nested
        // `Br 0` (or higher depths threading through `If`) finds the
        // right target — wasm semantics treat `If` as a labeled block
        // whose break target is the matching `End`.
        self.builder.switch_to_block(then_block);
        let stack_before_then = self.stack.len();
        // `If` is treated as a labelled block whose break target is the
        // matching End — but the result value is consumed via the
        // join-block phi (the explicit `If` lowering pattern) rather
        // than via the label-frame yield path. So we leave
        // `result_cl_ty = None` on the frame to avoid double-popping
        // the yield value.
        self.label_stack.push(LabelFrame {
            target_block: join_block,
            is_loop: false,
            result_cl_ty: None,
            loop_cont_block: None,
            back_edge_counter: None,
        });
        self.emit_body(then_body)?;
        self.label_stack.pop();
        // The arm may have terminated early (Br / Trap) and switched
        // to a dummy unreachable block. In that case any "value left
        // on the stack" is stale — we ignore the stack-discipline
        // check and feed cranelift a placeholder undef-like value so
        // the unreachable block still jumps to join_block with a
        // typed arg. The DCE pass drops the dummy on the floor.
        let then_result = if self.stack.len() == stack_before_then + 1 {
            self.stack.pop().unwrap()
        } else if self.stack.len() < stack_before_then {
            return Err(CraneliftError::Codegen(
                "If then-body underflowed the stack".into(),
            ));
        } else {
            // Stack drifted (e.g. Br/Trap terminated early without
            // pushing); emit an iconst placeholder so the join_block
            // edge stays typed. Codegen of subsequent ops uses the
            // join_block param, never this placeholder.
            self.placeholder_for(cr_ty)
        };
        self.builder.ins().jump(join_block, &[then_result.into()]);
        // Drop anything else the arm leaked.
        self.stack.truncate(stack_before_then);

        // Else-arm.
        self.builder.switch_to_block(else_block);
        let stack_before_else = self.stack.len();
        self.label_stack.push(LabelFrame {
            target_block: join_block,
            is_loop: false,
            result_cl_ty: None,
            loop_cont_block: None,
            back_edge_counter: None,
        });
        self.emit_body(else_body)?;
        self.label_stack.pop();
        let else_result = if self.stack.len() == stack_before_else + 1 {
            self.stack.pop().unwrap()
        } else if self.stack.len() < stack_before_else {
            return Err(CraneliftError::Codegen(
                "If else-body underflowed the stack".into(),
            ));
        } else {
            self.placeholder_for(cr_ty)
        };
        self.builder.ins().jump(join_block, &[else_result.into()]);
        self.stack.truncate(stack_before_else);

        self.builder.seal_block(join_block);
        self.builder.switch_to_block(join_block);
        let join_val = self.builder.block_params(join_block)[0];
        self.push(join_val);
        Ok(())
    }

    /// Capability gate: query the vtable via the host helper. The
    /// helper returns the raw fn pointer; the gate traps when the
    /// pointer is null.
    ///
    /// v5-beta-1 limits the lowered capability check to "presence" —
    /// the actual call_indirect that consumes the returned pointer
    /// is on the `CallNative` path, which currently sits outside the
    /// supported op envelope. The gate is still useful on its own
    /// because the analyzer / IR pass can emit `CheckCap { cap_bit }`
    /// pre-flight before a native fn the host hasn't granted, and
    /// the trap path validates the negative case end-to-end.
    fn emit_check_cap(&mut self, cap_bit: u32) -> Result<(), CraneliftError> {
        if !self.sandbox.capability_check {
            return Ok(());
        }
        if cap_bit == relon_ir::ir::NO_CAPABILITY_BIT {
            return Ok(());
        }
        let cap_bit_v = self.builder.ins().iconst(I32, i64::from(cap_bit));
        let inst = self.emit_host_fn_call(VtableSlot::RelonCapLookup, &[self.state_ptr, cap_bit_v]);
        let fn_ptr = self.builder.inst_results(inst)[0];
        let zero = self.builder.ins().iconst(self.pointer_ty, 0);
        let cmp = self.builder.ins().icmp(IntCC::Equal, fn_ptr, zero);
        self.cond_trap(cmp, TrapKind::CapabilityDenied);
        Ok(())
    }

    /// Lower `Op::CallNative { import_idx, param_tys, ret_ty, cap_bit }`.
    /// Stage 5 Phase C.1: full indirect dispatch via the capability
    /// vtable.
    ///
    /// Sequence:
    ///   1. (cap_bit != NO_CAPABILITY_BIT, capability_check on)
    ///      call `cap_lookup(state, cap_bit)` to materialise the host
    ///      fn pointer.
    ///   2. Trap with `CapabilityDenied` when the returned pointer is
    ///      null (slot not registered or denied by the host posture).
    ///   3. Build a cranelift `Signature` matching the IR-declared
    ///      `(param_tys) -> ret_ty` shape; install it as a SigRef on
    ///      the current function.
    ///   4. Pop `param_tys.len()` operands off the virtual stack and
    ///      `call_indirect(sig_ref, fn_ptr, args)`.
    ///   5. Push the (single) return value if `ret_ty != Null`.
    ///
    /// ABI: every host fn is exposed as `extern "C"` (`SystemV` calling
    /// convention) — host SDKs that register fns must transmute their
    /// concrete signature to [`crate::sandbox::HostFnPtr`] (a type-
    /// erased pointer); the cranelift call-site re-shapes the slot
    /// signature based on the IR's `param_tys + ret_ty` tag. Pointer-
    /// indirect arg types (String / List*) flow through as i32 arena
    /// offsets — the host fn is responsible for re-deriving the
    /// arena base via the sandbox state pointer if it needs the raw
    /// buffer.
    fn emit_call_native(
        &mut self,
        import_idx: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
        cap_bit: u32,
    ) -> Result<(), CraneliftError> {
        // Validate the import index. Helps surface IR-pass bugs early.
        let import = self.ir.imports.get(import_idx as usize).ok_or_else(|| {
            CraneliftError::Codegen(format!(
                "CallNative import_idx {import_idx} out of range (module has {} imports)",
                self.ir.imports.len()
            ))
        })?;
        if import.param_tys != param_tys {
            return Err(CraneliftError::Codegen(format!(
                "CallNative import #{import_idx} param shape disagreement: IR call has {:?}, import declares {:?}",
                param_tys, import.param_tys
            )));
        }
        if import.ret_ty != ret_ty {
            return Err(CraneliftError::Codegen(format!(
                "CallNative import #{import_idx} ret_ty disagreement: IR call has {:?}, import declares {:?}",
                ret_ty, import.ret_ty
            )));
        }

        // 1. cap_lookup -> fn_ptr (or null when the slot is empty).
        // Even when capability_check is OFF on the sandbox config, we
        // still need the fn pointer for the indirect call, so the
        // lookup always runs; only the null-check is gated.
        let effective_cap_bit = if cap_bit == relon_ir::ir::NO_CAPABILITY_BIT {
            // The host SDK convention is to register host fns at the
            // import's `import_idx` when no capability is required.
            // Mirror that: use `import_idx` as the lookup key so an
            // unguarded `#native` resolves to the same slot the SDK
            // populated. The vtable's `register(import_idx, fn_ptr)`
            // path is the canonical call-shape today; future host
            // SDKs may grow a separate "default cap" slot system.
            import_idx
        } else {
            cap_bit
        };
        let cap_bit_v = self.builder.ins().iconst(I32, i64::from(effective_cap_bit));
        let inst = self.emit_host_fn_call(VtableSlot::RelonCapLookup, &[self.state_ptr, cap_bit_v]);
        let fn_ptr = self.builder.inst_results(inst)[0];

        // 2. Null-check (always emitted: even with capability_check off
        //    we still need to refuse the call when the host never
        //    registered any fn at this slot; a null `call_indirect`
        //    would segfault).
        let zero = self.builder.ins().iconst(self.pointer_ty, 0);
        let cmp = self.builder.ins().icmp(IntCC::Equal, fn_ptr, zero);
        self.cond_trap(cmp, TrapKind::CapabilityDenied);

        // 3. Build the call signature mirroring (param_tys) -> ret_ty.
        let mut sig = Signature::new(CallConv::SystemV);
        for ty in param_tys {
            let cl_ty = ir_ty_to_cl(*ty)?;
            sig.params.push(AbiParam::new(cl_ty));
        }
        // Null return type means "no return value"; everything else
        // gets one return slot.
        if !matches!(ret_ty, IrType::Null) {
            let cl_ret = ir_ty_to_cl(ret_ty)?;
            sig.returns.push(AbiParam::new(cl_ret));
        }
        let sig_ref = self.builder.import_signature(sig);

        // 4. Pop args off the virtual stack (last-pushed = last arg).
        let mut args: Vec<CValue> = Vec::with_capacity(param_tys.len());
        for _ in 0..param_tys.len() {
            args.push(self.pop()?);
        }
        args.reverse();

        let call_inst = self.builder.ins().call_indirect(sig_ref, fn_ptr, &args);

        // 5. Push the return value (if any).
        if !matches!(ret_ty, IrType::Null) {
            let result = self.builder.inst_results(call_inst)[0];
            self.push(result);
        }

        Ok(())
    }

    /// Lower `Op::MakeClosure { fn_table_idx, captures, captures_size }`.
    /// Stage 5 Phase C.4.
    ///
    /// Closure handle layout (8 bytes total):
    ///   `[fn_table_idx: u32 LE][captures_ptr: u32 LE]`
    ///
    /// Layout in scratch:
    ///   1. Alloc 8 bytes for the handle (arena-relative ptr →
    ///      `handle_ptr`).
    ///   2. If `captures_size > 0`: alloc `captures_size` bytes for
    ///      the captures struct (→ `captures_ptr`); write each capture
    ///      from its let-local into the struct at the declared offset.
    ///   3. Store `fn_table_idx` at `handle_ptr + 0`.
    ///   4. Store `captures_ptr` (or 0) at `handle_ptr + 4`.
    ///   5. Push `handle_ptr` as i32 onto the operand stack.
    fn emit_make_closure(
        &mut self,
        fn_table_idx: u32,
        captures: &[relon_ir::ir::ClosureCapture],
        captures_size: u32,
    ) -> Result<(), CraneliftError> {
        // 1. Alloc 8 bytes for the handle.
        let handle_size = self.builder.ins().iconst(I32, 8);
        self.emit_alloc_scratch(handle_size)?;
        let handle_ptr = self.pop()?;

        // 2. Alloc captures struct if non-empty.
        let captures_ptr = if captures_size > 0 {
            let cs = self.builder.ins().iconst(I32, i64::from(captures_size));
            self.emit_alloc_scratch(cs)?;
            self.pop()?
        } else {
            self.builder.ins().iconst(I32, 0)
        };

        // 3. Store fn_table_idx at handle_ptr + 0.
        let fn_idx_v = self.builder.ins().iconst(I32, i64::from(fn_table_idx));
        // Use the StoreI32AtAbsolute pattern: arena_base + handle_ptr.
        let abs_handle = self.arena_addr(handle_ptr, 8)?;
        self.builder
            .ins()
            .store(MemFlags::trusted(), fn_idx_v, abs_handle, 0);
        // 4. Store captures_ptr at handle_ptr + 4.
        self.builder
            .ins()
            .store(MemFlags::trusted(), captures_ptr, abs_handle, 4);

        // 5. Write each capture from its let-local into the captures
        //    struct.
        if captures_size > 0 {
            let captures_abs = self.arena_addr(captures_ptr, captures_size)?;
            for cap in captures {
                let mapped_idx = self.remap_let_idx(cap.let_idx);
                let value = self.get_let(mapped_idx, cap.ty)?;
                let offset = i32::try_from(cap.offset).map_err(|_| {
                    CraneliftError::Codegen(format!(
                        "MakeClosure capture offset {} exceeds i32 range",
                        cap.offset
                    ))
                })?;
                self.builder
                    .ins()
                    .store(MemFlags::trusted(), value, captures_abs, offset);
            }
        }

        // 6. Push the handle_ptr onto the operand stack as the Closure
        //    i32 value.
        self.push(handle_ptr);
        Ok(())
    }

    /// Lower `Op::CallClosure { param_tys, ret_ty }`. Stage 5 Phase C.4.
    ///
    /// Stack discipline: `[Closure, arg0, arg1, ...] -> [ret_ty]`. We
    /// pop the user-visible args (in reverse), pop the closure
    /// handle, materialise the captures_ptr + fn_table_idx from the
    /// handle, look up the host fn pointer through
    /// `state.closure_table_base[fn_table_idx]`, then `call_indirect`
    /// with the prepended `(state, captures_ptr, args...)` signature.
    fn emit_call_closure(
        &mut self,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), CraneliftError> {
        // Pop user args in reverse.
        let mut user_args: Vec<CValue> = Vec::with_capacity(param_tys.len());
        for _ in 0..param_tys.len() {
            user_args.push(self.pop()?);
        }
        user_args.reverse();

        // Pop the closure handle (arena-relative i32 ptr).
        let handle_ptr = self.pop()?;

        // Load fn_table_idx + captures_ptr through the handle.
        let abs_handle = self.arena_addr(handle_ptr, 8)?;
        let fn_table_idx = self
            .builder
            .ins()
            .load(I32, MemFlags::trusted(), abs_handle, 0);
        let captures_ptr = self
            .builder
            .ins()
            .load(I32, MemFlags::trusted(), abs_handle, 4);

        // Look up host fn pointer through
        // state.closure_table_base[fn_table_idx]. Each slot is a
        // `usize` (host pointer size).
        let table_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            crate::sandbox::STATE_OFFSET_CLOSURE_TABLE_BASE,
        );
        let idx_p = self.builder.ins().uextend(self.pointer_ty, fn_table_idx);
        let stride_bits = match self.pointer_ty.bits() {
            64 => 3, // log2(8) = 3
            32 => 2, // log2(4) = 2
            _ => {
                return Err(CraneliftError::Codegen(
                    "unsupported pointer width for closure table".into(),
                ))
            }
        };
        let off = self.builder.ins().ishl_imm(idx_p, stride_bits);
        let slot_addr = self.builder.ins().iadd(table_base, off);
        let fn_ptr = self
            .builder
            .ins()
            .load(self.pointer_ty, MemFlags::trusted(), slot_addr, 0);
        // Null-check the resolved fn pointer (defensive: a
        // misconfigured closure_table_base would point at zero-filled
        // memory; a null call_indirect would segfault).
        let zero = self.builder.ins().iconst(self.pointer_ty, 0);
        let cmp = self.builder.ins().icmp(IntCC::Equal, fn_ptr, zero);
        self.cond_trap(cmp, TrapKind::CapabilityDenied);

        // Build call signature: (state, captures_ptr, params...) -> ret_ty.
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(self.pointer_ty));
        sig.params.push(AbiParam::new(I32));
        for ty in param_tys {
            sig.params.push(AbiParam::new(ir_ty_to_cl(*ty)?));
        }
        if !matches!(ret_ty, IrType::Null) {
            sig.returns.push(AbiParam::new(ir_ty_to_cl(ret_ty)?));
        }
        let sig_ref = self.builder.import_signature(sig);

        // Assemble args: [state, captures_ptr, user_args...].
        let mut call_args: Vec<CValue> = Vec::with_capacity(user_args.len() + 2);
        call_args.push(self.state_ptr);
        call_args.push(captures_ptr);
        call_args.extend(user_args);

        let inst = self
            .builder
            .ins()
            .call_indirect(sig_ref, fn_ptr, &call_args);

        if !matches!(ret_ty, IrType::Null) {
            let r = self.builder.inst_results(inst)[0];
            self.push(r);
        }
        Ok(())
    }

    /// Bump-allocate `size_bytes` inside the scratch region of the
    /// arena. Mirrors the wasm-side `emit_alloc_scratch_static`:
    ///
    /// 1. Read `state.scratch_cursor`.
    /// 2. Bounds-check `scratch_base + cursor + size <= arena_len`.
    /// 3. Bump the cursor.
    /// 4. Push the **arena-relative** offset `scratch_base + pre_cursor`
    ///    onto the virtual stack as an `i32`.
    ///
    /// The pushed value is an arena-relative i32 pointer the stdlib
    /// body's `LoadI32AtAbsolute` / `StoreI32AtAbsolute` /
    /// `MemcpyAtAbsolute` ops can dereference.
    fn emit_alloc_scratch(&mut self, size: CValue) -> Result<(), CraneliftError> {
        let cur = self.builder.ins().load(
            I32,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_SCRATCH_CURSOR,
        );
        let scratch_base = self.builder.ins().load(
            I32,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_SCRATCH_BASE,
        );
        let arena_len = self.builder.ins().load(
            I32,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_LEN,
        );
        // Bounds: scratch_base + cur + size <= arena_len.
        if self.sandbox.bounds_check {
            let base_plus_cur = self.builder.ins().iadd(scratch_base, cur);
            let end = self.builder.ins().iadd(base_plus_cur, size);
            let cmp = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, end, arena_len);
            self.cond_trap(cmp, TrapKind::BoundsViolation);
        }
        // Push the arena-relative offset (scratch_base + pre_cursor).
        let off = self.builder.ins().iadd(scratch_base, cur);
        // Bump.
        let new_cur = self.builder.ins().iadd(cur, size);
        self.builder.ins().store(
            MemFlags::trusted(),
            new_cur,
            self.state_ptr,
            STATE_OFFSET_SCRATCH_CURSOR,
        );
        self.push(off);
        Ok(())
    }

    /// Lower `Op::AllocScratchDyn`. The size is popped from the
    /// virtual stack (must be an `i32`).
    fn emit_alloc_scratch_dyn(&mut self) -> Result<(), CraneliftError> {
        let size = self.pop()?;
        self.emit_alloc_scratch(size)
    }

    /// Lower `Op::AllocScratch { size_bytes }`. The size is a
    /// compile-time constant.
    fn emit_alloc_scratch_static(&mut self, size_bytes: u32) -> Result<(), CraneliftError> {
        let size = self.builder.ins().iconst(I32, i64::from(size_bytes));
        self.emit_alloc_scratch(size)
    }

    /// Translate an arena-relative `i32` offset (top of stack) to its
    /// absolute host address. Performs the standard `arena_base + off`
    /// computation plus an optional bounds check against `arena_len`.
    /// Pushes nothing — the caller decides what to do with the
    /// returned cranelift value.
    fn arena_addr(&mut self, off_i32: CValue, slot_size: u32) -> Result<CValue, CraneliftError> {
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        if self.sandbox.bounds_check {
            let arena_len = self.builder.ins().load(
                I32,
                MemFlags::trusted(),
                self.state_ptr,
                STATE_OFFSET_ARENA_LEN,
            );
            let size = self.builder.ins().iconst(I32, i64::from(slot_size));
            let end = self.builder.ins().iadd(off_i32, size);
            let cmp = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, end, arena_len);
            self.cond_trap(cmp, TrapKind::BoundsViolation);
        }
        let off_p = self.builder.ins().uextend(self.pointer_ty, off_i32);
        Ok(self.builder.ins().iadd(arena_base, off_p))
    }

    /// Lower `Op::LoadI32AtAbsolute { offset }`. Pops an arena-
    /// relative i32 base, adds `offset`, performs the bounds check
    /// (`base + offset + 4 <= arena_len`), loads 4 bytes, and pushes
    /// the resulting i32.
    fn emit_load_i32_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 4)?;
        let v = self.builder.ins().load(I32, MemFlags::trusted(), abs, 0);
        self.push(v);
        Ok(())
    }

    /// Lower `Op::LoadI64AtAbsolute { offset }`.
    fn emit_load_i64_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 8)?;
        let v = self.builder.ins().load(I64, MemFlags::trusted(), abs, 0);
        self.push(v);
        Ok(())
    }

    /// Lower `Op::LoadF64AtAbsolute { offset }`.
    fn emit_load_f64_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 8)?;
        let v = self.builder.ins().load(
            cranelift_codegen::ir::types::F64,
            MemFlags::trusted(),
            abs,
            0,
        );
        self.push(v);
        Ok(())
    }

    /// Lower `Op::LoadI8UAtAbsolute { offset }`. Loads a single byte
    /// and zero-extends to i32.
    fn emit_load_i8u_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 1)?;
        let b = self.builder.ins().load(
            cranelift_codegen::ir::types::I8,
            MemFlags::trusted(),
            abs,
            0,
        );
        let v = self.builder.ins().uextend(I32, b);
        self.push(v);
        Ok(())
    }

    /// Lower `Op::StoreI32AtAbsolute { offset }`. Stack:
    /// `[base: i32, value: i32]`. Pops value first, then base.
    fn emit_store_i32_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let value = self.pop()?;
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 4)?;
        self.builder.ins().store(MemFlags::trusted(), value, abs, 0);
        Ok(())
    }

    /// Lower `Op::StoreI64AtAbsolute { offset }`.
    fn emit_store_i64_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let value = self.pop()?;
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 8)?;
        self.builder.ins().store(MemFlags::trusted(), value, abs, 0);
        Ok(())
    }

    /// Lower `Op::StoreF64AtAbsolute { offset }`.
    fn emit_store_f64_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let value = self.pop()?;
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 8)?;
        self.builder.ins().store(MemFlags::trusted(), value, abs, 0);
        Ok(())
    }

    /// Lower `Op::StoreI8AtAbsolute { offset }`. Pops i32 value;
    /// stores its low byte.
    fn emit_store_i8_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let value = self.pop()?;
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 1)?;
        let v8 = self
            .builder
            .ins()
            .ireduce(cranelift_codegen::ir::types::I8, value);
        self.builder.ins().store(MemFlags::trusted(), v8, abs, 0);
        Ok(())
    }

    /// Lower `Op::MemcpyAtAbsolute`. Stack: `[dest: i32, src: i32,
    /// len: i32]`. Translates each pointer through `arena_addr` and
    /// invokes libc memcpy via cranelift's `call_memcpy` helper.
    fn emit_memcpy_at_absolute(&mut self) -> Result<(), CraneliftError> {
        let len = self.pop()?;
        let src_off = self.pop()?;
        let dest_off = self.pop()?;
        // Bounds-check both pointers using the len.
        if self.sandbox.bounds_check {
            let arena_len = self.builder.ins().load(
                I32,
                MemFlags::trusted(),
                self.state_ptr,
                STATE_OFFSET_ARENA_LEN,
            );
            let dest_end = self.builder.ins().iadd(dest_off, len);
            let cmp_d = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, dest_end, arena_len);
            self.cond_trap(cmp_d, TrapKind::BoundsViolation);
            let src_end = self.builder.ins().iadd(src_off, len);
            let cmp_s = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, src_end, arena_len);
            self.cond_trap(cmp_s, TrapKind::BoundsViolation);
        }
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        let dest_p = self.builder.ins().uextend(self.pointer_ty, dest_off);
        let src_p = self.builder.ins().uextend(self.pointer_ty, src_off);
        let dest = self.builder.ins().iadd(arena_base, dest_p);
        let src = self.builder.ins().iadd(arena_base, src_p);
        let len_p = self.builder.ins().uextend(self.pointer_ty, len);
        self.builder
            .call_memcpy(self.frontend_config, dest, src, len_p);
        Ok(())
    }

    /// Lower `Op::Trap { kind }`. Unconditional branch to the trap
    /// block with the supplied kind code.
    fn emit_trap(&mut self, kind: TrapKind) -> Result<(), CraneliftError> {
        let one = self.builder.ins().iconst(I32, 1);
        self.cond_trap(one, kind);
        Ok(())
    }

    /// Emit a zero placeholder of the given cranelift type. Used to
    /// keep dead `If` arms typed when the body branched out early
    /// (Br / Trap) and didn't leave a real value on the stack.
    fn placeholder_for(&mut self, ty: cranelift_codegen::ir::Type) -> CValue {
        if ty == I64 {
            self.builder.ins().iconst(I64, 0)
        } else if ty == cranelift_codegen::ir::types::F64 {
            self.builder.ins().f64const(0.0)
        } else {
            self.builder.ins().iconst(I32, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_ir::ir::{Func, Module as IrModule, Op, TaggedOp};
    use relon_parser::TokenRange;

    /// Helper: synthesise a minimal IR module that returns
    /// `arg0 + arg1` (both `Int`).
    fn synth_add_module() -> IrModule {
        let body = vec![
            TaggedOp {
                op: Op::LocalGet(0),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::LocalGet(1),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::Add(IrType::I64),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::Return,
                range: TokenRange::default(),
            },
        ];
        let func = Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body,
            range: TokenRange::default(),
        };
        IrModule {
            imports: vec![],
            funcs: vec![func],
            entry_func_index: Some(0),
            closure_table: vec![],
        }
    }

    #[test]
    fn compile_module_rejects_non_i64_param() {
        let mut ir = synth_add_module();
        ir.funcs[0].params[0] = IrType::Bool;
        let cfg = SandboxConfig::default();
        let result = compile_module(&ir, &cfg);
        assert!(matches!(
            result,
            Err(CraneliftError::UnsupportedSignature(_))
        ));
    }

    #[test]
    fn compile_module_rejects_non_i64_return() {
        let mut ir = synth_add_module();
        ir.funcs[0].ret = IrType::Bool;
        let cfg = SandboxConfig::default();
        let result = compile_module(&ir, &cfg);
        assert!(matches!(
            result,
            Err(CraneliftError::UnsupportedSignature(_))
        ));
    }

    #[test]
    fn compile_module_emits_runnable_entry_for_add() {
        let ir = synth_add_module();
        let cfg = SandboxConfig::unchecked();
        let result = compile_module(&ir, &cfg);
        assert!(result.is_ok(), "compile failed: {:?}", result.err());
    }
}
