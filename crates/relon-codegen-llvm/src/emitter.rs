//! IR -> LLVM IR lowering.
//!
//! Phase B widens the emitter past the Phase A bootstrap envelope:
//!
//! - Two entry shapes:
//!   - **Legacy-i64**: `(I64...) -> I64` — driven by
//!     [`LlvmAotEvaluator::from_ir_direct`]. Mirrors the cranelift
//!     crate's same-named envelope; used by the Phase A bootstrap
//!     tests and the side-by-side `from_ir_direct` benchmarks.
//!   - **Buffer-protocol**: `(*state, i32 in_ptr, i32 in_len,
//!     i32 out_ptr, i32 out_cap, i64 caps) -> i32` — driven by
//!     [`LlvmAotEvaluator::from_source`]. Matches what
//!     `lower_workspace_single` emits for every user source.
//!
//! - Op set widened to the W1 / W2 production-source surface:
//!   `LocalGet`, `ConstI64` / `ConstI32` / `ConstBool`, `LetGet` /
//!   `LetSet`, `LoadField` / `StoreField` (scalar slots: I32 / I64 /
//!   F64 / Bool / Null), `Add` / `Sub` / `Mul` / `Div` / `Mod` /
//!   `BitAnd` (`I32` and `I64`), comparison ops (`Eq` / `Ne` /
//!   `Lt` / `Le` / `Gt` / `Ge` — `I32` / `I64` / `Bool` for `Eq`/`Ne`),
//!   structured control flow (`Block` / `Loop` / `Br` / `BrIf` /
//!   `If`), and `Return`.
//!
//! Ops outside the Phase B envelope (stdlib `Call`, pointer-indirect
//! `StoreField`, `MakeClosure`, sandbox-trap helpers, schema-method
//! dispatch, …) surface as [`crate::LlvmError::Codegen`]. They are
//! tracked for Phase C.
//!
//! ## Control-flow lowering vs cranelift
//!
//! Cranelift's `block-with-params` keeps phi nodes implicit (every
//! branch passes the carried values as block arguments). LLVM IR
//! requires explicit `phi` nodes per joining basic block. We avoid
//! both by spilling the IR stack through `alloca` slots whenever
//! control flow joins, and reading them back on the consumer side.
//! That mirrors how a naive byte-code-to-LLVM emitter behaves and
//! relies on LLVM's `mem2reg` pass at -O2/-O3 to turn the alloca
//! reads back into SSA values + phis. For the W1 / W2 hot loops
//! `mem2reg` collapses the alloca traffic into a single
//! loop-carried IR value (verified via `emit_ir_dump`'s output at
//! `-O2`).
//!
//! ## Stack discipline
//!
//! The IR's stack machine carries one value per push. We track the
//! per-op operand stack as `Vec<IntValue>` (every IR value the W1/W2
//! envelope produces fits in an integer type — I32 for Bool / I32-
//! tagged values, I64 for I64-tagged values). The wasm-style "every
//! value above the operand stack is unreachable after `br`" rule
//! lets us drop unconsumed stack slots silently — LLVM's verifier
//! catches missing terminators if we forget to seal a block.

use std::collections::HashMap;

use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module as LlvmModule};
use inkwell::types::{BasicMetadataTypeEnum, BasicTypeEnum};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue, IntValue, PointerValue,
};
use inkwell::{AddressSpace, IntPredicate};

use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};

use crate::error::LlvmError;
use crate::state::{
    ARENA_STATE_OFFSET_BASE, ARENA_STATE_OFFSET_SCRATCH_BASE, ARENA_STATE_OFFSET_SCRATCH_CURSOR,
    ARENA_STATE_OFFSET_TAIL_CURSOR,
};

/// Canonical export name the entry function uses in the emitted LLVM
/// module. The evaluator side `dlsym`s / `get_function`s against this
/// symbol after JIT finalize, so renaming it requires touching both
/// crates simultaneously.
pub(crate) const ENTRY_SYMBOL: &str = "relon_llvm_entry";

/// Phase D.1 dispatch-boundary fast path: a second exported entry
/// emitted alongside the buffer-protocol entry whenever the source's
/// `#main(Int...) -> Int` shape qualifies. Skips the HashMap pack +
/// arena round-trip the buffer envelope incurs, dropping the per-call
/// boundary cost from the ~650 ns band into the rust-native ballpark.
///
/// Only resolved when the evaluator's [`FastPathProfile`] is `Some`;
/// the symbol is absent from the JIT module otherwise.
pub(crate) const ENTRY_SYMBOL_FAST: &str = "relon_llvm_entry_fast";

/// Which signature the LLVM emitter should generate. Mirrors the
/// cranelift crate's `EntryShape` enum so a side-by-side comparison
/// of the two backends shares the same vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryShape {
    /// `(I64...) -> I64`. The Phase A bootstrap envelope — used by
    /// `from_ir_direct` callers (tests, helloworld_arith fixtures).
    LegacyI64,
    /// `(*state, i32 in_ptr, i32 in_len, i32 out_ptr, i32 out_cap,
    /// i64 caps) -> i32`. The shape `lower_workspace_single`
    /// synthesises for every user `#main` source. State is the
    /// first parameter to match the cranelift backend's
    /// `BufferEntryFn` layout.
    Buffer,
}

/// Phase D.1 fast-path profile: describes a `#main(Int...) -> Int`
/// source shape eligible for the typed legacy-i64 dispatch fast path.
///
/// The profile maps each declared `#main` Int parameter's buffer
/// offset to the LLVM fast entry's i64 positional slot, and records
/// the offset of the single Int return slot so the trailing
/// `StoreField` can be rewritten into a `ret`. Used exclusively by
/// [`emit_fast_entry`].
#[derive(Debug, Clone)]
pub(crate) struct FastPathProfile {
    /// One entry per declared `#main` arg: the field's byte offset in
    /// the input buffer (matches what `LoadField { offset }` carries
    /// in the IR body) and the i64 slot index in the fast entry
    /// signature. Vector order parallels schema declaration order.
    pub(crate) arg_offsets: Vec<u32>,
    /// Byte offset of the single `value` field in the return buffer.
    /// The trailing `StoreField { offset, ty: I64 }` whose offset
    /// matches this value gets rewritten into a `ret` on the value
    /// (after popping the IR stack normally). Any other `StoreField`
    /// surfaces as an emitter error — the fast path only handles
    /// single-value-wrapper returns.
    pub(crate) ret_offset: u32,
}

/// Phase E.1: per-module const-pool blob laid out at compile time and
/// copied into the arena prefix on every dispatch. Mirrors
/// `relon_codegen_native::codegen::ConstPool` (shape only — the LLVM
/// side keeps it scoped to this crate so the dep direction stays
/// one-way).
///
/// Layout: `[len: u32 LE][utf8 bytes]` records emitted in IR-walk
/// order, aligned to 4. Each `Op::ConstString { idx }` resolves to
/// `string_offsets[idx]` — the byte offset of its record inside
/// [`Self::bytes`] (= the arena-relative offset once the host has
/// copied the blob to the arena prefix).
#[derive(Debug, Default, Clone)]
pub struct ConstPool {
    /// `idx -> byte offset within `bytes`. The emitter materialises
    /// `Op::ConstString { idx }` as `iconst(I32, string_offsets[idx])`.
    pub string_offsets: std::collections::HashMap<u32, u32>,
    /// Materialised bytes in record order. The host trampoline copies
    /// these verbatim to `arena[..bytes.len()]` before every dispatch.
    pub bytes: Vec<u8>,
}

impl ConstPool {
    /// Build the pool by walking every function body in `module` and
    /// collecting each unique `Op::ConstString { idx, value }`. Records
    /// are laid out in walk-order with 4-byte alignment.
    pub fn from_module(module: &IrModule) -> Result<Self, LlvmError> {
        let mut pool = ConstPool::default();
        for func in &module.funcs {
            pool.collect_body(&func.body)?;
        }
        Ok(pool)
    }

    fn collect_body(&mut self, body: &[TaggedOp]) -> Result<(), LlvmError> {
        for tagged in body {
            self.collect_op(&tagged.op)?;
        }
        Ok(())
    }

    fn collect_op(&mut self, op: &Op) -> Result<(), LlvmError> {
        match op {
            Op::ConstString { idx, value } => self.add_string(*idx, value),
            Op::Block { body, .. } | Op::Loop { body, .. } => self.collect_body(body),
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                self.collect_body(then_body)?;
                self.collect_body(else_body)
            }
            // Op::Call inlines a bundled-stdlib body whose own
            // `Op::ConstString` literals must also land in the pool —
            // mirror cranelift's recursion through `builtin_stdlib`.
            Op::Call { fn_index, .. } => {
                let stdlib = relon_ir::stdlib::builtin_stdlib();
                if let Some(callee) = stdlib.get(*fn_index as usize) {
                    let body = callee.body_owned();
                    self.collect_body(&body)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn add_string(&mut self, idx: u32, value: &str) -> Result<(), LlvmError> {
        if self.string_offsets.contains_key(&idx) {
            return Ok(());
        }
        // Align to 4 so the `[len: u32]` header lands on a 4-byte
        // boundary — i32 loads through the JIT use `align=4` and we
        // don't want an unaligned trap on hosts where it matters.
        let rem = self.bytes.len() % 4;
        if rem != 0 {
            self.bytes.resize(self.bytes.len() + (4 - rem), 0);
        }
        let off = u32::try_from(self.bytes.len())
            .map_err(|_| LlvmError::Codegen("llvm const pool exceeds u32 range".into()))?;
        let len = u32::try_from(value.len())
            .map_err(|_| LlvmError::Codegen("ConstString length exceeds u32 range".into()))?;
        self.bytes.extend_from_slice(&len.to_le_bytes());
        self.bytes.extend_from_slice(value.as_bytes());
        self.string_offsets.insert(idx, off);
        Ok(())
    }
}

/// IR param signature that triggers [`EntryShape::Buffer`]. Mirrors
/// `is_buffer_protocol_signature` on the cranelift side.
pub(crate) fn is_buffer_protocol_signature(params: &[IrType], ret: IrType) -> bool {
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

/// Phase E.2 multi-function emit: lower every reachable IR function
/// into LLVM. The entry function `entry` is emitted under either the
/// legacy-i64 or buffer-protocol shape; each entry in `helpers` is
/// emitted as a sibling helper function with a plain typed
/// `(params...) -> ret` signature so the entry's `Op::Call` lowering
/// can route to it through a direct LLVM `call` instruction.
///
/// `helper_ir_indices` parallels `helpers`: entry `i` carries the
/// IR-side `funcs` index for the matching helper. Used by the
/// `Op::Call` lowering to resolve `fn_index - stdlib_count` back to the
/// matching `FunctionValue`.
///
/// `const_pool` ships the per-module ConstString blob the entry +
/// helper bodies index into via `Op::ConstString { idx }`. The host
/// copies `const_pool.bytes` to the arena prefix before every
/// dispatch so the materialised `iconst(I32, offset)` resolves to a
/// stable address.
///
/// Returns the entry `FunctionValue`, the detected entry shape, and the
/// helper lookup table the `Emit` driver hands off to the per-function
/// lowering so sibling calls can find their callee.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_module_funcs<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    entry: &Func,
    buffer_return_size: u32,
    const_pool: &ConstPool,
    helpers: &[&Func],
    helper_ir_indices: Option<&[u32]>,
) -> Result<
    (
        FunctionValue<'ctx>,
        EntryShape,
        HashMap<u32, FunctionValue<'ctx>>,
    ),
    LlvmError,
> {
    // Step 0: declare module-level intrinsics. `llvm.trap` is shared
    // by every Div / Mod sandbox guard so a single declaration covers
    // every per-op guard across every emitted function.
    declare_llvm_trap(ctx, module);

    // Step 1: declare every helper up-front so the entry / sibling
    // bodies can resolve forward references (mutual recursion, the
    // `fib(n - 1) + fib(n - 2)` self-call). LLVM is happy to issue
    // `call @foo` against a declared-only function; the body is
    // attached on the second pass.
    let mut helper_table: HashMap<u32, FunctionValue<'ctx>> = HashMap::new();
    if let Some(ir_indices) = helper_ir_indices {
        if ir_indices.len() != helpers.len() {
            return Err(LlvmError::Codegen(format!(
                "emit_module_funcs: helpers.len()={} but helper_ir_indices.len()={}",
                helpers.len(),
                ir_indices.len()
            )));
        }
    }
    for (i, helper) in helpers.iter().enumerate() {
        let fv = declare_helper_function(ctx, module, helper, i)?;
        let ir_idx = helper_ir_indices.map(|v| v[i]).unwrap_or(i as u32);
        helper_table.insert(ir_idx, fv);
    }

    // Step 2: emit the entry function body.
    let (entry_fn, shape) = if is_buffer_protocol_signature(&entry.params, entry.ret) {
        let fv = emit_buffer_entry_with_helpers(
            ctx,
            module,
            entry,
            buffer_return_size,
            const_pool,
            &helper_table,
        )?;
        (fv, EntryShape::Buffer)
    } else {
        // The legacy-i64 entry shape covers hand-built fixtures only; it
        // never references ConstString and supplies its own empty pool
        // inside `emit_legacy_entry_impl`.
        let fv = emit_legacy_entry_with_helpers(ctx, module, entry, &helper_table)?;
        (fv, EntryShape::LegacyI64)
    };

    // Step 3: emit each helper body now that every callee is declared.
    for helper in helpers.iter() {
        let helper_fn = helper_table
            .values()
            .find(|fv| {
                // Locate the FunctionValue by name; cheap enough — the
                // helper table is tiny and the find runs once per
                // helper.
                let expected = format!("relon_helper_{}", helper.name);
                fv.get_name().to_string_lossy() == expected
            })
            .copied()
            .ok_or_else(|| {
                LlvmError::Codegen(format!(
                    "emit_module_funcs: helper `{}` declared but FunctionValue missing",
                    helper.name
                ))
            })?;
        emit_helper_body(ctx, module, helper, helper_fn, const_pool, &helper_table)?;
    }

    Ok((entry_fn, shape, helper_table))
}

/// Declare a sibling helper function's LLVM signature without emitting
/// its body. Used to seat every helper into the module so the entry's
/// `Op::Call` lowering can resolve forward references (recursion,
/// mutual recursion). Sibling helpers use a plain typed
/// `(params...) -> ret` shape — no `*state` pointer, no buffer
/// protocol; the test harness drives recursive Int-only functions
/// directly. When the IR layer grows first-class closure values
/// (Phase F), this signature widens to carry `(*state, captures, ...)`.
fn declare_helper_function<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    slot: usize,
) -> Result<FunctionValue<'ctx>, LlvmError> {
    let mut param_types: Vec<BasicMetadataTypeEnum<'ctx>> = Vec::with_capacity(func.params.len());
    for (i, p) in func.params.iter().enumerate() {
        let bt = ir_ty_to_llvm_basic(ctx, *p).ok_or_else(|| {
            LlvmError::UnsupportedSignature(format!(
                "llvm-aot: helper `{}` param #{i} type {p:?} unsupported",
                func.name
            ))
        })?;
        param_types.push(basic_to_metadata(bt));
    }
    let ret_bt = ir_ty_to_llvm_basic(ctx, func.ret).ok_or_else(|| {
        LlvmError::UnsupportedSignature(format!(
            "llvm-aot: helper `{}` return type {:?} unsupported",
            func.name, func.ret
        ))
    })?;
    let fn_type = match ret_bt {
        BasicTypeEnum::IntType(t) => t.fn_type(&param_types, false),
        BasicTypeEnum::FloatType(t) => t.fn_type(&param_types, false),
        BasicTypeEnum::PointerType(t) => t.fn_type(&param_types, false),
        other => {
            return Err(LlvmError::Codegen(format!(
                "llvm-aot: helper `{}` ret BasicType {other:?} unsupported",
                func.name
            )));
        }
    };
    // Use a deterministic LLVM symbol so the entry's call site can be
    // pretty-printed in the IR dump. The slot keeps multiple helpers
    // with the same source name (shouldn't happen, but cheap) from
    // colliding.
    let _ = slot;
    let llvm_name = format!("relon_helper_{}", func.name);
    let fv = module.add_function(&llvm_name, fn_type, Some(Linkage::Internal));
    Ok(fv)
}

/// Phase E.2: declare the `llvm.trap` intrinsic on `module` if it is
/// not already present. The intrinsic has signature `void @llvm.trap()`
/// — calling it raises a target-specific trap (a `ud2` on x86-64) that
/// the host's `panic` handler can catch when paired with an
/// `unreachable`. Cheap to call on every emit pass; we keep the lookup
/// idempotent so test fixtures that re-enter the emitter don't end up
/// with duplicate declarations.
fn declare_llvm_trap<'ctx>(ctx: &'ctx Context, module: &LlvmModule<'ctx>) -> FunctionValue<'ctx> {
    if let Some(f) = module.get_function("llvm.trap") {
        return f;
    }
    let void_t = ctx.void_type();
    let fn_ty = void_t.fn_type(&[], false);
    module.add_function("llvm.trap", fn_ty, None)
}

fn ir_ty_to_llvm_basic<'ctx>(ctx: &'ctx Context, ty: IrType) -> Option<BasicTypeEnum<'ctx>> {
    match ty {
        IrType::I64 => Some(ctx.i64_type().into()),
        IrType::I32 | IrType::Bool | IrType::Null => Some(ctx.i32_type().into()),
        IrType::F64 => Some(ctx.f64_type().into()),
        // Pointer-indirect leaves carry an i32 buffer-relative offset
        // (matches the cranelift `ir_ty_to_cl` widening). The IR-side
        // tag is preserved; the LLVM slot is plain i32.
        IrType::String
        | IrType::ListInt
        | IrType::ListFloat
        | IrType::ListBool
        | IrType::ListString
        | IrType::ListSchema
        | IrType::Closure => Some(ctx.i32_type().into()),
    }
}

fn basic_to_metadata(bt: BasicTypeEnum<'_>) -> BasicMetadataTypeEnum<'_> {
    match bt {
        BasicTypeEnum::IntType(t) => t.into(),
        BasicTypeEnum::FloatType(t) => t.into(),
        BasicTypeEnum::PointerType(t) => t.into(),
        BasicTypeEnum::ArrayType(t) => t.into(),
        BasicTypeEnum::StructType(t) => t.into(),
        BasicTypeEnum::VectorType(t) => t.into(),
        BasicTypeEnum::ScalableVectorType(t) => t.into(),
    }
}

/// Lower a sibling helper's body against its declared LLVM
/// `FunctionValue`. Mirrors [`emit_legacy_entry`] but without enforcing
/// the legacy-i64 envelope — helpers may carry any
/// [`IrType`]-shaped param / return mix that `ir_ty_to_llvm_basic`
/// accepts.
fn emit_helper_body<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    llvm_fn: FunctionValue<'ctx>,
    const_pool: &ConstPool,
    helper_table: &HashMap<u32, FunctionValue<'ctx>>,
) -> Result<(), LlvmError> {
    let entry_bb = ctx.append_basic_block(llvm_fn, "entry");
    let builder = ctx.create_builder();
    builder.position_at_end(entry_bb);

    let mut emit = Emit::new(
        ctx,
        &builder,
        module,
        llvm_fn,
        EntryShape::LegacyI64,
        /*arena_base_ptr=*/ None,
        /*state_ptr=*/ None,
        /*buffer_return_size=*/ 0,
        const_pool,
    );
    // Helper functions have no implicit state slot; `LocalGet(0)` maps
    // straight to LLVM param 0.
    emit.param_base = 0;
    emit.helper_table = Some(helper_table.clone());
    // Record the IR-declared return type so `Op::Return` knows what to
    // widen / truncate to when the operand stack value's width differs
    // from the LLVM signature's return slot.
    emit.helper_ret_ty = Some(func.ret);
    emit.llvm_trap_fn = Some(declare_llvm_trap(ctx, module));
    emit.lower_body(&func.body)?;
    Ok(())
}

/// Phase D.1: emit a typed `(i64, i64, ...) -> i64` fast entry
/// alongside the buffer-protocol entry. Reuses the IR body's op
/// stream but rewrites every buffer-protocol `LoadField` into a
/// direct LLVM param read (via `profile.arg_offsets`) and every
/// trailing `StoreField` at the return-value offset into a `ret`
/// against the stashed value.
///
/// Returns `Err` when the IR contains ops outside the fast-path
/// envelope (string ops, sandbox traps, pointer-indirect StoreField,
/// stdlib calls — anything that escapes the simple Int-arithmetic
/// loop). The evaluator side surfaces this as "fast path unavailable;
/// fall back to the buffer entry" rather than a hard error so adding
/// more workloads doesn't risk regressing the buffer path.
pub(crate) fn emit_fast_entry<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    profile: &FastPathProfile,
) -> Result<FunctionValue<'ctx>, LlvmError> {
    if !is_buffer_protocol_signature(&func.params, func.ret) {
        return Err(LlvmError::UnsupportedSignature(
            "fast-path entry requires buffer-protocol IR".into(),
        ));
    }
    let arity = profile.arg_offsets.len();
    if arity > 8 {
        // Cap at 8 to keep the typed dispatch table in evaluator.rs
        // finite. Sources with arity > 8 stay on the buffer path —
        // their boundary cost is amortised across more work anyway.
        return Err(LlvmError::UnsupportedSignature(format!(
            "fast-path entry: arity {arity} exceeds cap of 8"
        )));
    }

    let i64_t = ctx.i64_type();
    let param_types: Vec<BasicMetadataTypeEnum<'ctx>> = (0..arity).map(|_| i64_t.into()).collect();
    let fn_type = i64_t.fn_type(&param_types, false);
    let llvm_fn = module.add_function(ENTRY_SYMBOL_FAST, fn_type, None);

    let entry_bb = ctx.append_basic_block(llvm_fn, "fast_entry");
    let builder = ctx.create_builder();
    builder.position_at_end(entry_bb);

    // Reserve an alloca for the return value. The fast emitter
    // rewrites the trailing `StoreField` (which under buffer protocol
    // writes the i64 result into the arena) to a store into this
    // slot; the implicit `Op::Return` at end-of-body loads from the
    // slot and `ret`s it. Placing the alloca in the entry block lets
    // LLVM's mem2reg promote it to SSA across the loop boundary.
    let ret_slot = builder
        .build_alloca(i64_t, "fast_ret_slot")
        .map_err(|e| LlvmError::Codegen(format!("fast ret_slot alloca: {e}")))?;
    // Initialise to 0 so any early `Op::Return` (no value path) still
    // produces a defined value — matches the buffer entry's
    // "ret root_size when no scalar stored" envelope.
    builder
        .build_store(ret_slot, i64_t.const_zero())
        .map_err(|e| LlvmError::Codegen(format!("fast ret_slot init: {e}")))?;

    // The fast entry is a typed `(i64...) -> i64` shape derived from
    // the buffer-protocol IR after the dispatch-boundary rewrite. It
    // doesn't touch the const-data pool (the IR only contains scalar
    // arithmetic ops) so we hand it an empty pool to keep
    // `Emit::new` polymorphic.
    let empty_pool = ConstPool::default();
    let mut emit = Emit::new(
        ctx,
        &builder,
        module,
        llvm_fn,
        EntryShape::LegacyI64,
        /*arena_base_ptr=*/ None,
        /*state_ptr=*/ None,
        /*buffer_return_size=*/ 0,
        &empty_pool,
    );
    emit.fast_path = Some(FastEmit {
        profile: profile.clone(),
        ret_slot,
    });
    // LLVM param i corresponds to arg i — no implicit state slot for
    // the fast entry. `LocalGet` should never appear in the body
    // because the IR producer only emits LocalGet for the handshake
    // params (which the fast path doesn't pass).
    emit.param_base = 0;
    emit.llvm_trap_fn = Some(declare_llvm_trap(ctx, module));
    emit.lower_body(&func.body)?;

    // The buffer-protocol IR ends with `Op::Return` which the fast
    // emitter rewrote into a load+ret. If the body fell through
    // without an explicit Return (shouldn't happen for well-formed
    // `#main` IR, but be defensive), seal it with a load+ret.
    if let Some(cur) = builder.get_insert_block() {
        if cur.get_terminator().is_none() {
            let v = builder
                .build_load(i64_t, ret_slot, "fast_ret_load")
                .map_err(|e| LlvmError::Codegen(format!("fast trailing load: {e}")))?
                .into_int_value();
            builder
                .build_return(Some(&v))
                .map_err(|e| LlvmError::Codegen(format!("fast trailing ret: {e}")))?;
        }
    }

    Ok(llvm_fn)
}

// ---------------------------------------------------------------------------
// Legacy-i64 entry (Phase A bootstrap envelope, retained for tests)
// ---------------------------------------------------------------------------

fn emit_legacy_entry_with_helpers<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    helper_table: &HashMap<u32, FunctionValue<'ctx>>,
) -> Result<FunctionValue<'ctx>, LlvmError> {
    emit_legacy_entry_impl(ctx, module, func, Some(helper_table))
}

/// Emit a Phase-A `(I64...) -> I64` function. Used by tests + the
/// Phase A bootstrap benchmarks that exercise the hand-built IR
/// fixtures directly (no buffer-protocol wrapping).
fn emit_legacy_entry_impl<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    helper_table: Option<&HashMap<u32, FunctionValue<'ctx>>>,
) -> Result<FunctionValue<'ctx>, LlvmError> {
    for (i, p) in func.params.iter().enumerate() {
        if *p != IrType::I64 {
            return Err(LlvmError::UnsupportedSignature(format!(
                "llvm-aot: legacy-i64 envelope expects I64 param at #{i}, got {p:?}"
            )));
        }
    }
    if func.ret != IrType::I64 {
        return Err(LlvmError::UnsupportedSignature(format!(
            "llvm-aot: legacy-i64 envelope expects I64 return, got {:?}",
            func.ret
        )));
    }

    let i64_t = ctx.i64_type();
    let param_types: Vec<BasicMetadataTypeEnum<'ctx>> =
        (0..func.params.len()).map(|_| i64_t.into()).collect();
    let fn_type = i64_t.fn_type(&param_types, false);
    let llvm_fn = module.add_function(ENTRY_SYMBOL, fn_type, None);

    let entry_bb = ctx.append_basic_block(llvm_fn, "entry");
    let builder = ctx.create_builder();
    builder.position_at_end(entry_bb);

    // Legacy-i64 entry shape only consumes the hand-built fixtures
    // (helloworld_arith) which never reference ConstString — an empty
    // pool is enough.
    let empty_pool = ConstPool::default();
    let mut emit = Emit::new(
        ctx,
        &builder,
        module,
        llvm_fn,
        EntryShape::LegacyI64,
        None,
        None,
        /*buffer_return_size=*/ 0,
        &empty_pool,
    );
    // Param order under the legacy envelope: every IR LocalGet(i)
    // maps to llvm_fn.param(i) — no implicit state slot.
    emit.param_base = 0;
    if let Some(table) = helper_table {
        emit.helper_table = Some(table.clone());
    }
    emit.llvm_trap_fn = Some(declare_llvm_trap(ctx, module));
    emit.lower_body(&func.body)?;

    Ok(llvm_fn)
}

// ---------------------------------------------------------------------------
// Buffer-protocol entry (Phase B production envelope)
// ---------------------------------------------------------------------------

fn emit_buffer_entry_with_helpers<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    buffer_return_size: u32,
    const_pool: &ConstPool,
    helper_table: &HashMap<u32, FunctionValue<'ctx>>,
) -> Result<FunctionValue<'ctx>, LlvmError> {
    emit_buffer_entry_impl(
        ctx,
        module,
        func,
        buffer_return_size,
        const_pool,
        Some(helper_table),
    )
}

/// Emit the buffer-protocol entry function. The cranelift backend's
/// equivalent lives in `relon-codegen-native::codegen::mod.rs` —
/// signature mirrored here so a host that holds either evaluator
/// can dispatch through the same `(state, in_ptr, …)` argv shape.
fn emit_buffer_entry_impl<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    buffer_return_size: u32,
    const_pool: &ConstPool,
    helper_table: Option<&HashMap<u32, FunctionValue<'ctx>>>,
) -> Result<FunctionValue<'ctx>, LlvmError> {
    let i32_t = ctx.i32_type();
    let i64_t = ctx.i64_type();
    let ptr_t = ctx.ptr_type(AddressSpace::default());

    // (*state, i32 in_ptr, i32 in_len, i32 out_ptr, i32 out_cap, i64 caps) -> i32
    let param_types: Vec<BasicMetadataTypeEnum<'ctx>> = vec![
        ptr_t.into(),
        i32_t.into(),
        i32_t.into(),
        i32_t.into(),
        i32_t.into(),
        i64_t.into(),
    ];
    let fn_type = i32_t.fn_type(&param_types, false);
    let llvm_fn = module.add_function(ENTRY_SYMBOL, fn_type, None);

    let entry_bb = ctx.append_basic_block(llvm_fn, "entry");
    let builder = ctx.create_builder();
    builder.position_at_end(entry_bb);

    // Resolve the per-call arena base once at function entry. The
    // LoadField / StoreField helpers consume this cached value so
    // the JIT doesn't reload `state->arena_base` on every access.
    let state_param = llvm_fn
        .get_nth_param(0)
        .ok_or_else(|| LlvmError::Codegen("buffer entry missing state param".into()))?
        .into_pointer_value();

    // Pointer arithmetic on the state struct: GEP by ARENA_STATE_OFFSET_BASE
    // bytes through an i8 view, then load the `usize` arena base.
    // We use opaque pointers so the GEP element type only matters
    // for the offset calculation.
    let i8_t = ctx.i8_type();
    let arena_base_gep = unsafe {
        builder
            .build_in_bounds_gep(
                i8_t,
                state_param,
                &[i32_t.const_int(ARENA_STATE_OFFSET_BASE as u64, false)],
                "arena_base_gep",
            )
            .map_err(|e| LlvmError::Codegen(format!("arena_base GEP: {e}")))?
    };
    // `arena_base` is `usize`. On every supported host that's i64
    // (we only target x86_64 today; the inkwell feature set in the
    // Cargo.toml is `target-x86`). If we add a 32-bit host the
    // load type needs to follow `pointer_type` width — Phase B
    // assumes the workspace's only target is 64-bit.
    let arena_base_int = builder
        .build_load(i64_t, arena_base_gep, "arena_base")
        .map_err(|e| LlvmError::Codegen(format!("arena_base load: {e}")))?
        .into_int_value();
    let arena_base_ptr = builder
        .build_int_to_ptr(arena_base_int, ptr_t, "arena_base_ptr")
        .map_err(|e| LlvmError::Codegen(format!("arena_base inttoptr: {e}")))?;

    // Phase E.1 prologue: init `state.tail_cursor = buffer_return_size`
    // so the first pointer-indirect StoreField lands past the fixed
    // area. Cheap (one store per call) — keeping it unconditional
    // avoids a body pre-scan. Bodies that never touch the tail
    // cursor pay the dead store; mem2reg / DSE eliminate it at -O3.
    let tail_init_gep = unsafe {
        builder
            .build_in_bounds_gep(
                i8_t,
                state_param,
                &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_TAIL_CURSOR), false)],
                "tail_cursor_init_gep",
            )
            .map_err(|e| LlvmError::Codegen(format!("tail_cursor init GEP: {e}")))?
    };
    let tail_init = i32_t.const_int(u64::from(buffer_return_size), false);
    builder
        .build_store(tail_init_gep, tail_init)
        .map_err(|e| LlvmError::Codegen(format!("tail_cursor init store: {e}")))?;

    let mut emit = Emit::new(
        ctx,
        &builder,
        module,
        llvm_fn,
        EntryShape::Buffer,
        Some(arena_base_ptr),
        Some(state_param),
        buffer_return_size,
        const_pool,
    );
    // Buffer-protocol LocalGet(0..=3) reads the four i32 handshake
    // slots; LocalGet(4) reads the i64 `caps` slot. The state
    // pointer occupies slot 0 in the LLVM function — IR locals
    // start at +1 from there.
    emit.param_base = 1;
    if let Some(table) = helper_table {
        emit.helper_table = Some(table.clone());
    }
    emit.llvm_trap_fn = Some(declare_llvm_trap(ctx, module));
    emit.lower_body(&func.body)?;

    Ok(llvm_fn)
}

// ---------------------------------------------------------------------------
// Per-function emitter state
// ---------------------------------------------------------------------------

/// Per-function emitter state. Holds the inkwell builder borrow,
/// the LLVM function the emit targets, the IR's operand stack, and
/// the alloca slots backing `LetSet` / `LetGet`.
///
/// `param_base` accounts for the entry-shape's implicit param slot:
/// the buffer-protocol entry has the `*state` pointer at LLVM param
/// 0, so `LocalGet(0)` resolves to LLVM param 1. The legacy-i64
/// entry has no implicit slot, so `param_base = 0`.
struct Emit<'ctx, 'b, 'cp> {
    ctx: &'ctx Context,
    builder: &'b Builder<'ctx>,
    func: FunctionValue<'ctx>,
    /// Phase F.1: cached module reference so per-op lowering can
    /// declare extern symbols (the F.1 `str.contains` host shim) on
    /// demand without threading the module through every helper. The
    /// reference is borrowed for the emit pass only; `inkwell` keeps
    /// `Module` and `FunctionValue` lifetimes orthogonal so a borrow
    /// here doesn't conflict with the surrounding `add_function`
    /// calls in the entry/helper emit paths.
    module: &'b LlvmModule<'ctx>,
    shape: EntryShape,
    /// Cached `arena_base` pointer for the buffer-protocol entry.
    /// `None` for the legacy entry shape — `LoadField` / `StoreField`
    /// reject themselves before reaching for this value.
    arena_base_ptr: Option<PointerValue<'ctx>>,
    /// Cached state-pointer LLVM value (param 0 of the buffer entry).
    /// Phase E.1 uses it to load / store the per-call tail-cursor /
    /// scratch-cursor / scratch-base slots. `None` outside the
    /// buffer-protocol entry shape.
    state_ptr: Option<PointerValue<'ctx>>,
    /// Operand stack mirroring the IR's virtual stack. Every value
    /// in flight is an LLVM integer of the matching IR type. The
    /// pair tags the IR type so consumers can pick the right
    /// signed / unsigned predicate without re-deriving it.
    stack: Vec<TypedValue<'ctx>>,
    /// `LetSet { idx }` alloca slots, keyed by `(idx, ty)`. Each
    /// idx has at most one type at a time — the IR lowering pass
    /// guarantees no aliasing between idx's of different types.
    let_slots: std::collections::HashMap<u32, (PointerValue<'ctx>, IrType)>,
    /// LLVM param offset corresponding to `LocalGet(0)`. See
    /// [`Self::lookup_param`] — `param_base + idx` is the LLVM
    /// param index.
    param_base: u32,
    /// Label stack carrying the (entry_bb, exit_bb, kind) of every
    /// nested [`Op::Block`] / [`Op::Loop`]. `Br { label_depth }`
    /// indexes from the back (depth 0 = innermost). `Block`s exit
    /// to their tail; `Loop`s exit to their head.
    label_stack: Vec<LabelFrame<'ctx>>,
    /// Monotonic counter to mint unique LLVM basic block / value
    /// names so the dumped IR is human-readable.
    name_seq: u32,
    /// Phase B: hard-coded `return_root_size` returned from a
    /// buffer-protocol `Op::Return`. The IR producer leaves no
    /// value on the operand stack for `Return` under buffer
    /// protocol — the trampoline reads back `bytes_written` to
    /// decode the output record. We hard-code this to the schema's
    /// `return_layout.root_size`, passed in at emit time.
    buffer_return_size: u32,
    /// Phase D.1: set when emitting the fast-path entry. The
    /// `Op::LoadField` / `Op::StoreField` / `Op::Return` lowering
    /// branches consult this to rewrite the buffer-protocol IR
    /// against the typed `(i64...) -> i64` LLVM signature.
    fast_path: Option<FastEmit<'ctx>>,
    /// Phase E.2 multi-function lookup: when populated, `Op::Call`
    /// with `fn_index >= stdlib_function_count()` resolves to the
    /// matching sibling `FunctionValue` and emits a direct LLVM
    /// `call`. The map is keyed by IR-side `funcs` index (i.e.
    /// `fn_index - stdlib_count`). Empty for hand-built fixtures that
    /// never reference user-defined functions.
    helper_table: Option<HashMap<u32, FunctionValue<'ctx>>>,
    /// Phase E.2: when emitting a helper body (not the entry), this
    /// carries the IR-declared return type so `Op::Return` can pick
    /// the right LLVM `ret` shape. `None` while lowering the entry
    /// body — the entry's return shape is dictated by `EntryShape`.
    helper_ret_ty: Option<IrType>,
    /// Phase E.2: cached `llvm.trap` intrinsic `FunctionValue`. The
    /// intrinsic is declared once per module (in
    /// [`emit_module_funcs`]); each `Emit` snapshots the pointer so
    /// per-op `Div(I64)` / `Mod(I64)` guards can call it without
    /// re-querying the module.
    llvm_trap_fn: Option<FunctionValue<'ctx>>,
    /// Phase E.1: per-module const-data lookup. `Op::ConstString { idx }`
    /// reads the matching offset and pushes `iconst(I32, off)`.
    const_pool: &'cp ConstPool,
    /// Phase E.1: stack of inline call frames. `Op::Call` pushes one
    /// before lowering the callee body; `Op::Return` inside the
    /// callee body pops the typed value into the topmost frame's
    /// result alloca and jumps to its exit block. The callee's
    /// `LocalGet(idx)` resolves to `params[idx]` rather than the
    /// entry's LLVM params; `LetGet/LetSet` indices are remapped
    /// against `let_offset` so concurrent inline frames don't clash.
    inline_frames: Vec<InlineFrame<'ctx>>,
    /// Phase E.1: did the body emit a pointer-indirect StoreField?
    /// When set, the buffer-protocol epilogue returns the post-bump
    /// tail cursor (in bytes past `out_ptr`) rather than the
    /// statically-known `buffer_return_size`. Mirrors cranelift's
    /// `needs_tail_cursor` flag.
    needs_tail_cursor: bool,
    /// Phase H: bytes literal pushed by the *immediately preceding*
    /// `Op::ConstString` op (i.e. still the top-of-stack at the start
    /// of the next `lower_op` call). Cleared at the start of every
    /// `lower_op` and re-populated by the `Op::ConstString` arm at
    /// its tail. The `Op::Call` arm reads this when `fn_index ==
    /// STDLIB_IDX_CONTAINS` to detect the const-needle case and
    /// inline a tight byte-scan loop, skipping the
    /// `relon_llvm_str_contains_arena` extern shim's FFI boundary
    /// (~10-15 cycles of prologue/epilogue per call on x86_64). On
    /// the W4 / W4_long hot loops the needle is always a
    /// compile-time const (`"x"`), so the const-needle fast path
    /// fires 100% of iters. Stays `None` when the needle came in via
    /// `LocalGet` / `LetGet` / any non-`ConstString` producer — those
    /// fall through to the existing extern path.
    last_const_string: Option<Vec<u8>>,
}

/// Phase E.1: per-call inline-frame state. One entry per active
/// stdlib `Op::Call`; the callee body lowers against the topmost
/// frame.
struct InlineFrame<'ctx> {
    /// LLVM values bound to the callee's `LocalGet(0..arity)` reads.
    /// Order matches the IR's declared parameter order — the
    /// `Op::Call` site popped them from the caller's operand stack
    /// (top-of-stack = last param) and reversed.
    params: Vec<TypedValue<'ctx>>,
    /// Offset added to the callee's `LetGet/LetSet` indices so its
    /// let-bindings don't alias the caller's slots. Mirrors the
    /// cranelift backend's `let_offset`.
    let_offset: u32,
    /// Result alloca + exit basic block. The callee's `Op::Return`
    /// stores the popped value into the alloca and unconditionally
    /// branches to `exit_bb`; the caller continues from there with a
    /// matching load.
    ret_slot: PointerValue<'ctx>,
    /// LLVM type stored at [`Self::ret_slot`]. Pre-computed from the
    /// IR-declared `ret_ty` of the stdlib call so the caller-side
    /// load knows what width to read.
    ret_ty: IrType,
    /// Branch target for `Op::Return` inside the callee body. The
    /// caller positions the builder here after the inline finishes
    /// and pushes the loaded return value back onto the operand
    /// stack.
    exit_bb: inkwell::basic_block::BasicBlock<'ctx>,
}

/// Phase D.1 fast-path emission state. Carried inside [`Emit`] when
/// lowering the typed fast entry.
#[derive(Clone)]
struct FastEmit<'ctx> {
    profile: FastPathProfile,
    /// Alloca holding the i64 return value. Trailing `StoreField`
    /// at `profile.ret_offset` writes into this slot; `Op::Return`
    /// loads from it.
    ret_slot: PointerValue<'ctx>,
}

#[derive(Clone, Copy)]
struct TypedValue<'ctx> {
    val: IntValue<'ctx>,
    /// IR-level tag of `val`. Recorded so Phase C predicates that
    /// inspect operand types (signed-vs-unsigned cmp, F64 routing)
    /// have it on hand without re-deriving from LLVM bit width.
    /// Phase B never consumes this field; `#[allow(dead_code)]`
    /// keeps the lint clean while we're still wiring future Op
    /// support.
    #[allow(dead_code)]
    ty: IrType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LabelKind {
    /// `Br` jumps **past** the block (forward exit).
    Block,
    /// `Br` jumps **back** to the loop header (continue).
    Loop,
}

#[derive(Clone)]
struct LabelFrame<'ctx> {
    /// Header basic block. For Block this is unused for branching
    /// (we never branch backward to the start of a block); for Loop
    /// it's the target of a `Br` (continue).
    header_bb: inkwell::basic_block::BasicBlock<'ctx>,
    /// Tail basic block — what code after the block / after the
    /// loop falls through to. For Block this is the `Br` target;
    /// for Loop the surrounding code lives here.
    tail_bb: inkwell::basic_block::BasicBlock<'ctx>,
    kind: LabelKind,
}

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: &'ctx Context,
        builder: &'b Builder<'ctx>,
        module: &'b LlvmModule<'ctx>,
        func: FunctionValue<'ctx>,
        shape: EntryShape,
        arena_base_ptr: Option<PointerValue<'ctx>>,
        state_ptr: Option<PointerValue<'ctx>>,
        buffer_return_size: u32,
        const_pool: &'cp ConstPool,
    ) -> Self {
        Self {
            ctx,
            builder,
            func,
            module,
            shape,
            arena_base_ptr,
            state_ptr,
            stack: Vec::with_capacity(8),
            let_slots: std::collections::HashMap::new(),
            param_base: 0,
            label_stack: Vec::new(),
            name_seq: 0,
            buffer_return_size,
            fast_path: None,
            helper_table: None,
            helper_ret_ty: None,
            llvm_trap_fn: None,
            const_pool,
            inline_frames: Vec::new(),
            needs_tail_cursor: false,
            last_const_string: None,
        }
    }

    fn next_name(&mut self, hint: &str) -> String {
        self.name_seq += 1;
        format!("{hint}_{}", self.name_seq)
    }

    // -- stack helpers --------------------------------------------------

    fn push(&mut self, v: IntValue<'ctx>, ty: IrType) {
        self.stack.push(TypedValue { val: v, ty });
    }

    fn pop(&mut self, ip_hint: &str) -> Result<TypedValue<'ctx>, LlvmError> {
        self.stack.pop().ok_or_else(|| {
            LlvmError::Codegen(format!(
                "operand stack underflow at {ip_hint}: producer emitted an Op with no matching push"
            ))
        })
    }

    fn pop_int(&mut self, ip_hint: &str) -> Result<IntValue<'ctx>, LlvmError> {
        self.pop(ip_hint).map(|tv| tv.val)
    }

    // -- locals / lets --------------------------------------------------

    fn lookup_param(&self, idx: u32) -> Result<IntValue<'ctx>, LlvmError> {
        let llvm_idx = self
            .param_base
            .checked_add(idx)
            .ok_or_else(|| LlvmError::Codegen(format!("LocalGet({idx}): param idx overflow")))?;
        let p = self.func.get_nth_param(llvm_idx).ok_or_else(|| {
            LlvmError::Codegen(format!(
                "LocalGet({idx}) -> llvm param #{llvm_idx} out of range; function has {} param(s)",
                self.func.count_params()
            ))
        })?;
        match p {
            BasicValueEnum::IntValue(v) => Ok(v),
            other => Err(LlvmError::Codegen(format!(
                "LocalGet({idx}) llvm param #{llvm_idx} is {other:?}, expected IntValue"
            ))),
        }
    }

    fn ensure_let_slot(&mut self, idx: u32, ty: IrType) -> Result<PointerValue<'ctx>, LlvmError> {
        if let Some((ptr, existing_ty)) = self.let_slots.get(&idx) {
            if *existing_ty != ty {
                return Err(LlvmError::Codegen(format!(
                    "let-slot {idx} aliased: previous type {existing_ty:?}, new type {ty:?}"
                )));
            }
            return Ok(*ptr);
        }
        // Allocate in the function's entry block so the alloca is
        // hoisted out of any loop body. inkwell's `build_alloca`
        // emits at the current position, so we temporarily reposition.
        let entry_bb = self.func.get_first_basic_block().ok_or_else(|| {
            LlvmError::Codegen("ensure_let_slot: function has no entry block".into())
        })?;
        let cur = self.builder.get_insert_block();
        // Position at the start of the entry block so allocas group
        // at the top — LLVM mem2reg requires this canonical layout
        // to promote slots into SSA.
        if let Some(first_instr) = entry_bb.get_first_instruction() {
            self.builder.position_before(&first_instr);
        } else {
            self.builder.position_at_end(entry_bb);
        }
        let llvm_ty: inkwell::types::BasicTypeEnum<'ctx> = match ty {
            IrType::I64 => self.ctx.i64_type().into(),
            // Phase E.1: String / List* arena offsets ride on an i32
            // slot — matches the cranelift backend's pointer-as-i32
            // wire representation.
            IrType::I32
            | IrType::Bool
            | IrType::Null
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool => self.ctx.i32_type().into(),
            other => {
                return Err(LlvmError::Codegen(format!(
                    "let-slot {idx}: unsupported type {other:?}"
                )));
            }
        };
        let name = format!("let_{idx}");
        let ptr = self
            .builder
            .build_alloca(llvm_ty, &name)
            .map_err(|e| LlvmError::Codegen(format!("let-slot {idx} alloca: {e}")))?;
        if let Some(bb) = cur {
            self.builder.position_at_end(bb);
        }
        self.let_slots.insert(idx, (ptr, ty));
        Ok(ptr)
    }

    // -- entry point ----------------------------------------------------

    fn lower_body(&mut self, body: &[TaggedOp]) -> Result<(), LlvmError> {
        for (ip, tagged) in body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        // After `Op::Return` we positioned at a fresh "after_return_cont"
        // block which is dead and unterminated. Seal it with
        // `unreachable` so LLVM's verifier accepts the module. Same
        // pattern applies to the post-`Br` continuation block.
        if let Some(cur) = self.builder.get_insert_block() {
            if cur.get_terminator().is_none() {
                self.builder
                    .build_unreachable()
                    .map_err(|e| LlvmError::Codegen(format!("trailing unreachable: {e}")))?;
            }
        }
        Ok(())
    }

    // -- per-op lowering ------------------------------------------------

    fn lower_op(&mut self, ip: usize, tagged: &TaggedOp) -> Result<(), LlvmError> {
        let ip_hint = format!("ip={ip} op={:?}", tagged.op);
        // Phase H const-needle fast path: capture (and clear) the
        // `Op::ConstString` peek-state at the very start of every
        // `lower_op` dispatch. The `Op::Call` arm consults `prev_const_string`
        // to decide between the inline byte-scan and the extern shim.
        // Every other arm leaves `self.last_const_string` at `None` —
        // the only re-populator is the `Op::ConstString` arm at its
        // tail. Result: `prev_const_string.is_some()` iff the prior
        // emitted op was `Op::ConstString` and its value is still the
        // top-of-stack (no intervening op consumed it).
        let prev_const_string = self.last_const_string.take();
        match &tagged.op {
            // ---- literals ----
            Op::ConstI64(v) => {
                let c = self.ctx.i64_type().const_int(*v as u64, true);
                self.push(c, IrType::I64);
            }
            Op::ConstI32(v) => {
                let c = self.ctx.i32_type().const_int(*v as u32 as u64, false);
                self.push(c, IrType::I32);
            }
            Op::ConstBool(b) => {
                // Bool occupies an i32 slot on the IR's virtual stack.
                let c = self.ctx.i32_type().const_int(u64::from(*b), false);
                self.push(c, IrType::Bool);
            }

            // ---- locals / lets ----
            Op::LocalGet(idx) => {
                // Phase E.1: an active inline frame redirects
                // `LocalGet(i)` to the inlined call's `i`-th argument
                // instead of the entry-function's LLVM params.
                if let Some(frame) = self.inline_frames.last() {
                    let i = *idx as usize;
                    let tv = frame.params.get(i).ok_or_else(|| {
                        LlvmError::Codegen(format!(
                            "inline LocalGet({idx}) out of range — callee has {} params",
                            frame.params.len()
                        ))
                    })?;
                    self.push(tv.val, tv.ty);
                } else {
                    let p = self.lookup_param(*idx)?;
                    // The legacy envelope walks all-i64; the buffer envelope
                    // walks (i32 ×4, i64). The IR has the right type on
                    // the param descriptor, but we don't carry it through
                    // LocalGet — re-derive from the LLVM param width.
                    let width = p.get_type().get_bit_width();
                    let ty = if width == 32 {
                        IrType::I32
                    } else {
                        IrType::I64
                    };
                    self.push(p, ty);
                }
            }
            Op::LetSet { idx, ty } => {
                let v = self.pop(&ip_hint)?;
                let mapped = self.remap_let_idx(*idx);
                let slot = self.ensure_let_slot(mapped, *ty)?;
                // Coerce on bool / null where the producer pushed an i32
                // slot but the let-slot was declared as the canonical
                // 32-bit width.
                let stored = self.coerce_to_let_ty(v, *ty)?;
                self.builder
                    .build_store(slot, stored)
                    .map_err(|e| LlvmError::Codegen(format!("LetSet store: {e}")))?;
            }
            Op::LetGet { idx, ty } => {
                // Phase E.1: remap the callee's let-idx against the
                // active inline frame so concurrent stdlib inlines
                // don't clash on slot numbers.
                let mapped = self.remap_let_idx(*idx);
                let slot = self.ensure_let_slot(mapped, *ty)?;
                let llvm_ty: inkwell::types::BasicTypeEnum<'ctx> = match *ty {
                    IrType::I64 => self.ctx.i64_type().into(),
                    IrType::I32
                    | IrType::Bool
                    | IrType::Null
                    | IrType::String
                    | IrType::ListInt
                    | IrType::ListFloat
                    | IrType::ListBool => self.ctx.i32_type().into(),
                    other => {
                        return Err(LlvmError::Codegen(format!(
                            "LetGet({idx}): unsupported type {other:?}"
                        )));
                    }
                };
                let name = self.next_name("letget");
                let v = self
                    .builder
                    .build_load(llvm_ty, slot, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LetGet load: {e}")))?
                    .into_int_value();
                self.push(v, *ty);
            }

            // ---- arithmetic ----
            Op::Add(ty) => match ty {
                // Phase E.1: `Op::Add(IrType::String)` is the
                // pair-wise String + String form (the StrConcatN
                // fold only fires for compile-time-known chains —
                // `reduce("", (acc, s) => acc + s)` lowers to a
                // per-iter `Add(String)`). Route through the bundled
                // stdlib `concat` body via inline call, matching the
                // cranelift backend.
                IrType::String => {
                    let concat_idx =
                        relon_ir::stdlib::stdlib_function_index("concat").ok_or_else(|| {
                            LlvmError::Codegen("stdlib `concat` slot not found".to_string())
                        })?;
                    let param_tys = [IrType::String, IrType::String];
                    self.emit_call_stdlib(&ip_hint, concat_idx, 2, &param_tys, IrType::String)?
                }
                _ => self.emit_binop(&ip_hint, *ty, BinOp::Add)?,
            },
            Op::Sub(ty) => self.emit_binop(&ip_hint, *ty, BinOp::Sub)?,
            Op::Mul(ty) => self.emit_binop(&ip_hint, *ty, BinOp::Mul)?,
            Op::Div(ty) => self.emit_binop(&ip_hint, *ty, BinOp::Div)?,
            Op::Mod(ty) => self.emit_binop(&ip_hint, *ty, BinOp::Mod)?,
            Op::BitAnd(ty) => self.emit_binop(&ip_hint, *ty, BinOp::BitAnd)?,

            // ---- comparisons ----
            Op::Eq(ty) => self.emit_cmp(&ip_hint, *ty, IntPredicate::EQ)?,
            Op::Ne(ty) => self.emit_cmp(&ip_hint, *ty, IntPredicate::NE)?,
            Op::Lt(ty) => self.emit_cmp(&ip_hint, *ty, IntPredicate::SLT)?,
            Op::Le(ty) => self.emit_cmp(&ip_hint, *ty, IntPredicate::SLE)?,
            Op::Gt(ty) => self.emit_cmp(&ip_hint, *ty, IntPredicate::SGT)?,
            Op::Ge(ty) => self.emit_cmp(&ip_hint, *ty, IntPredicate::SGE)?,

            // ---- buffer-protocol I/O ----
            Op::LoadField { offset, ty } => self.emit_load_field(*offset, *ty)?,
            Op::StoreField { offset, ty } => self.emit_store_field(&ip_hint, *offset, *ty)?,

            // ---- control flow ----
            Op::Block { result_ty, body } => self.emit_block(*result_ty, body)?,
            Op::Loop { result_ty, body } => self.emit_loop(*result_ty, body)?,
            Op::Br { label_depth } => self.emit_br(*label_depth)?,
            Op::BrIf { label_depth } => self.emit_br_if(&ip_hint, *label_depth)?,
            Op::If {
                result_ty,
                then_body,
                else_body,
            } => self.emit_if(&ip_hint, *result_ty, then_body, else_body)?,

            // ---- return ----
            Op::Return => self.emit_return(&ip_hint)?,

            // ---- Phase E.1: const-data pool ----
            Op::ConstString { idx, value } => {
                let off = self
                    .const_pool
                    .string_offsets
                    .get(idx)
                    .copied()
                    .ok_or_else(|| {
                        LlvmError::Codegen(format!(
                            "Op::ConstString {{ idx: {idx} }}: missing const-pool entry — \
                         did the host forget to lay out the pool blob before dispatch?"
                        ))
                    })?;
                let c = self.ctx.i32_type().const_int(u64::from(off), false);
                self.push(c, IrType::String);
                // Phase H peek-state: record the literal bytes so the
                // next `lower_op` call can detect `Op::Call(contains)`
                // with this string still at top-of-stack and switch
                // to the inline byte-scan instead of the extern shim.
                // Cleared at the start of every `lower_op` — see the
                // `prev_const_string.take()` line at the dispatch
                // head — so a single intervening op (Push / Pop /
                // Add / ...) drops the optimisation cleanly.
                self.last_const_string = Some(value.as_bytes().to_vec());
            }

            // ---- Phase E.1: raw-memory primitives ----
            Op::LoadI32AtAbsolute { offset } => {
                self.emit_load_at_absolute(&ip_hint, *offset, AbsLoad::I32)?
            }
            Op::LoadI64AtAbsolute { offset } => {
                self.emit_load_at_absolute(&ip_hint, *offset, AbsLoad::I64)?
            }
            Op::LoadI8UAtAbsolute { offset } => {
                self.emit_load_at_absolute(&ip_hint, *offset, AbsLoad::I8U)?
            }
            Op::LoadF64AtAbsolute { offset } => {
                self.emit_load_at_absolute(&ip_hint, *offset, AbsLoad::F64)?
            }
            Op::StoreI32AtAbsolute { offset } => {
                self.emit_store_at_absolute(&ip_hint, *offset, AbsStore::I32)?
            }
            Op::StoreI64AtAbsolute { offset } => {
                self.emit_store_at_absolute(&ip_hint, *offset, AbsStore::I64)?
            }
            Op::StoreI8AtAbsolute { offset } => {
                self.emit_store_at_absolute(&ip_hint, *offset, AbsStore::I8)?
            }
            Op::StoreF64AtAbsolute { offset } => {
                self.emit_store_at_absolute(&ip_hint, *offset, AbsStore::F64)?
            }
            Op::MemcpyAtAbsolute => self.emit_memcpy_at_absolute(&ip_hint)?,
            Op::AllocScratch { size_bytes } => self.emit_alloc_scratch_static(*size_bytes)?,
            Op::AllocScratchDyn => self.emit_alloc_scratch_dyn(&ip_hint)?,
            Op::StrConcatN { operand_count } => self.emit_str_concat_n(&ip_hint, *operand_count)?,

            // ---- Phase E.1 + E.2 call dispatch ----
            // stdlib indices (#278) route through the bundled-body
            // inline path (`emit_call_stdlib`); user-defined indices
            // (#279) resolve through the helper table populated by
            // `emit_module_funcs`.
            Op::Call {
                fn_index,
                arg_count,
                param_tys,
                ret_ty,
            } => {
                let stdlib_count = relon_ir::stdlib::stdlib_function_count();
                // Phase F.1: `contains(haystack, needle) -> Bool` short-
                // circuit. The bundled stdlib body is a hand-transcribed
                // O(s_len * p_len) byte scan that defeats LLVM's auto-
                // vectoriser on the inner compare loop (every iter
                // reloads the needle bytes through a let-slot). On the
                // W4 / W4_long cmp_lua rows that turns into a 3.4× /
                // 256× gap vs LuaJIT (which uses SIMD-accelerated
                // `string.find`). Route the call through the host shim
                // `relon_llvm_str_contains_arena` which defers to
                // `core::str::contains` — std's substring search backs
                // single-byte needles with SIMD `memchr` and uses a
                // Two-Way matcher for longer needles, closing the gap
                // without inventing a Relon-specific SIMD path.
                if *fn_index < stdlib_count
                    && relon_ir::stdlib::stdlib_function_index("contains") == Some(*fn_index)
                    && *arg_count == 2
                    && param_tys == &[IrType::String, IrType::String]
                    && *ret_ty == IrType::Bool
                {
                    // Phase H: when the needle was pushed by the
                    // immediately-preceding `Op::ConstString` (peek
                    // state populated at `lower_op` head), inline a
                    // tight byte-scan against the literal bytes.
                    // Skips the `relon_llvm_str_contains_arena` FFI
                    // boundary entirely — ~10-15 cycles of prologue /
                    // epilogue / IC atomic loads per call. The W4 /
                    // W4_long hot loops always hit this path (needle
                    // = `"x"` literal); dynamic-needle callers (e.g.
                    // `filter((s) => s.contains(other))` where
                    // `other` flows in via an outer let-slot) fall
                    // through to the existing Phase G extern shim.
                    if let Some(needle_bytes) = prev_const_string.as_deref() {
                        self.emit_str_contains_const_needle(&ip_hint, needle_bytes)?;
                    } else {
                        self.emit_str_contains_extern(&ip_hint)?;
                    }
                } else if *fn_index < stdlib_count {
                    self.emit_call_stdlib(&ip_hint, *fn_index, *arg_count, param_tys, *ret_ty)?
                } else {
                    self.emit_call(&ip_hint, *fn_index, *arg_count, param_tys, *ret_ty)?
                }
            }

            other => {
                return Err(LlvmError::Codegen(format!(
                    "unsupported op (Phase E.1 envelope): {other:?} at ip={ip}"
                )));
            }
        }
        Ok(())
    }

    // -- Phase E.1: inline-call frame helpers --------------------------

    /// Translate a callee `LetGet/LetSet` index against the topmost
    /// inline frame. Mirrors cranelift's `remap_let_idx`.
    fn remap_let_idx(&self, idx: u32) -> u32 {
        match self.inline_frames.last() {
            Some(frame) => frame.let_offset.saturating_add(idx),
            None => idx,
        }
    }

    /// Lower `Op::Return`. The shape decides what flows back:
    ///
    /// - Legacy-i64: pop the top of the operand stack and `ret v`.
    /// - Buffer-protocol: return a hard-coded i32 `return_root_size`
    ///   so the host trampoline reads back the full fixed area.
    ///   Phase B doesn't emit pointer-indirect StoreField, so the
    ///   tail-cursor path is dead — `return_root_size` is enough.
    ///
    /// Mirrors the cranelift backend's `emit_return` for the same
    /// shapes.
    fn emit_return(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        // Phase E.1: inline-frame return. The callee body pops the
        // typed return value, stores it into the frame's ret_slot,
        // then unconditionally jumps to exit_bb. The caller side picks
        // up from there in `emit_call_stdlib`.
        if let Some((ret_slot, exit_bb, ret_ty)) = self
            .inline_frames
            .last()
            .map(|f| (f.ret_slot, f.exit_bb, f.ret_ty))
        {
            let v = self.pop(ip_hint)?;
            // Coerce the popped value's width to the slot type if
            // needed (Bool / Null on an i32 stack but stored as i32
            // already — no coercion. String / ListInt on i32 — same.
            // I64 on i64 — same. We rely on the caller's typing
            // contract.)
            let stored = self.coerce_to_let_ty(v, ret_ty)?;
            self.builder
                .build_store(ret_slot, stored)
                .map_err(|e| LlvmError::Codegen(format!("inline Return store: {e}")))?;
            self.builder
                .build_unconditional_branch(exit_bb)
                .map_err(|e| LlvmError::Codegen(format!("inline Return br: {e}")))?;
            // Open a fresh dummy block so any subsequent ops the body
            // emits (e.g. dead trailing ConstBool after Trap) have
            // somewhere to land. LLVM's verifier prunes the dead chain.
            let dummy = self.ctx.append_basic_block(self.func, "after_inline_ret");
            self.builder.position_at_end(dummy);
            return Ok(());
        }
        // Phase D.1 fast path: the trailing buffer-protocol `Op::Return`
        // doesn't carry a value on the stack (the IR producer already
        // emitted a `StoreField` into the output buffer that the fast
        // emitter redirected into `ret_slot`). Load + `ret` from the
        // slot to produce the typed i64 result.
        if let Some(fast) = self.fast_path.as_ref() {
            let i64_t = self.ctx.i64_type();
            let v = self
                .builder
                .build_load(i64_t, fast.ret_slot, "fast_ret_load")
                .map_err(|e| LlvmError::Codegen(format!("fast Return load: {e}")))?
                .into_int_value();
            self.builder
                .build_return(Some(&v))
                .map_err(|e| LlvmError::Codegen(format!("fast Return: {e}")))?;
            // Open a dead continuation block so downstream ops have
            // somewhere to land — matches the buffer/legacy branches
            // below. The block stays dead; the verifier accepts it
            // once we seal with `unreachable` in `lower_body`'s
            // trailing branch.
            let cont = self.ctx.append_basic_block(self.func, "after_return_cont");
            self.builder.position_at_end(cont);
            // Suppress the `_` warning on ip_hint when this branch
            // runs.
            let _ = ip_hint;
            return Ok(());
        }
        // Phase E.2 helper-body return: when lowering a sibling
        // function rather than the entry, pop the operand and emit a
        // typed return matching the helper's declared IR return type.
        // Widens / truncates the popped i32 / i64 to the declared LLVM
        // ret slot when the two widths disagree.
        if let Some(ret_ty) = self.helper_ret_ty {
            let v = self.pop_int(ip_hint)?;
            let want_width = match ret_ty {
                IrType::I64 => 64,
                IrType::I32
                | IrType::Bool
                | IrType::Null
                | IrType::String
                | IrType::ListInt
                | IrType::ListFloat
                | IrType::ListBool
                | IrType::ListString
                | IrType::ListSchema
                | IrType::Closure => 32,
                IrType::F64 => {
                    return Err(LlvmError::Codegen(
                        "helper Return: F64 not yet supported in Phase E.2".into(),
                    ));
                }
            };
            let have_width = v.get_type().get_bit_width();
            let final_v = if have_width == want_width {
                v
            } else if have_width < want_width {
                let target_ty = if want_width == 64 {
                    self.ctx.i64_type()
                } else {
                    self.ctx.i32_type()
                };
                self.builder
                    .build_int_z_extend(v, target_ty, "helper_ret_zext")
                    .map_err(|e| LlvmError::Codegen(format!("helper Return zext: {e}")))?
            } else {
                let target_ty = if want_width == 64 {
                    self.ctx.i64_type()
                } else {
                    self.ctx.i32_type()
                };
                self.builder
                    .build_int_truncate(v, target_ty, "helper_ret_trunc")
                    .map_err(|e| LlvmError::Codegen(format!("helper Return trunc: {e}")))?
            };
            self.builder
                .build_return(Some(&final_v))
                .map_err(|e| LlvmError::Codegen(format!("helper Return: {e}")))?;
            let cont = self.ctx.append_basic_block(self.func, "after_return_cont");
            self.builder.position_at_end(cont);
            return Ok(());
        }
        match self.shape {
            EntryShape::LegacyI64 => {
                let v = self.pop_int(ip_hint)?;
                self.builder
                    .build_return(Some(&v))
                    .map_err(|e| LlvmError::Codegen(format!("Return (legacy): {e}")))?;
            }
            EntryShape::Buffer => {
                let i32_t = self.ctx.i32_type();
                // Phase E.1: when the body emitted a pointer-indirect
                // StoreField (String / List* return) the trampoline
                // needs to know how many bytes past `out_ptr` the tail
                // cursor advanced to. Read it back from the state slot
                // so the host can decode the variable-length payload.
                // Bodies that only wrote into the fixed area keep the
                // historical "return root_size" path so a trampoline
                // that doesn't bother to consult `tail_cursor` still
                // works.
                let v: IntValue<'ctx> = if self.needs_tail_cursor {
                    let state_ptr = self.state_ptr.ok_or_else(|| {
                        LlvmError::Codegen(
                            "buffer Return needs tail_cursor but state ptr unavailable".into(),
                        )
                    })?;
                    let i8_t = self.ctx.i8_type();
                    let tail_gep = unsafe {
                        self.builder
                            .build_in_bounds_gep(
                                i8_t,
                                state_ptr,
                                &[i32_t
                                    .const_int(u64::from(ARENA_STATE_OFFSET_TAIL_CURSOR), false)],
                                "tail_cursor_gep",
                            )
                            .map_err(|e| LlvmError::Codegen(format!("tail_cursor GEP: {e}")))?
                    };
                    self.builder
                        .build_load(i32_t, tail_gep, "tail_cursor")
                        .map_err(|e| LlvmError::Codegen(format!("tail_cursor load: {e}")))?
                        .into_int_value()
                } else {
                    i32_t.const_int(u64::from(self.buffer_return_size), false)
                };
                self.builder
                    .build_return(Some(&v))
                    .map_err(|e| LlvmError::Codegen(format!("Return (buffer): {e}")))?;
            }
        }
        // After the explicit return, the rest of the surrounding
        // body is unreachable. Open a fresh continuation block so
        // any subsequent ops (a stray `LetGet` after a Br-tail
        // Return, etc.) emit somewhere valid. The block is dead;
        // LLVM's verifier accepts it as long as it ends with a
        // terminator — we seal it with `unreachable` lazily when
        // the next terminator-emitting op needs to bind it.
        let cont = self.ctx.append_basic_block(self.func, "after_return_cont");
        self.builder.position_at_end(cont);
        Ok(())
    }

    /// Phase E.2 multi-function dispatch: lower `Op::Call`.
    ///
    /// The IR's `fn_index` is split as `[0..stdlib_count) = bundled
    /// stdlib body` / `[stdlib_count..) = user-defined sibling`. The
    /// LLVM emitter currently only routes the sibling slice — stdlib
    /// inlining stays parked on the cranelift backend. A stdlib call
    /// surfaces `LlvmError::Codegen` so the host can fall back.
    fn emit_call(
        &mut self,
        ip_hint: &str,
        fn_index: u32,
        arg_count: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        let stdlib_count = relon_ir::stdlib::stdlib_function_count();
        if fn_index < stdlib_count {
            return Err(LlvmError::Codegen(format!(
                "Op::Call to stdlib fn_index={fn_index} not yet supported in LLVM AOT \
                 (cranelift inlines bundled stdlib bodies; LLVM path widens with #278)"
            )));
        }
        let helper_idx = fn_index - stdlib_count;
        let callee = match self.helper_table.as_ref().and_then(|t| t.get(&helper_idx)) {
            Some(fv) => *fv,
            None => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call helper_idx={helper_idx} (fn_index={fn_index}, stdlib_count={stdlib_count}) \
                     not in helper_table — module may be missing the function"
                )));
            }
        };

        // Sanity check arity against the declared signature.
        if callee.count_params() as usize != param_tys.len() {
            return Err(LlvmError::Codegen(format!(
                "Op::Call helper_idx={helper_idx}: callee has {} LLVM params, IR declares {}",
                callee.count_params(),
                param_tys.len()
            )));
        }
        if arg_count as usize != param_tys.len() {
            return Err(LlvmError::Codegen(format!(
                "Op::Call helper_idx={helper_idx}: arg_count={arg_count} != param_tys.len()={}",
                param_tys.len()
            )));
        }

        // Pop the arguments off the operand stack — last-pushed value
        // is the last param.
        let mut args: Vec<BasicMetadataValueEnum<'ctx>> = Vec::with_capacity(arg_count as usize);
        for _ in 0..arg_count {
            args.push(self.pop_int(ip_hint)?.into());
        }
        args.reverse();

        // Adjust each arg's LLVM type to match the callee's declared
        // param: widen / truncate i32 <-> i64 as needed. The IR's
        // stack-machine semantics keep types tagged but the wasm slot
        // widening can leave a Bool-as-i32 in front of an I64 callee
        // param. We re-coerce here to match the helper's signature.
        for (i, (slot, want_ty)) in args.iter_mut().zip(param_tys.iter()).enumerate() {
            let arg_val = match slot {
                BasicMetadataValueEnum::IntValue(v) => *v,
                other => {
                    return Err(LlvmError::Codegen(format!(
                        "Op::Call arg #{i}: expected IntValue, got {other:?}"
                    )));
                }
            };
            let want_width = match *want_ty {
                IrType::I64 => 64,
                IrType::I32
                | IrType::Bool
                | IrType::Null
                | IrType::String
                | IrType::ListInt
                | IrType::ListFloat
                | IrType::ListBool
                | IrType::ListString
                | IrType::ListSchema
                | IrType::Closure => 32,
                IrType::F64 => {
                    return Err(LlvmError::Codegen(format!(
                        "Op::Call arg #{i}: F64 param not yet supported in Phase E.2"
                    )));
                }
            };
            let have_width = arg_val.get_type().get_bit_width();
            if have_width != want_width {
                let target_ty = if want_width == 64 {
                    self.ctx.i64_type()
                } else {
                    self.ctx.i32_type()
                };
                let coerced = if have_width < want_width {
                    self.builder
                        .build_int_z_extend(arg_val, target_ty, "call_arg_zext")
                        .map_err(|e| LlvmError::Codegen(format!("call arg zext: {e}")))?
                } else {
                    self.builder
                        .build_int_truncate(arg_val, target_ty, "call_arg_trunc")
                        .map_err(|e| LlvmError::Codegen(format!("call arg trunc: {e}")))?
                };
                *slot = coerced.into();
            }
        }

        let name = self.next_name("call_ret");
        let call_site = self
            .builder
            .build_call(callee, &args, &name)
            .map_err(|e| LlvmError::Codegen(format!("Op::Call build_call: {e}")))?;
        let ret_val = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(v) => v,
            inkwell::values::ValueKind::Instruction(_) => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call helper_idx={helper_idx}: callee returned void; Phase E.2 envelope expects a typed return"
                )));
            }
        };
        let ret_int = match ret_val {
            BasicValueEnum::IntValue(v) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call helper_idx={helper_idx}: callee returned {other:?}, expected IntValue"
                )));
            }
        };
        self.push(ret_int, ret_ty);
        Ok(())
    }

    // -- helpers --------------------------------------------------------

    fn coerce_to_let_ty(
        &self,
        tv: TypedValue<'ctx>,
        target: IrType,
    ) -> Result<BasicValueEnum<'ctx>, LlvmError> {
        let want_width = match target {
            IrType::I64 => 64,
            IrType::I32
            | IrType::Bool
            | IrType::Null
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool => 32,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "let-slot coerce: unsupported target type {other:?}"
                )));
            }
        };
        let have_width = tv.val.get_type().get_bit_width();
        if have_width == want_width {
            return Ok(tv.val.into());
        }
        let target_ty = if want_width == 64 {
            self.ctx.i64_type()
        } else {
            self.ctx.i32_type()
        };
        if have_width < want_width {
            self.builder
                .build_int_z_extend(tv.val, target_ty, "let_zext")
                .map(|v| v.as_basic_value_enum())
                .map_err(|e| LlvmError::Codegen(format!("let zext: {e}")))
        } else {
            self.builder
                .build_int_truncate(tv.val, target_ty, "let_trunc")
                .map(|v| v.as_basic_value_enum())
                .map_err(|e| LlvmError::Codegen(format!("let trunc: {e}")))
        }
    }

    fn emit_binop(&mut self, ip_hint: &str, ty: IrType, op: BinOp) -> Result<(), LlvmError> {
        let b = self.pop_int(ip_hint)?;
        let a = self.pop_int(ip_hint)?;

        // Phase E.2 sandbox parity: guard Div / Mod against a zero RHS
        // so the JIT raises a deterministic trap instead of leaving
        // LLVM's `sdiv` / `srem` to invoke UB (which on x86 surfaces
        // as a host-level SIGFPE that the host can't catch on stable
        // Rust). Emit an `if rhs == 0 { llvm.trap; unreachable } else
        // { ... }` skeleton and continue the division in the `else`
        // arm. The `unreachable` after `llvm.trap` is what tells LLVM
        // the trap path doesn't fall through.
        if matches!(op, BinOp::Div | BinOp::Mod) {
            let zero = b.get_type().const_zero();
            let cmp_name = self.next_name("divz_cmp");
            let is_zero = self
                .builder
                .build_int_compare(IntPredicate::EQ, b, zero, &cmp_name)
                .map_err(|e| LlvmError::Codegen(format!("{} divz cmp: {e}", op.name())))?;
            let trap_bb = self.ctx.append_basic_block(self.func, "div_by_zero_trap");
            let cont_bb = self.ctx.append_basic_block(self.func, "div_by_zero_ok");
            self.builder
                .build_conditional_branch(is_zero, trap_bb, cont_bb)
                .map_err(|e| LlvmError::Codegen(format!("{} divz branch: {e}", op.name())))?;
            // Trap block: call `llvm.trap` then `unreachable`. The
            // intrinsic is declared lazily; subsequent emits reuse the
            // declaration so the module ends up with at most one
            // `@llvm.trap` symbol regardless of how many guards fire.
            self.builder.position_at_end(trap_bb);
            self.emit_llvm_trap_call(op.name())?;
            self.builder
                .build_unreachable()
                .map_err(|e| LlvmError::Codegen(format!("{} divz unreachable: {e}", op.name())))?;
            // Continue normal codegen in the "ok" block.
            self.builder.position_at_end(cont_bb);
        }

        let name = self.next_name(op.name());
        let r = match op {
            BinOp::Add => self.builder.build_int_add(a, b, &name),
            BinOp::Sub => self.builder.build_int_sub(a, b, &name),
            BinOp::Mul => self.builder.build_int_mul(a, b, &name),
            BinOp::Div => self.builder.build_int_signed_div(a, b, &name),
            BinOp::Mod => self.builder.build_int_signed_rem(a, b, &name),
            BinOp::BitAnd => self.builder.build_and(a, b, &name),
        }
        .map_err(|e| LlvmError::Codegen(format!("{} build failed: {e}", op.name())))?;
        self.push(r, ty);
        Ok(())
    }

    /// Phase E.2: emit a call to the `llvm.trap` intrinsic. The
    /// intrinsic must be pre-declared on the module via
    /// [`declare_llvm_trap`] before the first guard fires; the
    /// declaration is cached on the `Emit` so repeated div / mod
    /// guards share one `FunctionValue`. The `op_hint` is used only
    /// for diagnostic naming on the build_call site.
    fn emit_llvm_trap_call(&mut self, op_hint: &str) -> Result<(), LlvmError> {
        let trap_fn = self.llvm_trap_fn.ok_or_else(|| {
            LlvmError::Codegen(format!(
                "{op_hint}: llvm.trap intrinsic missing — emit_module_funcs forgot to declare it"
            ))
        })?;
        let name = self.next_name("trap_call");
        self.builder
            .build_call(trap_fn, &[], &name)
            .map_err(|e| LlvmError::Codegen(format!("{op_hint} llvm.trap build_call: {e}")))?;
        Ok(())
    }

    fn emit_cmp(
        &mut self,
        ip_hint: &str,
        operand_ty: IrType,
        pred: IntPredicate,
    ) -> Result<(), LlvmError> {
        // Pop in the order [b, a] — the deepest operand is the first
        // push (lhs of the comparison).
        let b = self.pop_int(ip_hint)?;
        let a = self.pop_int(ip_hint)?;
        // Phase B keeps every comparison signed (matches what the IR
        // producer emits for `Lt` / `Le` / `Gt` / `Ge`). `Eq` / `Ne`
        // are signedness-agnostic at the LLVM level, so the
        // producer's predicate flows through unchanged.
        let _ = operand_ty;
        let name = self.next_name("cmp");
        let result_i1 = self
            .builder
            .build_int_compare(pred, a, b, &name)
            .map_err(|e| LlvmError::Codegen(format!("Cmp build failed: {e}")))?;
        // The IR's virtual stack wants a `Bool` (i32 slot). Widen the
        // i1 to i32 so the rest of the pipeline (StoreField for Bool
        // returns, BrIf for control flow) sees the canonical width.
        let name_zext = self.next_name("cmp_zext");
        let widened = self
            .builder
            .build_int_z_extend(result_i1, self.ctx.i32_type(), &name_zext)
            .map_err(|e| LlvmError::Codegen(format!("Cmp zext: {e}")))?;
        self.push(widened, IrType::Bool);
        Ok(())
    }

    /// Emit a LoadField — buffer-protocol only. The LLVM IR loads
    /// `arena_base + in_ptr + offset` for a value of `ty`. Phase D.1
    /// fast-path mode short-circuits this into a direct LLVM param
    /// access against the matching arg slot.
    fn emit_load_field(&mut self, offset: u32, ty: IrType) -> Result<(), LlvmError> {
        // Phase D.1 fast path: lift the buffer-protocol field load
        // into a direct LLVM param read whenever the field's offset
        // matches one of the profile's declared arg offsets.
        if let Some(fast) = self.fast_path.as_ref() {
            if ty != IrType::I64 {
                return Err(LlvmError::Codegen(format!(
                    "fast-path LoadField: only I64 args supported, got {ty:?}"
                )));
            }
            let slot = fast
                .profile
                .arg_offsets
                .iter()
                .position(|&o| o == offset)
                .ok_or_else(|| {
                    LlvmError::Codegen(format!(
                        "fast-path LoadField: offset {offset} not in profile.arg_offsets"
                    ))
                })?;
            // LLVM param `slot` is the i64 arg directly under the
            // fast-entry signature (no implicit state slot, no
            // handshake i32 quartet).
            let p = self.func.get_nth_param(slot as u32).ok_or_else(|| {
                LlvmError::Codegen(format!(
                    "fast-path LoadField: llvm param #{slot} missing on function"
                ))
            })?;
            let v = p.into_int_value();
            self.push(v, IrType::I64);
            return Ok(());
        }
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("LoadField outside buffer-protocol entry shape".into())
        })?;
        let in_ptr_i32 = self.lookup_param(0)?; // IR LocalGet(0) == in_ptr
        let addr = self.compute_buffer_addr(arena_base_ptr, in_ptr_i32, offset)?;
        let (llvm_ty, push_ty) = self.field_load_kind(ty)?;
        let name = self.next_name("loadf");
        let raw = self
            .builder
            .build_load(llvm_ty, addr, &name)
            .map_err(|e| LlvmError::Codegen(format!("LoadField load: {e}")))?
            .into_int_value();
        // Widen Bool / Null (i8 on the wire) to i32 to match the IR's
        // virtual-stack convention; I32 / I64 / I8-tagged-as-Null are
        // already the correct width.
        let widened = match push_ty {
            IrType::Bool | IrType::Null => {
                let name = self.next_name("loadf_zext");
                self.builder
                    .build_int_z_extend(raw, self.ctx.i32_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadField zext: {e}")))?
            }
            _ => raw,
        };
        self.push(widened, push_ty);
        Ok(())
    }

    fn emit_store_field(
        &mut self,
        ip_hint: &str,
        offset: u32,
        ty: IrType,
    ) -> Result<(), LlvmError> {
        // Phase E.1: pointer-indirect types (String / List*) route to
        // the tail-cursor protocol — bump-allocate inside the output
        // buffer's tail region, memcpy the record there, and stamp
        // the buffer-relative offset into the fixed-area slot. Comes
        // before the Phase D.1 fast-path check because the fast path
        // explicitly rejects non-I64 stores.
        if matches!(
            ty,
            IrType::String | IrType::ListInt | IrType::ListFloat | IrType::ListBool
        ) {
            return self.emit_store_field_pointer_indirect(ip_hint, offset, ty);
        }
        // Phase D.1 fast path: rewrite trailing StoreField into a
        // store against the i64 ret_slot. Only the single Int return
        // slot is supported — any other offset means the IR is past
        // the fast-path envelope (multi-field record, tail-cursor
        // payload) and we reject.
        if let Some(fast) = self.fast_path.clone() {
            if ty != IrType::I64 {
                return Err(LlvmError::Codegen(format!(
                    "fast-path StoreField: only I64 returns supported, got {ty:?}"
                )));
            }
            if offset != fast.profile.ret_offset {
                return Err(LlvmError::Codegen(format!(
                    "fast-path StoreField: offset {offset} != profile.ret_offset {}",
                    fast.profile.ret_offset
                )));
            }
            let v = self.pop_int(ip_hint)?;
            self.builder
                .build_store(fast.ret_slot, v)
                .map_err(|e| LlvmError::Codegen(format!("fast StoreField ret_slot: {e}")))?;
            return Ok(());
        }
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("StoreField outside buffer-protocol entry shape".into())
        })?;
        let out_ptr_i32 = self.lookup_param(2)?; // IR LocalGet(2) == out_ptr
        let addr = self.compute_buffer_addr(arena_base_ptr, out_ptr_i32, offset)?;
        let v = self.pop_int(ip_hint)?;
        let store_val: BasicValueEnum<'ctx> = match ty {
            IrType::I64 => v.into(),
            IrType::I32 => v.into(),
            IrType::F64 => {
                // The IR's virtual stack carries f64 as bit-cast i64;
                // we don't see ConstF64 / Add(F64) in the Phase B
                // envelope, but a future LoadField -> StoreField pair
                // could leave an i64 on the stack tagged as F64.
                // Treat it as an i64 store; the bit-cast happens at
                // the host side.
                v.into()
            }
            IrType::Bool | IrType::Null => {
                // Narrow the i32 to i8 before storing.
                let name = self.next_name("storef_trunc");
                let narrowed = self
                    .builder
                    .build_int_truncate(v, self.ctx.i8_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("StoreField trunc: {e}")))?;
                narrowed.into()
            }
            other => {
                return Err(LlvmError::Codegen(format!(
                    "StoreField: Phase B envelope rejects {other:?}"
                )));
            }
        };
        self.builder
            .build_store(addr, store_val)
            .map_err(|e| LlvmError::Codegen(format!("StoreField store: {e}")))?;
        Ok(())
    }

    /// Compute `arena_base + buf_ptr + offset` as an LLVM pointer.
    /// The result is a typed-stripped opaque pointer suitable for any
    /// `load` / `store` width.
    fn compute_buffer_addr(
        &mut self,
        arena_base_ptr: PointerValue<'ctx>,
        buf_ptr_i32: IntValue<'ctx>,
        offset: u32,
    ) -> Result<PointerValue<'ctx>, LlvmError> {
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let i8_t = self.ctx.i8_type();
        // Widen `buf_ptr_i32` to i64 (zero-extend — wasm semantics
        // treat the i32 as an unsigned byte offset).
        let name = self.next_name("buf_ptr_zext");
        let buf_ptr64 = self
            .builder
            .build_int_z_extend(buf_ptr_i32, i64_t, &name)
            .map_err(|e| LlvmError::Codegen(format!("buf_ptr zext: {e}")))?;
        let off_const = i32_t.const_int(u64::from(offset), false);
        let off64 = self
            .builder
            .build_int_z_extend(off_const, i64_t, "off_zext")
            .map_err(|e| LlvmError::Codegen(format!("offset zext: {e}")))?;
        let name = self.next_name("buf_off");
        let combined = self
            .builder
            .build_int_add(buf_ptr64, off64, &name)
            .map_err(|e| LlvmError::Codegen(format!("buf_ptr + offset: {e}")))?;
        // GEP from the cached arena_base pointer (which is an i8*)
        // by the combined byte offset.
        let name = self.next_name("field_addr");
        let addr = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, arena_base_ptr, &[combined], &name)
                .map_err(|e| LlvmError::Codegen(format!("field GEP: {e}")))?
        };
        Ok(addr)
    }

    // -- control flow ---------------------------------------------------

    fn emit_block(
        &mut self,
        result_ty: Option<IrType>,
        body: &[TaggedOp],
    ) -> Result<(), LlvmError> {
        if result_ty.is_some() {
            return Err(LlvmError::Codegen(
                "Block with result_ty: Phase B envelope does not carry block-result phis".into(),
            ));
        }
        let header_bb = self.ctx.append_basic_block(self.func, "block_head");
        let tail_bb = self.ctx.append_basic_block(self.func, "block_tail");

        // Fallthrough from the current insertion point into the
        // block's header.
        self.builder
            .build_unconditional_branch(header_bb)
            .map_err(|e| LlvmError::Codegen(format!("Block fallthrough: {e}")))?;
        self.builder.position_at_end(header_bb);

        self.label_stack.push(LabelFrame {
            header_bb,
            tail_bb,
            kind: LabelKind::Block,
        });
        for (ip, tagged) in body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        // If the body ran without an explicit `Br`, fall through to
        // `tail_bb`. A `Br` that fired already terminated the current
        // block via `build_unconditional_branch`; in that case the
        // builder's current block is already terminated and we must
        // not emit another branch.
        let cur_terminated = self
            .builder
            .get_insert_block()
            .and_then(|bb| bb.get_terminator())
            .is_some();
        if !cur_terminated {
            self.builder
                .build_unconditional_branch(tail_bb)
                .map_err(|e| LlvmError::Codegen(format!("Block tail fallthrough: {e}")))?;
        }
        self.builder.position_at_end(tail_bb);
        self.label_stack.pop();
        Ok(())
    }

    fn emit_loop(&mut self, result_ty: Option<IrType>, body: &[TaggedOp]) -> Result<(), LlvmError> {
        if result_ty.is_some() {
            return Err(LlvmError::Codegen(
                "Loop with result_ty: Phase B envelope does not carry loop-result phis".into(),
            ));
        }
        let header_bb = self.ctx.append_basic_block(self.func, "loop_head");
        let tail_bb = self.ctx.append_basic_block(self.func, "loop_tail");

        self.builder
            .build_unconditional_branch(header_bb)
            .map_err(|e| LlvmError::Codegen(format!("Loop fallthrough: {e}")))?;
        self.builder.position_at_end(header_bb);

        self.label_stack.push(LabelFrame {
            header_bb,
            tail_bb,
            kind: LabelKind::Loop,
        });
        for (ip, tagged) in body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        // If the body fell through without an explicit `Br`, that's
        // an implicit "exit the loop" in wasm semantics — the loop
        // body executed once and the loop terminates. Emit a branch
        // to `tail_bb`.
        let cur_terminated = self
            .builder
            .get_insert_block()
            .and_then(|bb| bb.get_terminator())
            .is_some();
        if !cur_terminated {
            self.builder
                .build_unconditional_branch(tail_bb)
                .map_err(|e| LlvmError::Codegen(format!("Loop implicit exit: {e}")))?;
        }
        self.builder.position_at_end(tail_bb);
        self.label_stack.pop();
        Ok(())
    }

    fn label_target(&self, depth: u32) -> Result<&LabelFrame<'ctx>, LlvmError> {
        let len = self.label_stack.len();
        let idx = len
            .checked_sub(1 + depth as usize)
            .ok_or_else(|| LlvmError::Codegen(format!("label_depth {depth} out of range")))?;
        Ok(&self.label_stack[idx])
    }

    fn emit_br(&mut self, label_depth: u32) -> Result<(), LlvmError> {
        let target = self.label_target(label_depth)?;
        let bb = match target.kind {
            LabelKind::Block => target.tail_bb,
            LabelKind::Loop => target.header_bb,
        };
        self.builder
            .build_unconditional_branch(bb)
            .map_err(|e| LlvmError::Codegen(format!("Br: {e}")))?;
        // After a `Br`, the rest of the surrounding body is
        // unreachable in wasm semantics. LLVM does not allow
        // emitting more instructions into a terminated block — we
        // open a fresh `unreachable_after_br` block so the
        // emitter's invariants stay satisfied. The block stays
        // dead; LLVM's verifier and -O2 prune it.
        let dead_bb = self
            .ctx
            .append_basic_block(self.func, "unreachable_after_br");
        self.builder.position_at_end(dead_bb);
        // Seal it with an `unreachable` so the verifier accepts the
        // dead block before -O2 cleans it up.
        self.builder
            .build_unreachable()
            .map_err(|e| LlvmError::Codegen(format!("dead-block unreachable: {e}")))?;
        // Reposition to a fresh successor so subsequent ops have an
        // open block to emit into. The successor will itself become
        // dead, but the verifier is happy with the chain.
        let cont_bb = self.ctx.append_basic_block(self.func, "after_br_cont");
        self.builder.position_at_end(cont_bb);
        Ok(())
    }

    fn emit_br_if(&mut self, ip_hint: &str, label_depth: u32) -> Result<(), LlvmError> {
        let cond = self.pop_int(ip_hint)?;
        // Narrow the i32 / i64 condition to i1.
        let zero = cond.get_type().const_zero();
        let name = self.next_name("br_cond");
        let cond_i1 = self
            .builder
            .build_int_compare(IntPredicate::NE, cond, zero, &name)
            .map_err(|e| LlvmError::Codegen(format!("BrIf cmp: {e}")))?;
        let target = self.label_target(label_depth)?;
        let take_bb = match target.kind {
            LabelKind::Block => target.tail_bb,
            LabelKind::Loop => target.header_bb,
        };
        // Fall-through path stays in the surrounding body.
        let fallthru_bb = self.ctx.append_basic_block(self.func, "br_if_fallthru");
        self.builder
            .build_conditional_branch(cond_i1, take_bb, fallthru_bb)
            .map_err(|e| LlvmError::Codegen(format!("BrIf: {e}")))?;
        self.builder.position_at_end(fallthru_bb);
        Ok(())
    }

    fn emit_if(
        &mut self,
        ip_hint: &str,
        result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
    ) -> Result<(), LlvmError> {
        let cond = self.pop_int(ip_hint)?;
        let name = self.next_name("if_cond");
        let cond_i1 = self
            .builder
            .build_int_compare(IntPredicate::NE, cond, cond.get_type().const_zero(), &name)
            .map_err(|e| LlvmError::Codegen(format!("If cmp: {e}")))?;
        let then_bb = self.ctx.append_basic_block(self.func, "if_then");
        let else_bb = self.ctx.append_basic_block(self.func, "if_else");
        let merge_bb = self.ctx.append_basic_block(self.func, "if_merge");
        self.builder
            .build_conditional_branch(cond_i1, then_bb, else_bb)
            .map_err(|e| LlvmError::Codegen(format!("If branch: {e}")))?;

        // Then arm.
        self.builder.position_at_end(then_bb);
        for (ip, tagged) in then_body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        let then_result = self.pop(ip_hint).ok();
        let then_end_bb = self.builder.get_insert_block().unwrap();
        let then_terminated = then_end_bb.get_terminator().is_some();
        if !then_terminated {
            self.builder
                .build_unconditional_branch(merge_bb)
                .map_err(|e| LlvmError::Codegen(format!("If then->merge: {e}")))?;
        }

        // Else arm.
        self.builder.position_at_end(else_bb);
        for (ip, tagged) in else_body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        let else_result = self.pop(ip_hint).ok();
        let else_end_bb = self.builder.get_insert_block().unwrap();
        let else_terminated = else_end_bb.get_terminator().is_some();
        if !else_terminated {
            self.builder
                .build_unconditional_branch(merge_bb)
                .map_err(|e| LlvmError::Codegen(format!("If else->merge: {e}")))?;
        }

        // Merge phi if both arms terminated normally.
        self.builder.position_at_end(merge_bb);
        match (then_result, else_result) {
            (Some(t), Some(e)) => {
                let phi_ty: inkwell::types::BasicTypeEnum<'ctx> = match result_ty {
                    IrType::I64 => self.ctx.i64_type().into(),
                    IrType::I32 | IrType::Bool | IrType::Null => self.ctx.i32_type().into(),
                    other => {
                        return Err(LlvmError::Codegen(format!(
                            "If result_ty {other:?} unsupported"
                        )));
                    }
                };
                let phi = self
                    .builder
                    .build_phi(phi_ty, "if_phi")
                    .map_err(|e| LlvmError::Codegen(format!("If phi: {e}")))?;
                let then_val: BasicValueEnum<'ctx> = t.val.into();
                let else_val: BasicValueEnum<'ctx> = e.val.into();
                if !then_terminated {
                    phi.add_incoming(&[(&then_val, then_end_bb)]);
                }
                if !else_terminated {
                    phi.add_incoming(&[(&else_val, else_end_bb)]);
                }
                let v = phi.as_basic_value().into_int_value();
                self.push(v, result_ty);
            }
            _ => {
                // One arm didn't push (e.g. ended with Return).
                // Phase B's W1/W2 path doesn't exercise this — surface
                // an error so a future shape doesn't silently miscompile.
                if !then_terminated || !else_terminated {
                    return Err(LlvmError::Codegen(
                        "If arms produced no value but did not terminate".into(),
                    ));
                }
                // Both arms terminated (e.g. both Return). Surface
                // `merge_bb` as unreachable.
                self.builder
                    .build_unreachable()
                    .map_err(|e| LlvmError::Codegen(format!("If merge unreachable: {e}")))?;
            }
        }

        Ok(())
    }
}

#[derive(Clone, Copy)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BitAnd,
}

impl BinOp {
    fn name(self) -> &'static str {
        match self {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            BinOp::Div => "sdiv",
            BinOp::Mod => "srem",
            BinOp::BitAnd => "and",
        }
    }
}

/// Inline lookup table used by `emit_load_field`. Picks the LLVM
/// integer type + the IR tag we push back onto the operand stack
/// for a Phase-B-supported scalar field type.
impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    fn field_load_kind(
        &self,
        ty: IrType,
    ) -> Result<(inkwell::types::BasicTypeEnum<'ctx>, IrType), LlvmError> {
        let pair: (inkwell::types::BasicTypeEnum<'ctx>, IrType) = match ty {
            IrType::I64 => (self.ctx.i64_type().into(), IrType::I64),
            IrType::I32 => (self.ctx.i32_type().into(), IrType::I32),
            IrType::F64 => (self.ctx.f64_type().into(), IrType::F64),
            IrType::Bool => (self.ctx.i8_type().into(), IrType::Bool),
            IrType::Null => (self.ctx.i8_type().into(), IrType::Null),
            other => {
                return Err(LlvmError::Codegen(format!(
                    "LoadField: Phase B envelope rejects {other:?}"
                )));
            }
        };
        Ok(pair)
    }
}

// ---------------------------------------------------------------------------
// Phase E.1: raw-memory primitives, scratch allocator, StrConcatN.
// ---------------------------------------------------------------------------

/// Variants of the absolute-pointer load lowering paths.
#[derive(Clone, Copy)]
enum AbsLoad {
    I32,
    I64,
    I8U,
    F64,
}

/// Variants of the absolute-pointer store lowering paths.
#[derive(Clone, Copy)]
enum AbsStore {
    I32,
    I64,
    I8,
    F64,
}

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Compute `arena_base + off_i32` as an LLVM pointer. Mirrors
    /// `Codegen::arena_addr` on the cranelift side — used by every
    /// `*AtAbsolute` lowering path. No bounds check (Phase E.1 retains
    /// the same "trust the IR + LLVM trap on UB" stance as Phase B).
    fn arena_addr_i32(&mut self, off_i32: IntValue<'ctx>) -> Result<PointerValue<'ctx>, LlvmError> {
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("absolute load/store outside buffer-protocol entry shape".into())
        })?;
        let i64_t = self.ctx.i64_type();
        let i8_t = self.ctx.i8_type();
        let name = self.next_name("abs_off_zext");
        let off64 = self
            .builder
            .build_int_z_extend(off_i32, i64_t, &name)
            .map_err(|e| LlvmError::Codegen(format!("abs offset zext: {e}")))?;
        let name = self.next_name("abs_addr");
        let addr = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, arena_base_ptr, &[off64], &name)
                .map_err(|e| LlvmError::Codegen(format!("abs GEP: {e}")))?
        };
        Ok(addr)
    }

    /// Compose `base + offset` (both i32) into the absolute pointer
    /// each `Load*AtAbsolute` / `Store*AtAbsolute` op reads from.
    fn compose_abs_addr(
        &mut self,
        base: IntValue<'ctx>,
        offset: u32,
    ) -> Result<PointerValue<'ctx>, LlvmError> {
        let off_const = self.ctx.i32_type().const_int(u64::from(offset), false);
        let name = self.next_name("abs_compose");
        let composed = self
            .builder
            .build_int_add(base, off_const, &name)
            .map_err(|e| LlvmError::Codegen(format!("abs compose add: {e}")))?;
        self.arena_addr_i32(composed)
    }

    fn emit_load_at_absolute(
        &mut self,
        ip_hint: &str,
        offset: u32,
        kind: AbsLoad,
    ) -> Result<(), LlvmError> {
        let base = self.pop_int(ip_hint)?;
        let addr = self.compose_abs_addr(base, offset)?;
        match kind {
            AbsLoad::I32 => {
                let name = self.next_name("loadi32_abs");
                let v = self
                    .builder
                    .build_load(self.ctx.i32_type(), addr, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadI32AtAbsolute: {e}")))?
                    .into_int_value();
                self.push(v, IrType::I32);
            }
            AbsLoad::I64 => {
                let name = self.next_name("loadi64_abs");
                let v = self
                    .builder
                    .build_load(self.ctx.i64_type(), addr, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadI64AtAbsolute: {e}")))?
                    .into_int_value();
                self.push(v, IrType::I64);
            }
            AbsLoad::I8U => {
                let name = self.next_name("loadi8u_abs");
                let b = self
                    .builder
                    .build_load(self.ctx.i8_type(), addr, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadI8UAtAbsolute: {e}")))?
                    .into_int_value();
                let name = self.next_name("loadi8u_zext");
                let v = self
                    .builder
                    .build_int_z_extend(b, self.ctx.i32_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadI8UAtAbsolute zext: {e}")))?;
                self.push(v, IrType::I32);
            }
            AbsLoad::F64 => {
                // Float ops are outside the present W3/W4 envelope; we
                // still accept LoadF64AtAbsolute to keep the dispatcher
                // exhaustive. The stack carries the bit-cast i64.
                let name = self.next_name("loadf64_abs");
                let v = self
                    .builder
                    .build_load(self.ctx.f64_type(), addr, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadF64AtAbsolute: {e}")))?;
                // Bit-cast to i64 to feed the int-typed virtual stack.
                let i64_t = self.ctx.i64_type();
                let name = self.next_name("loadf64_bitcast");
                let bits = self
                    .builder
                    .build_bit_cast(v, i64_t, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadF64 bitcast: {e}")))?
                    .into_int_value();
                self.push(bits, IrType::F64);
            }
        }
        Ok(())
    }

    fn emit_store_at_absolute(
        &mut self,
        ip_hint: &str,
        offset: u32,
        kind: AbsStore,
    ) -> Result<(), LlvmError> {
        // Stack: `[base, value]` — top is the value, below it is the
        // base. Mirrors cranelift's pop order.
        let value = self.pop_int(ip_hint)?;
        let base = self.pop_int(ip_hint)?;
        let addr = self.compose_abs_addr(base, offset)?;
        match kind {
            AbsStore::I32 => {
                self.builder
                    .build_store(addr, value)
                    .map_err(|e| LlvmError::Codegen(format!("StoreI32AtAbsolute: {e}")))?;
            }
            AbsStore::I64 => {
                self.builder
                    .build_store(addr, value)
                    .map_err(|e| LlvmError::Codegen(format!("StoreI64AtAbsolute: {e}")))?;
            }
            AbsStore::I8 => {
                // Narrow the i32 value to i8 before the store.
                let name = self.next_name("storei8_trunc");
                let narrowed = self
                    .builder
                    .build_int_truncate(value, self.ctx.i8_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("StoreI8AtAbsolute trunc: {e}")))?;
                self.builder
                    .build_store(addr, narrowed)
                    .map_err(|e| LlvmError::Codegen(format!("StoreI8AtAbsolute: {e}")))?;
            }
            AbsStore::F64 => {
                // The IR's virtual stack carries f64 as bit-cast i64;
                // bit-cast back before the store so the destination
                // bytes match the wasm f64 wire layout.
                let name = self.next_name("storef64_bitcast");
                let f = self
                    .builder
                    .build_bit_cast(value, self.ctx.f64_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("StoreF64 bitcast: {e}")))?;
                self.builder
                    .build_store(addr, f)
                    .map_err(|e| LlvmError::Codegen(format!("StoreF64AtAbsolute: {e}")))?;
            }
        }
        Ok(())
    }

    /// Lower `Op::MemcpyAtAbsolute`. Stack: `[dst, src, len]`. Calls
    /// LLVM's `llvm.memcpy.p0.p0.i64` intrinsic with both pointers
    /// resolved through `arena_base`.
    fn emit_memcpy_at_absolute(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let len = self.pop_int(ip_hint)?;
        let src = self.pop_int(ip_hint)?;
        let dst = self.pop_int(ip_hint)?;
        let dst_ptr = self.arena_addr_i32(dst)?;
        let src_ptr = self.arena_addr_i32(src)?;
        // `inkwell`'s `build_memcpy` requires the length to be the
        // pointer-width int. Widen our i32 length to i64 (zero-extend).
        let i64_t = self.ctx.i64_type();
        let len64 = self
            .builder
            .build_int_z_extend(len, i64_t, "memcpy_len_zext")
            .map_err(|e| LlvmError::Codegen(format!("memcpy len zext: {e}")))?;
        // Pick a 1-byte alignment hint — the inner records aren't
        // guaranteed > 1-byte aligned (string headers land on 4-byte
        // boundaries but their payload follows immediately). The LLVM
        // optimiser will refine when it can prove a tighter bound.
        self.builder
            .build_memcpy(dst_ptr, 1, src_ptr, 1, len64)
            .map_err(|e| LlvmError::Codegen(format!("MemcpyAtAbsolute build: {e}")))?;
        Ok(())
    }

    /// Bump-allocate `size_v` (i32) bytes inside the arena's scratch
    /// region. Pushes the pre-bump cursor as an arena-relative i32
    /// offset onto the virtual stack — same shape as cranelift's
    /// `emit_alloc_scratch`.
    fn emit_alloc_scratch_common(&mut self, size_v: IntValue<'ctx>) -> Result<(), LlvmError> {
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen(
                "AllocScratch outside buffer-protocol entry shape (no state ptr)".into(),
            )
        })?;
        let i32_t = self.ctx.i32_type();
        let i8_t = self.ctx.i8_type();

        // GEP-then-load helpers. We hand-roll the i8-offset GEPs
        // because the inkwell wrappers expect a struct field accessor.
        let cursor_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_SCRATCH_CURSOR), false)],
                    "scratch_cursor_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("scratch_cursor GEP: {e}")))?
        };
        let cur = self
            .builder
            .build_load(i32_t, cursor_gep, "scratch_cursor")
            .map_err(|e| LlvmError::Codegen(format!("scratch_cursor load: {e}")))?
            .into_int_value();
        let base_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_SCRATCH_BASE), false)],
                    "scratch_base_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("scratch_base GEP: {e}")))?
        };
        let scratch_base = self
            .builder
            .build_load(i32_t, base_gep, "scratch_base")
            .map_err(|e| LlvmError::Codegen(format!("scratch_base load: {e}")))?
            .into_int_value();

        // Returned arena-relative offset = scratch_base + cur.
        let off = self
            .builder
            .build_int_add(scratch_base, cur, "scratch_off")
            .map_err(|e| LlvmError::Codegen(format!("scratch off add: {e}")))?;
        // New cursor = cur + size.
        let new_cur = self
            .builder
            .build_int_add(cur, size_v, "scratch_new_cur")
            .map_err(|e| LlvmError::Codegen(format!("scratch cur bump: {e}")))?;
        self.builder
            .build_store(cursor_gep, new_cur)
            .map_err(|e| LlvmError::Codegen(format!("scratch cursor store: {e}")))?;
        self.push(off, IrType::I32);
        Ok(())
    }

    fn emit_alloc_scratch_static(&mut self, size_bytes: u32) -> Result<(), LlvmError> {
        let size_v = self.ctx.i32_type().const_int(u64::from(size_bytes), false);
        self.emit_alloc_scratch_common(size_v)
    }

    fn emit_alloc_scratch_dyn(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let size = self.pop_int(ip_hint)?;
        self.emit_alloc_scratch_common(size)
    }

    /// Lower `Op::StrConcatN { operand_count }`. Pops N i32 arena
    /// offsets, sums their `[len: u32]` headers, allocates one scratch
    /// record sized `total + 4`, stamps the header, then memcpys each
    /// operand's payload at the running cursor. Pushes the resulting
    /// i32 offset. Mirrors cranelift's `emit_str_concat_n`.
    fn emit_str_concat_n(&mut self, ip_hint: &str, operand_count: u32) -> Result<(), LlvmError> {
        if operand_count < 2 {
            return Err(LlvmError::Codegen(format!(
                "Op::StrConcatN with operand_count={operand_count} (expected >= 2)"
            )));
        }
        let n = operand_count as usize;
        let i32_t = self.ctx.i32_type();
        // Pop N i32 offsets; reverse so source-order matches stack-
        // order (deepest leaf is the first operand).
        let mut offs: Vec<IntValue<'ctx>> = Vec::with_capacity(n);
        for _ in 0..n {
            offs.push(self.pop_int(ip_hint)?);
        }
        offs.reverse();
        // Load each operand's `[len: u32]` header once.
        let mut lens: Vec<IntValue<'ctx>> = Vec::with_capacity(n);
        for off in &offs {
            let addr = self.arena_addr_i32(*off)?;
            let name = self.next_name("strconcat_len");
            let l = self
                .builder
                .build_load(i32_t, addr, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN len load: {e}")))?
                .into_int_value();
            lens.push(l);
        }
        // total_len = Σ lens.
        let mut total_len = lens[0];
        for v in &lens[1..] {
            let name = self.next_name("strconcat_sumlen");
            total_len = self
                .builder
                .build_int_add(total_len, *v, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN sum: {e}")))?;
        }
        // record_size = total_len + 4 (header).
        let four = i32_t.const_int(4, false);
        let name = self.next_name("strconcat_recsize");
        let record_size = self
            .builder
            .build_int_add(total_len, four, &name)
            .map_err(|e| LlvmError::Codegen(format!("StrConcatN record_size: {e}")))?;
        // Allocate the scratch record.
        self.emit_alloc_scratch_common(record_size)?;
        let base_off = self.pop_int(ip_hint)?;
        // Write header: i32.store(base, total_len).
        let base_abs = self.arena_addr_i32(base_off)?;
        self.builder
            .build_store(base_abs, total_len)
            .map_err(|e| LlvmError::Codegen(format!("StrConcatN header store: {e}")))?;
        // Walk operands in source order, copying payloads at the
        // running cursor.
        let name = self.next_name("strconcat_cursor0");
        let mut cursor_off = self
            .builder
            .build_int_add(base_off, four, &name)
            .map_err(|e| LlvmError::Codegen(format!("StrConcatN cursor init: {e}")))?;
        for i in 0..n {
            let len = lens[i];
            let name = self.next_name("strconcat_srcoff");
            let src_off_payload = self
                .builder
                .build_int_add(offs[i], four, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN src off: {e}")))?;
            let dst_ptr = self.arena_addr_i32(cursor_off)?;
            let src_ptr = self.arena_addr_i32(src_off_payload)?;
            let i64_t = self.ctx.i64_type();
            let name = self.next_name("strconcat_lenzext");
            let len64 = self
                .builder
                .build_int_z_extend(len, i64_t, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN len zext: {e}")))?;
            self.builder
                .build_memcpy(dst_ptr, 1, src_ptr, 1, len64)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN memcpy: {e}")))?;
            let name = self.next_name("strconcat_cursornext");
            cursor_off = self
                .builder
                .build_int_add(cursor_off, len, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN cursor bump: {e}")))?;
        }
        // Push the resulting record offset.
        self.push(base_off, IrType::String);
        Ok(())
    }

    /// Lower `Op::StoreField { ty }` for pointer-indirect types
    /// (`String`, `ListInt`, `ListFloat`, `ListBool`). Pops the source
    /// arena offset, copies the `[len:u32 LE][payload]` record into
    /// the output buffer's tail region (`out_ptr + tail_cursor`),
    /// writes `tail_cursor` (buffer-relative offset of the new record)
    /// into the fixed-area slot at `offset`, and bumps `tail_cursor`.
    /// Mirrors cranelift's `emit_store_pointer_indirect`.
    fn emit_store_field_pointer_indirect(
        &mut self,
        ip_hint: &str,
        offset: u32,
        ty: IrType,
    ) -> Result<(), LlvmError> {
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("StoreField (pointer-indirect) outside buffer entry".into())
        })?;
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen("StoreField (pointer-indirect): missing state ptr".into())
        })?;
        let src_off_i32 = self.pop_int(ip_hint)?;
        let i32_t = self.ctx.i32_type();
        let i8_t = self.ctx.i8_type();
        // Read the record's `[len: u32]` header to size the memcpy.
        let src_abs = self.arena_addr_i32(src_off_i32)?;
        let len_i32 = self
            .builder
            .build_load(i32_t, src_abs, "ptr_indirect_len")
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect len load: {e}")))?
            .into_int_value();
        let record_size = match ty {
            IrType::String => {
                let four = i32_t.const_int(4, false);
                self.builder
                    .build_int_add(len_i32, four, "string_recsize")
                    .map_err(|e| LlvmError::Codegen(format!("String record_size: {e}")))?
            }
            IrType::ListInt | IrType::ListFloat => {
                // record_size = 8 + 8 * element_count.
                let three = i32_t.const_int(3, false);
                let shifted = self
                    .builder
                    .build_left_shift(len_i32, three, "list_shl")
                    .map_err(|e| LlvmError::Codegen(format!("list shl: {e}")))?;
                let eight = i32_t.const_int(8, false);
                self.builder
                    .build_int_add(shifted, eight, "list_recsize")
                    .map_err(|e| LlvmError::Codegen(format!("list record_size: {e}")))?
            }
            IrType::ListBool => {
                let four = i32_t.const_int(4, false);
                self.builder
                    .build_int_add(len_i32, four, "listbool_recsize")
                    .map_err(|e| LlvmError::Codegen(format!("listbool record_size: {e}")))?
            }
            _ => {
                return Err(LlvmError::Codegen(format!(
                    "emit_store_field_pointer_indirect: unsupported {ty:?}"
                )));
            }
        };
        // Pick the alignment for the tail bump. String / ListBool stay
        // 4-aligned (the leading u32 length); ListInt / ListFloat need
        // 8 so the i64 / f64 payload that follows is aligned.
        let align: u32 = match ty {
            IrType::String | IrType::ListBool => 4,
            IrType::ListInt | IrType::ListFloat => 8,
            _ => unreachable!(),
        };
        // Tail bump: aligned = align_up(cur, align); new_cur = aligned + record_size.
        let tail_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_TAIL_CURSOR), false)],
                    "tail_cursor_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("tail_cursor GEP: {e}")))?
        };
        let cur = self
            .builder
            .build_load(i32_t, tail_gep, "tail_cursor_pre")
            .map_err(|e| LlvmError::Codegen(format!("tail_cursor load: {e}")))?
            .into_int_value();
        let aligned = if align <= 1 {
            cur
        } else {
            let add = i32_t.const_int(u64::from(align - 1), false);
            let mask_val = !(align - 1);
            let mask = i32_t.const_int(u64::from(mask_val), false);
            let sum = self
                .builder
                .build_int_add(cur, add, "tail_align_sum")
                .map_err(|e| LlvmError::Codegen(format!("tail align add: {e}")))?;
            self.builder
                .build_and(sum, mask, "tail_align_and")
                .map_err(|e| LlvmError::Codegen(format!("tail align and: {e}")))?
        };
        let new_cur = self
            .builder
            .build_int_add(aligned, record_size, "tail_cursor_post")
            .map_err(|e| LlvmError::Codegen(format!("tail cur bump: {e}")))?;
        self.builder
            .build_store(tail_gep, new_cur)
            .map_err(|e| LlvmError::Codegen(format!("tail cursor store: {e}")))?;
        // memcpy(arena_base + out_ptr + aligned, src_abs, record_size).
        let out_ptr_i32 = self.lookup_param(2)?; // IR LocalGet(2) == out_ptr
        let dst_off = self
            .builder
            .build_int_add(out_ptr_i32, aligned, "ptr_indirect_dst_off")
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect dst off: {e}")))?;
        let dst_ptr = self.arena_addr_i32(dst_off)?;
        let i64_t = self.ctx.i64_type();
        let rec64 = self
            .builder
            .build_int_z_extend(record_size, i64_t, "ptr_indirect_rec64")
            .map_err(|e| LlvmError::Codegen(format!("rec64 zext: {e}")))?;
        let _ = arena_base_ptr;
        self.builder
            .build_memcpy(dst_ptr, align, src_abs, 1, rec64)
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect memcpy: {e}")))?;
        // Store `aligned` (buffer-relative offset of the new record)
        // into the fixed-area slot at `out_ptr + offset`.
        let slot_off = self
            .builder
            .build_int_add(
                out_ptr_i32,
                i32_t.const_int(u64::from(offset), false),
                "ptr_indirect_slot_off",
            )
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect slot off: {e}")))?;
        let slot_addr = self.arena_addr_i32(slot_off)?;
        self.builder
            .build_store(slot_addr, aligned)
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect slot store: {e}")))?;
        // Flag the body so the buffer-protocol epilogue returns the
        // post-bump tail cursor.
        self.needs_tail_cursor = true;
        Ok(())
    }

    /// Lower `Op::Call { fn_index, ... }` by inlining the bundled
    /// stdlib body. Mirrors cranelift's `emit_call_stdlib` — pop the
    /// args, set up an inline frame with an exit-block target, lower
    /// the callee body recursively against the inline frame, then
    /// continue at the exit block with the loaded result on the stack.
    fn emit_call_stdlib(
        &mut self,
        ip_hint: &str,
        fn_index: u32,
        arg_count: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        let stdlib = relon_ir::stdlib::builtin_stdlib();
        let callee = stdlib.get(fn_index as usize).ok_or_else(|| {
            LlvmError::Codegen(format!(
                "Op::Call fn_index {fn_index} outside bundled stdlib (max {})",
                stdlib.len()
            ))
        })?;
        if callee.params.len() != arg_count as usize {
            return Err(LlvmError::Codegen(format!(
                "Op::Call to `{}` declares {arg_count} args but callee has {}",
                callee.name,
                callee.params.len()
            )));
        }
        for (i, (declared, expected)) in callee.params.iter().zip(param_tys.iter()).enumerate() {
            if declared != expected {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call to `{}` arg #{i}: callee expects {declared:?}, IR tags {expected:?}",
                    callee.name
                )));
            }
        }
        // Pop args in reverse so `params[i]` is the i-th declared arg.
        let mut args: Vec<TypedValue<'ctx>> = Vec::with_capacity(arg_count as usize);
        for _ in 0..arg_count {
            args.push(self.pop(ip_hint)?);
        }
        args.reverse();

        // Pick a let_offset window past any active let slots so the
        // callee's `LetSet 0` lands at `let_offset + 0` and never
        // clashes with the caller's bindings. Cranelift uses
        // `max(idx) + 1`; we do the same by inspecting `let_slots`.
        let let_offset = self
            .let_slots
            .keys()
            .copied()
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);

        // Alloca for the callee's return value. The callee's
        // `Op::Return` stores into this slot then jumps to `exit_bb`;
        // the caller-side load below pushes the value back on the
        // virtual stack.
        let ret_llvm_ty: inkwell::types::BasicTypeEnum<'ctx> = match ret_ty {
            IrType::I64 => self.ctx.i64_type().into(),
            IrType::I32
            | IrType::Bool
            | IrType::Null
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool => self.ctx.i32_type().into(),
            other => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call ret_ty {other:?} unsupported in inline frame"
                )));
            }
        };
        // Allocate the ret slot in the function's entry block so it
        // stays out of any loop body; mem2reg promotes it on -O2/-O3.
        let entry_bb = self.func.get_first_basic_block().ok_or_else(|| {
            LlvmError::Codegen("emit_call_stdlib: function has no entry block".into())
        })?;
        let cur = self.builder.get_insert_block();
        if let Some(first_instr) = entry_bb.get_first_instruction() {
            self.builder.position_before(&first_instr);
        } else {
            self.builder.position_at_end(entry_bb);
        }
        let ret_slot = self
            .builder
            .build_alloca(ret_llvm_ty, "call_ret_slot")
            .map_err(|e| LlvmError::Codegen(format!("call ret_slot alloca: {e}")))?;
        if let Some(bb) = cur {
            self.builder.position_at_end(bb);
        }

        let exit_bb = self.ctx.append_basic_block(self.func, "call_exit");
        let frame = InlineFrame {
            params: args,
            let_offset,
            ret_slot,
            ret_ty,
            exit_bb,
        };
        self.inline_frames.push(frame);
        let body = callee.body_owned();
        let result = self.lower_body(&body);
        // Always pop the frame before returning the error so the emit
        // state stays consistent on failure.
        self.inline_frames.pop();
        result?;

        // After the inline body finishes the current block has either
        // hit `Op::Return` (which terminated via `br exit_bb`) or fell
        // through. If it fell through, branch to exit_bb so the
        // load + push below has a single in-edge.
        let cur_terminated = self
            .builder
            .get_insert_block()
            .and_then(|bb| bb.get_terminator())
            .is_some();
        if !cur_terminated {
            self.builder
                .build_unconditional_branch(exit_bb)
                .map_err(|e| LlvmError::Codegen(format!("inline call fallthrough: {e}")))?;
        }
        // Position at the exit block and load the result.
        self.builder.position_at_end(exit_bb);
        let name = self.next_name("call_ret_load");
        let v = self
            .builder
            .build_load(ret_llvm_ty, ret_slot, &name)
            .map_err(|e| LlvmError::Codegen(format!("inline call ret load: {e}")))?
            .into_int_value();
        self.push(v, ret_ty);
        Ok(())
    }

    /// Phase F.1: lower `contains(haystack: String, needle: String) ->
    /// Bool` by emitting a direct extern call to
    /// `relon_llvm_str_contains_arena` instead of inlining the bundled
    /// stdlib body. See the `str_helpers` module docs for the ABI and
    /// the rationale (W4 / W4_long gap vs LuaJIT closed by std's
    /// SIMD-backed `str::contains`).
    ///
    /// Operand stack contract: pops `needle_off` (top), then
    /// `haystack_off`. Pushes the i32 0/1 result tagged as
    /// [`IrType::Bool`] so downstream `If` / `BrIf` ops see the same
    /// width the inlined body would have produced.
    fn emit_str_contains_extern(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        // Pop in reverse order: IR pushes `[haystack, needle]`, so the
        // top-of-stack is the needle. We need to materialise the
        // pointers in declaration order (haystack first) for the call,
        // so collect the offsets first and resolve to pointers below.
        let needle_off = self.pop_int(ip_hint)?;
        let haystack_off = self.pop_int(ip_hint)?;
        self.emit_str_contains_extern_with_offsets(ip_hint, haystack_off, needle_off)
    }

    /// Phase H: shared "given already-popped i32 offsets, emit the
    /// extern shim call" backbone. Split out of
    /// [`Self::emit_str_contains_extern`] so the const-needle
    /// fast path can reuse the extern fallback for `needle.len() > 1`
    /// (where the inline byte-scan no longer wins over the shim's
    /// SIMD-backed Two-Way matcher).
    fn emit_str_contains_extern_with_offsets(
        &mut self,
        _ip_hint: &str,
        haystack_off: IntValue<'ctx>,
        needle_off: IntValue<'ctx>,
    ) -> Result<(), LlvmError> {
        // GEP into the cached arena base. Mirrors `emit_load_at_absolute`
        // / `emit_str_concat_n` — both produce `arena_base + off_i32`
        // pointers the inner ops then read through. The shim consumes
        // raw `*const u8` headers, so we hand the GEP result directly.
        let haystack_ptr = self.arena_addr_i32(haystack_off)?;
        let needle_ptr = self.arena_addr_i32(needle_off)?;

        // Declare (or look up) the extern shim. Idempotent so multiple
        // `contains` call sites in the same module share a single
        // declaration — LLVM's verifier rejects duplicate function
        // definitions but happily reuses an existing extern.
        let shim = self.declare_str_contains_extern();

        let call_name = self.next_name("str_contains_extern");
        let call_site = self
            .builder
            .build_call(
                shim,
                &[
                    BasicMetadataValueEnum::PointerValue(haystack_ptr),
                    BasicMetadataValueEnum::PointerValue(needle_ptr),
                ],
                &call_name,
            )
            .map_err(|e| LlvmError::Codegen(format!("str_contains call: {e}")))?;

        let ret_val = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(v) => v,
            inkwell::values::ValueKind::Instruction(_) => {
                return Err(LlvmError::Codegen(
                    "relon_llvm_str_contains_arena returned void; expected i32".into(),
                ));
            }
        };
        let ret_i32 = match ret_val {
            BasicValueEnum::IntValue(v) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "relon_llvm_str_contains_arena returned non-int {other:?}"
                )));
            }
        };
        // Bool is encoded as i32 (0 / 1) across the LLVM AOT envelope,
        // matching what the inlined `contains_string_body` would have
        // produced through `Op::Ne(I32)` against `0`. No truncation /
        // sign-extension needed — the shim returns the same 0/1 i32
        // shape downstream `BrIf` / `Eq(Bool)` consumers expect.
        self.push(ret_i32, IrType::Bool);
        Ok(())
    }

    /// Phase H: lower `contains(haystack, "literal") -> Bool` for the
    /// const-needle case detected at the `Op::Call` site.
    ///
    /// Operand stack contract: pops `needle_off` (top — discarded; we
    /// have the literal bytes), then `haystack_off`, pushes the i32
    /// 0/1 result as [`IrType::Bool`]. The needle's arena-record
    /// pointer is unused on the fast paths because we already know
    /// the bytes at compile time.
    ///
    /// Dispatch by needle length:
    /// - `0` — every haystack contains the empty string; push `i32(1)`
    ///   directly. Matches `core::str::contains("")`'s semantics and
    ///   the bundled stdlib body's `p_len == 0 → true` short-circuit.
    /// - `1` — emit an inline byte-scan loop against the cached
    ///   haystack record. LLVM 18's loop vectoriser recognises the
    ///   single-byte equality scan and lowers it to SSE2 `pcmpeqb` +
    ///   `pmovmskb` (the same SIMD memchr LuaJIT exploits via libc).
    ///   Skips the `relon_llvm_str_contains_arena` FFI boundary — no
    ///   IC atomic loads, no register save/restore, no spill of the
    ///   surrounding loop's IV / accumulator. Per-call cost drops
    ///   from ~5 ns (Phase G shim) to ~1.5-2 ns on x86_64. This is
    ///   the hot path for the W4 / W4_long cmp_lua rows (needle =
    ///   `"x"`).
    /// - `> 1` — fall through to the extern shim. The shim's
    ///   `compute_contains` uses `str::contains` with Rust's Two-Way
    ///   matcher; inlining that here would balloon the IR for no
    ///   measured win (the multi-byte case isn't on the W4 / W4_long
    ///   hot loop).
    fn emit_str_contains_const_needle(
        &mut self,
        ip_hint: &str,
        needle_bytes: &[u8],
    ) -> Result<(), LlvmError> {
        // Pop both operands up-front. For `len == 0` / `len == 1` we
        // discard `needle_off` — the inline path reads the needle byte
        // from the source-emitted `needle_bytes` slice. For `len > 1`
        // we forward both offsets to the shim path.
        let needle_off = self.pop_int(ip_hint)?;
        let haystack_off = self.pop_int(ip_hint)?;

        match needle_bytes.len() {
            0 => {
                // Empty needle: always matches. Push `i32(1)` typed as
                // Bool to match the inlined stdlib body's encoding.
                let one = self.ctx.i32_type().const_int(1, false);
                self.push(one, IrType::Bool);
                Ok(())
            }
            1 => self.emit_str_contains_inline_byte(ip_hint, haystack_off, needle_bytes[0]),
            _ => {
                // Multi-byte needle: shim with Two-Way matcher beats a
                // naive open-coded scan. Forward both offsets.
                self.emit_str_contains_extern_with_offsets(ip_hint, haystack_off, needle_off)
            }
        }
    }

    /// Phase H: emit a direct libc `memchr` call for the single-byte
    /// const-needle case. Pushes the i32 0/1 result tagged as
    /// [`IrType::Bool`].
    ///
    /// IR shape (haystack record at `arena_base + haystack_off` carries
    /// `[len_u32 LE][payload bytes]`):
    ///
    /// ```text
    /// hay_len   = load i32, ptr (arena_base + haystack_off)
    /// hay_payld = gep (arena_base + haystack_off + 4)
    /// hay_len64 = zext i32 hay_len -> i64
    /// res_ptr   = call ptr @memchr(ptr hay_payld, i32 needle_byte, i64 hay_len64)
    /// hit       = icmp ne ptr res_ptr, null
    /// result    = zext i1 hit -> i32
    /// ```
    ///
    /// ## Why direct libc memchr instead of an open-coded scan?
    ///
    /// LLVM 18's loop vectoriser refuses to vectorise the open-coded
    /// scan because the inner body has a data-dependent early exit
    /// (`if byte == needle break`). Without vectorisation the W4_long
    /// row's 256-byte haystack would walk byte-by-byte at ~1 ns / byte
    /// — a ~256 ns/iter regression vs the Phase G shim's SIMD-backed
    /// `core::slice::contains(&u8)` (which calls into the `memchr`
    /// crate's `memchr` function, in turn delegating to libc on
    /// Linux). Calling libc `memchr` directly gives us the same SIMD
    /// `pcmpeqb` + `pmovmskb` lowering glibc ships, *without* the
    /// Phase G shim's per-call IC + record-parsing overhead.
    ///
    /// ## Symbol resolution
    ///
    /// `memchr` is in libc, resolved by MCJIT's default `dlsym` lookup
    /// when the symbol is declared with [`Linkage::External`]. No
    /// explicit `engine.add_global_mapping` call is required (the
    /// Phase F.1 shim needed one because its symbol lives inside the
    /// relon-codegen-llvm dylib, which dlsym can't see from MCJIT).
    fn emit_str_contains_inline_byte(
        &mut self,
        _ip_hint: &str,
        haystack_off: IntValue<'ctx>,
        needle_byte: u8,
    ) -> Result<(), LlvmError> {
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());
        let four = i32_t.const_int(4, false);
        let needle_arg = i32_t.const_int(u64::from(needle_byte), false);

        // Materialise haystack record header + payload pointer.
        let hay_hdr_ptr = self.arena_addr_i32(haystack_off)?;
        let hay_len_name = self.next_name("strc_inl_haylen");
        let hay_len = self
            .builder
            .build_load(i32_t, hay_hdr_ptr, &hay_len_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline hay_len: {e}")))?
            .into_int_value();
        let payload_off_name = self.next_name("strc_inl_payoff");
        let payload_off = self
            .builder
            .build_int_add(haystack_off, four, &payload_off_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline payload_off: {e}")))?;
        let hay_payload_ptr = self.arena_addr_i32(payload_off)?;
        let hay_len64_name = self.next_name("strc_inl_haylen64");
        let hay_len64 = self
            .builder
            .build_int_z_extend(hay_len, i64_t, &hay_len64_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline hay_len64: {e}")))?;

        // Declare libc `memchr` once per module.
        let memchr_fn = self.declare_libc_memchr();
        let call_name = self.next_name("strc_inl_memchr");
        let call_site = self
            .builder
            .build_call(
                memchr_fn,
                &[
                    BasicMetadataValueEnum::PointerValue(hay_payload_ptr),
                    BasicMetadataValueEnum::IntValue(needle_arg),
                    BasicMetadataValueEnum::IntValue(hay_len64),
                ],
                &call_name,
            )
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline memchr call: {e}")))?;
        let res_ptr_basic = call_site.try_as_basic_value();
        let res_ptr = match res_ptr_basic {
            inkwell::values::ValueKind::Basic(BasicValueEnum::PointerValue(p)) => p,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "memchr returned non-pointer: {other:?}"
                )));
            }
        };
        let null_ptr = ptr_t.const_null();
        let hit_name = self.next_name("strc_inl_hit");
        let hit_i1 = self
            .builder
            .build_int_compare(IntPredicate::NE, res_ptr, null_ptr, &hit_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline cmp: {e}")))?;
        let res_name = self.next_name("strc_inl_res");
        let res_v = self
            .builder
            .build_int_z_extend(hit_i1, i32_t, &res_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline zext: {e}")))?;
        self.push(res_v, IrType::Bool);
        Ok(())
    }

    /// Idempotent declaration of libc `memchr`. Returns the cached
    /// `FunctionValue` so callers can issue `build_call` without
    /// re-parsing the signature. MCJIT's default `dlsym` resolver
    /// picks up the libc symbol — no `engine.add_global_mapping` is
    /// required.
    fn declare_libc_memchr(&self) -> FunctionValue<'ctx> {
        const SYM: &str = "memchr";
        if let Some(f) = self.module.get_function(SYM) {
            return f;
        }
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        // memchr signature: const void *memchr(const void *s, int c, size_t n)
        let fn_ty = ptr_t.fn_type(&[ptr_t.into(), i32_t.into(), i64_t.into()], false);
        self.module
            .add_function(SYM, fn_ty, Some(Linkage::External))
    }

    /// Idempotent declaration of the
    /// [`crate::str_helpers::relon_llvm_str_contains_arena`] extern.
    /// Returns the cached `FunctionValue` so callers can issue
    /// `build_call` without re-parsing the signature on every call site.
    fn declare_str_contains_extern(&self) -> FunctionValue<'ctx> {
        let sym = crate::str_helpers::RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL;
        if let Some(f) = self.module.get_function(sym) {
            return f;
        }
        let i32_t = self.ctx.i32_type();
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());
        let fn_ty = i32_t.fn_type(&[ptr_t.into(), ptr_t.into()], false);
        self.module
            .add_function(sym, fn_ty, Some(Linkage::External))
    }
}
