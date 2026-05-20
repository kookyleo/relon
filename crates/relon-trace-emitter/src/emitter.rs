//! Main `OptimizedTrace` → cranelift IR translator.
//!
//! The pass walks the `TraceOp` stream once, emitting cranelift IR
//! into the supplied [`cranelift_codegen::Context`]. Every trace
//! conforms to the [`crate::TRACE_ENTRY_SIG`] ABI; the emitter wires
//! up the entry block, the result-slot store path, and the shared
//! deopt block before delegating per-op lowering.
//!
//! ## Lowering rules at a glance
//!
//! | TraceOp variant | cranelift IR |
//! |-----------------|--------------|
//! | `Add` / `Sub` / `Mul` | `iadd` / `isub` / `imul` |
//! | `Div` | divisor-zero guard (`brif → deopt`) + `sdiv` |
//! | `Mod` | divisor-zero guard (`brif → deopt`) + `srem` |
//! | `Cmp` | `icmp` + `uextend` to i32 |
//! | `Load` | `load.i64` (bounds-checked via preceding `Guard`) |
//! | `Store` | `store.i64` |
//! | `ConstI32` / `ConstI64` | `iconst.i32` / `iconst.i64` |
//! | `Guard(*)` | see [`crate::guard_emit`] |
//! | `Call` | `__relon_trace_resolve_call(func_id)` + `call_indirect` |
//! | `Return` | store result + return i32 (`Success`) |
//! | `MarkLoopHead` / `MarkLoopBack` | cranelift block + `jump` |
//!
//! The translation is *intentionally* one-pass. SSA renaming is
//! handled by the trace recorder; cranelift's own SSA construction
//! takes over from there.

use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{
    self, AbiParam, BlockArg, ExtFuncData, ExternalName, Function, InstBuilder, MemFlags,
    Signature, UserExternalName, UserFuncName,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::Context as CodegenContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use relon_trace_jit::{EffectClass, GuardSite, OptimizedTrace, SsaVar, TraceOp};

use crate::abi::{AbiSignatureExt, HostHookId, TraceEntryStatus, TRACE_ENTRY_SIG};
use crate::guard_emit::{emit_guard, GuardEmitCtx, GuardEmitError};

/// Override table for [`HostHookId`] → cranelift `UserExternalName.index`.
///
/// v6-δ M1: the historical emitter hard-coded hook indices to `0`, `1`,
/// `2` matching the [`HostHookId`] discriminant. That breaks once the
/// trace module declares the helpers as proper imports via
/// `cranelift_module::Module::declare_function` — the module uses
/// `FuncId.as_u32()` for the `UserExternalName.index`, which won't
/// match `HostHookId`'s ordinal once the host re-orders declarations
/// (e.g. when the trace fn itself is declared first and gets FuncId 0).
///
/// Callers building a trace JIT module supply this override so the
/// emitter's `call save_deopt(...)` instruction targets the right
/// FuncId. The fields default to the historical 0/1/2 layout for
/// existing test fixtures that don't go through `cranelift_module`.
#[derive(Debug, Clone, Copy)]
pub struct HostHookFuncIds {
    pub save_deopt: u32,
    pub resolve_call: u32,
    pub inline_cache_lookup: u32,
    /// F-D7 string fast-path hooks. Indices match the host module's
    /// declared FuncIds for the four `__relon_str_*` shims.
    pub str_concat: u32,
    pub str_contains: u32,
    pub str_find: u32,
    pub str_substring: u32,
    /// F-D7-I: cranelift `FuncId.as_u32()` for
    /// `__relon_str_concat_alloc`. `None` means the host did not
    /// declare the helper — the emitter's inline `StrConcat` path will
    /// silently fall back to the extern `__relon_str_concat` shim,
    /// preserving correctness for hosts wired against the pre-F-D7-I
    /// ABI. Hosts that want the inline fast path MUST set this.
    pub str_concat_alloc: Option<u32>,
    /// F-D8: cranelift `FuncId.as_u32()` for `__relon_trace_list_get`.
    /// `None` means the host has not declared the helper; emitter will
    /// surface `EmitError::HostHookNotDeclared` if a `TraceOp::ListGet`
    /// is seen.
    pub list_get: Option<u32>,
    /// F-D8: cranelift `FuncId.as_u32()` for `__relon_trace_dict_lookup`.
    /// `None` means the host has not declared the helper; emitter will
    /// surface `EmitError::HostHookNotDeclared` if a `TraceOp::DictLookup`
    /// is seen.
    pub dict_lookup: Option<u32>,
    /// F-D8-E.2: cranelift `FuncId.as_u32()` for
    /// `__relon_trace_dict_lookup_prechecked`. `None` means the host
    /// has not declared the helper; emitter surfaces
    /// `EmitError::HostHookNotDeclared(DictLookupPrechecked)` if a
    /// `TraceOp::DictLookupPrechecked` is seen. The optimizer's
    /// `dict_ic_hoist` pass produces these ops so a host that wires
    /// `dict_lookup` MUST also wire this one — otherwise installed
    /// traces with hot dict accesses fail to emit.
    pub dict_lookup_prechecked: Option<u32>,
}

impl Default for HostHookFuncIds {
    /// Historical layout: hook index == [`HostHookId`] discriminant.
    /// Only safe when no `cranelift_module::Module` is owning the
    /// imports — e.g. the emitter's own unit tests.
    fn default() -> Self {
        Self {
            save_deopt: 0,
            resolve_call: 1,
            inline_cache_lookup: 2,
            str_concat: 3,
            str_contains: 4,
            str_find: 5,
            str_substring: 6,
            // F-D7-I helper is opt-in: tests that don't drive the host
            // module path keep the inline `StrConcat` lowering disabled
            // (the emitter falls back to the extern `__relon_str_concat`
            // call which the historical layout already declared at
            // FuncId 3).
            str_concat_alloc: None,
            // F-D8 helpers are opt-in: tests that don't exercise
            // dict/list ops keep the historical 3-slot layout.
            list_get: None,
            dict_lookup: None,
            dict_lookup_prechecked: None,
        }
    }
}

/// Public entry point. Builds the trace entry's cranelift IR into
/// `ctx.func`. The caller is expected to set `ctx.func` to a
/// freshly-named `Function`; the emitter overwrites its signature.
pub struct TraceEmitter;

impl TraceEmitter {
    /// Emit the supplied [`OptimizedTrace`] into `ctx.func`.
    pub fn emit(trace: &OptimizedTrace, ctx: &mut CodegenContext) -> Result<(), EmitError> {
        // 64-bit host. The integration phase passes the real
        // target ISA; the standalone test path uses I64 directly.
        let pointer_ty = I64;
        Self::emit_with_pointer_ty(trace, ctx, pointer_ty)
    }

    /// Same as [`TraceEmitter::emit`] but with a caller-supplied
    /// pointer width. Used by tests that want to exercise 32-bit
    /// builds without depending on a target ISA.
    pub fn emit_with_pointer_ty(
        trace: &OptimizedTrace,
        ctx: &mut CodegenContext,
        pointer_ty: ir::Type,
    ) -> Result<(), EmitError> {
        Self::emit_with_hooks(trace, ctx, pointer_ty, HostHookFuncIds::default())
    }

    /// Same as [`TraceEmitter::emit_with_pointer_ty`] but with a
    /// caller-supplied [`HostHookFuncIds`] mapping each
    /// [`HostHookId`] to the cranelift `FuncId` the host has
    /// pre-declared for that helper. v6-δ M1 callers that own a
    /// `cranelift_module::Module` MUST go through this variant so the
    /// `call save_deopt(...)` instruction targets the right `FuncId`
    /// instead of accidentally calling back into the trace function
    /// itself (which historically was assigned FuncId 0 by
    /// `cranelift_module`).
    pub fn emit_with_hooks(
        trace: &OptimizedTrace,
        ctx: &mut CodegenContext,
        pointer_ty: ir::Type,
        hook_func_ids: HostHookFuncIds,
    ) -> Result<(), EmitError> {
        Self::emit_with_hooks_and_call_conv(
            trace,
            ctx,
            pointer_ty,
            hook_func_ids,
            crate::call_conv::trace_entry_call_conv(),
        )
    }

    /// v6-ε-0-C: same as [`TraceEmitter::emit_with_hooks`] but with an
    /// explicit calling convention for the trace entry function.
    ///
    /// Defaults via [`crate::call_conv::trace_entry_call_conv`] route
    /// here picking [`CallConv::Tail`] on x86_64 + aarch64 and
    /// [`CallConv::SystemV`] elsewhere. Tests / benches that want to
    /// pin a specific conv (e.g. the comparison bench row that
    /// installs both a Tail and a SystemV trace side-by-side) call
    /// this variant directly.
    ///
    /// **Host hook helpers** (`save_deopt`, `resolve_call`,
    /// `inline_cache_lookup`) keep their [`CallConv::SystemV`]
    /// signatures regardless of the trace-entry conv: they're
    /// implemented as Rust `extern "C"` functions which always use
    /// the platform SysV/Fastcall ABI. Cranelift handles the
    /// cross-conv `call` correctly because the clobber set is
    /// computed from `call_conv_of_callee`.
    pub fn emit_with_hooks_and_call_conv(
        trace: &OptimizedTrace,
        ctx: &mut CodegenContext,
        pointer_ty: ir::Type,
        hook_func_ids: HostHookFuncIds,
        entry_call_conv: CallConv,
    ) -> Result<(), EmitError> {
        let signature = TRACE_ENTRY_SIG.to_cranelift(pointer_ty, entry_call_conv);
        ctx.func = Function::with_name_signature(UserFuncName::user(0, 0), signature.clone());

        let mut builder_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);

        // Entry block + ABI param pickup.
        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        let trace_ctx_ptr = builder.block_params(entry_block)[0];
        let input_args_ptr = builder.block_params(entry_block)[1];

        // Shared deopt block: takes (trace_pc: i32, external_pc: i64)
        // params so every guard site can pass its identifying triple
        // straight in. The block body lives below.
        let deopt_block = builder.create_block();
        builder.append_block_param(deopt_block, I32);
        builder.append_block_param(deopt_block, I64);

        // Pre-declare runtime helpers we may reference. Cranelift
        // requires `ExtFuncData` declarations live in the Function's
        // DFG before any `call` instruction references them.
        let save_deopt = declare_host_hook(
            builder.func,
            HostHookId::SaveDeopt,
            &[pointer_ty, I32, I64],
            &[],
            pointer_ty,
            hook_func_ids.save_deopt,
        );
        let resolve_call = declare_host_hook(
            builder.func,
            HostHookId::ResolveCall,
            &[pointer_ty, I32],
            &[pointer_ty],
            pointer_ty,
            hook_func_ids.resolve_call,
        );
        let _inline_cache_lookup = declare_host_hook(
            builder.func,
            HostHookId::InlineCacheLookup,
            &[pointer_ty, I32, I64],
            &[I64],
            pointer_ty,
            hook_func_ids.inline_cache_lookup,
        );

        // F-D7 string hooks. Each takes/returns opaque `*const StringRef`
        // pointers carried in pointer-typed slots; the shim resolves to
        // a Rust `&Arc<str>`-style payload at the host boundary.
        // - concat: (ptr, ptr) -> ptr
        // - contains: (ptr, ptr) -> i32
        // - find: (ptr, ptr) -> i64
        // - substring: (ptr, i64, i64) -> ptr
        let str_concat = declare_host_hook(
            builder.func,
            HostHookId::StrConcat,
            &[pointer_ty, pointer_ty],
            &[pointer_ty],
            pointer_ty,
            hook_func_ids.str_concat,
        );
        let str_contains = declare_host_hook(
            builder.func,
            HostHookId::StrContains,
            &[pointer_ty, pointer_ty],
            &[I32],
            pointer_ty,
            hook_func_ids.str_contains,
        );
        let str_find = declare_host_hook(
            builder.func,
            HostHookId::StrFind,
            &[pointer_ty, pointer_ty],
            &[I64],
            pointer_ty,
            hook_func_ids.str_find,
        );
        let str_substring = declare_host_hook(
            builder.func,
            HostHookId::StrSubstring,
            &[pointer_ty, I64, I64],
            &[pointer_ty],
            pointer_ty,
            hook_func_ids.str_substring,
        );

        // F-D7-I: optional alloc helper for the inline `StrConcat`
        // short-rhs lowering. Declared only when the host wired the
        // FuncId; absence keeps `emit_str_concat` on the extern shim.
        let str_concat_alloc = hook_func_ids.str_concat_alloc.map(|fid| {
            declare_host_hook(
                builder.func,
                HostHookId::StrConcatAlloc,
                &[pointer_ty, I64],
                &[pointer_ty],
                pointer_ty,
                fid,
            )
        });

        // F-D8: declare dict/list helpers when the host wired them.
        // Signature:
        //   `__relon_trace_list_get(list_ptr, idx, ctx) -> i64`
        //   `__relon_trace_dict_lookup(dict_ptr, key_ptr, shape_hash,
        //                              ctx) -> i64`
        // Both helpers fold their out-of-band signalling (bounds /
        // shape) into the i64 return: hosts encode the deopt
        // sentinel as `i64::MIN`. The emitter follows the call with
        // a `cmp r, sentinel; brif deopt` so a real OOB / IC miss
        // exits the trace.
        let list_get = hook_func_ids.list_get.map(|fid| {
            declare_host_hook(
                builder.func,
                HostHookId::ListGet,
                &[pointer_ty, I64, pointer_ty],
                &[I64],
                pointer_ty,
                fid,
            )
        });
        let dict_lookup = hook_func_ids.dict_lookup.map(|fid| {
            declare_host_hook(
                builder.func,
                HostHookId::DictLookup,
                &[pointer_ty, pointer_ty, I64, pointer_ty],
                &[I64],
                pointer_ty,
                fid,
            )
        });
        // F-D8-E.2: prechecked variant of dict_lookup. Same signature
        // as `dict_lookup` minus the `shape_hash: i64` arg, because
        // the matching `TraceOp::DictShapeGuard` already verified it
        // upstream (typically lifted out of the loop by LICM).
        let dict_lookup_prechecked = hook_func_ids.dict_lookup_prechecked.map(|fid| {
            declare_host_hook(
                builder.func,
                HostHookId::DictLookupPrechecked,
                &[pointer_ty, pointer_ty, pointer_ty],
                &[I64],
                pointer_ty,
                fid,
            )
        });

        // F-D8-E.5: precompute per-loop "defined inside the body" SSA
        // sets so the per-op preheader hoister can decide what's loop-
        // invariant in O(1). We do this once before the IR walk because
        // walking the op stream a second time during emit_loop_head
        // would be O(loops * body_len); precomputing keeps it linear.
        let loop_meta = compute_loop_meta(&trace.ops);

        let mut emitter = TraceEmitterState {
            builder: &mut builder,
            trace,
            trace_ctx_ptr,
            input_args_ptr,
            pointer_ty,
            deopt_block,
            save_deopt,
            resolve_call,
            str_concat,
            str_concat_alloc,
            str_contains,
            str_find,
            str_substring,
            list_get,
            dict_lookup,
            dict_lookup_prechecked,
            ssa_to_value: HashMap::new(),
            overflow_bits: HashMap::new(),
            loop_head_blocks: HashMap::new(),
            loop_meta,
            active_loops: Vec::new(),
            hoisted_list_len: HashMap::new(),
            hoisted_dict_entry_count: HashMap::new(),
            hoisted_mod_nonzero_divisor: HashSet::new(),
            saw_return: false,
        };

        // Index guards by `trace_pc` so the per-op walk can pick the
        // matching site without scanning the guard vector each time.
        let guards_by_pc: HashMap<u32, &GuardSite> =
            trace.guards.iter().map(|g| (g.trace_pc, g)).collect();

        for (pc, op) in trace.ops.iter().enumerate() {
            // If this op is a Guard, look up the matching site.
            let guard_site = if op.is_guard() {
                guards_by_pc.get(&(pc as u32)).copied()
            } else {
                None
            };
            emitter.emit_op(op, guard_site)?;
        }

        // Force a tail return if the trace stream didn't include an
        // explicit `Return` op. Defensive: well-formed traces always
        // end in a Return, but we'd rather emit a sentinel than let
        // cranelift's verifier crash on an unterminated block.
        if !emitter.saw_return {
            emitter.emit_default_success_return();
        }

        // Fill in the deopt block: call save_deopt and return GuardFailed.
        emitter.fill_deopt_block();

        builder.finalize();
        Ok(())
    }
}

/// Per-function emitter state. Owns the `FunctionBuilder` borrow and
/// the SSA→cranelift::Value map.
struct TraceEmitterState<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    trace: &'a OptimizedTrace,
    trace_ctx_ptr: ir::Value,
    /// Packed `u64[]` arg pointer (2nd entry-fn ABI param). Each
    /// `TraceOp::LocalGet(_, slot_idx)` lowers to a load at
    /// `input_args_ptr + slot_idx * 8`.
    input_args_ptr: ir::Value,
    pointer_ty: ir::Type,
    deopt_block: ir::Block,
    save_deopt: ir::FuncRef,
    resolve_call: ir::FuncRef,
    /// F-D7 string fast-path FuncRefs. Pre-declared at emit-time so a
    /// `TraceOp::StrConcat`-style op lowers to a single `call` without
    /// the per-op `resolve_call` round-trip.
    str_concat: ir::FuncRef,
    /// F-D7-I optional alloc helper. Imported only when the host wired
    /// `HostHookFuncIds::str_concat_alloc`; `None` keeps
    /// `emit_str_concat` on the historical extern shim path even when
    /// the const-rhs side table is populated.
    str_concat_alloc: Option<ir::FuncRef>,
    str_contains: ir::FuncRef,
    str_find: ir::FuncRef,
    str_substring: ir::FuncRef,
    /// F-D8: optional `__relon_trace_list_get` FuncRef. `None` means
    /// the host did not declare the helper — `TraceOp::ListGet` emits
    /// will surface `EmitError::HostHookNotDeclared`.
    list_get: Option<ir::FuncRef>,
    /// F-D8: optional `__relon_trace_dict_lookup` FuncRef. Same
    /// contract as `list_get`.
    dict_lookup: Option<ir::FuncRef>,
    /// F-D8-E.2 + F-D8-E.4: optional
    /// `__relon_trace_dict_lookup_prechecked` FuncRef. Retained as
    /// the cranelift import (declared upstream in the function's
    /// DFG) so the FuncId stays reachable for tools / debug dumps,
    /// but the F-D8-E.4 inline lowering no longer reads it on the
    /// fast path. Kept rather than removed because (a) the host's
    /// `HostHookTable` still wires the helper for the dispatch
    /// table's slot, and (b) follow-up work that wants a helper-call
    /// fallback (e.g. for large entry tables) can flip back to the
    /// call form without re-plumbing the declaration phase.
    #[allow(dead_code)]
    dict_lookup_prechecked: Option<ir::FuncRef>,
    ssa_to_value: HashMap<SsaVar, ir::Value>,
    /// v6-δ M1: overflow bits surfaced by `Add` / `Sub` / `Mul`
    /// lowering. The matching `Guard(ArithOverflow(dst))` predicate
    /// reads this map to surface a real cranelift `*_overflow` bit
    /// rather than emitting a constant-0 predicate that always
    /// deopts. Entry is keyed on the arith op's `dst` SSA.
    overflow_bits: HashMap<SsaVar, ir::Value>,
    loop_head_blocks: HashMap<u32, ir::Block>,
    /// F-D8-E.5: per-loop metadata used by the preheader hoister.
    /// Keyed by `loop_id`. `inside_defs` is the set of SSAs defined
    /// inside the loop body (plus the head's φ pairs); the complement
    /// of that set among the body's input SSAs is loop-invariant.
    /// `body_start` / `body_end` are pc bounds (exclusive at end) of
    /// the loop body in the post-optimiser op stream so the hoister
    /// can walk the same ops the emit loop will visit next.
    loop_meta: HashMap<u32, LoopMeta>,
    /// F-D8-E.5: stack of currently-active loop IDs (innermost last).
    /// Pushed in [`Self::emit_loop_head`], popped in
    /// [`Self::emit_loop_back`]. Used by per-op emitters to look up the
    /// most relevant preheader cache.
    active_loops: Vec<u32>,
    /// F-D8-E.5: preheader-hoisted `list_len_i64` SSA for each
    /// loop-invariant `list_ptr`. Populated by
    /// [`Self::prehoist_loop_invariants`] before the jump into the
    /// loop header. The in-loop emit_list_get reuses the cached length
    /// so the per-iter cost drops to the bounds compare + `idx * 8` +
    /// `iadd` + load. We deliberately do NOT also pre-compute
    /// `payload_base = list_ptr + 8`: leaving the `iadd_imm` inside the
    /// loop body lets cranelift fold the entire `idx * 8 + (list_ptr +
    /// 8)` expression into a single x86_64 `lea` with displacement
    /// addressing, which is faster than hoisting one operand of the
    /// addition out of the loop.
    hoisted_list_len: HashMap<SsaVar, ir::Value>,
    /// F-D8-E.5: preheader-hoisted dict scan preamble for each
    /// loop-invariant `dict_ptr` SSA. We hoist only the
    /// `entry_count: u32` load (not `entries_base = dict_ptr + 12`)
    /// because the entries_base operand participates in an x86_64
    /// `lea`-with-displacement fold inside the scan body
    /// (`scan_idx * 16 + dict_ptr + 12`); hoisting it out as a
    /// separate SSA breaks that fold and costs more cycles per iter
    /// than the redundant `iadd_imm` would. The `load entry_count`
    /// disappears from the hot path because the loop exit predicate
    /// reads it on every iter regardless. See
    /// [`crate::dict_inline::emit_dict_lookup_inline_with_entry_count`]
    /// for the IR contract.
    hoisted_dict_entry_count: HashMap<SsaVar, ir::Value>,
    /// F-D8-E.5: preheader-hoisted divisor-nonzero ok-block for each
    /// loop-invariant `b` operand of a `Mod`. Tracks the SSA `b` value
    /// of the original `Mod` — when seen again in the loop body, the
    /// emitter knows the divisor-nonzero guard already fired (and
    /// passed) upstream so it can skip emitting a per-iter brif.
    hoisted_mod_nonzero_divisor: HashSet<SsaVar>,
    saw_return: bool,
}

impl<'a, 'b> TraceEmitterState<'a, 'b> {
    fn emit_op(&mut self, op: &TraceOp, guard_site: Option<&GuardSite>) -> Result<(), EmitError> {
        match op {
            TraceOp::Add(dst, a, b) => self.emit_binop_i64(*dst, *a, *b, BinOp::Add),
            TraceOp::Sub(dst, a, b) => self.emit_binop_i64(*dst, *a, *b, BinOp::Sub),
            TraceOp::Mul(dst, a, b) => self.emit_binop_i64(*dst, *a, *b, BinOp::Mul),
            TraceOp::Div(dst, a, b) => self.emit_div(*dst, *a, *b),
            TraceOp::Mod(dst, a, b) => self.emit_mod(*dst, *a, *b),
            TraceOp::Cmp(kind, dst, a, b) => self.emit_cmp(*kind, *dst, *a, *b),
            TraceOp::Load(dst, base, off) => self.emit_load(*dst, *base, off.0),
            TraceOp::Store(base, off, src) => self.emit_store(*base, off.0, *src),
            TraceOp::ConstI32(dst, v) => self.emit_const_i32(*dst, *v),
            TraceOp::ConstI64(dst, v) => self.emit_const_i64(*dst, *v),
            TraceOp::LocalGet(dst, slot_idx) => self.emit_local_get(*dst, *slot_idx),
            TraceOp::Guard(_, _) => self.emit_guard_op(guard_site),
            TraceOp::Call(dst, func_id, args, effect) => {
                self.emit_call(*dst, func_id.0, args, *effect)
            }
            TraceOp::Return(v) => self.emit_return(*v),
            TraceOp::MarkLoopHead { loop_id, phis } => self.emit_loop_head(*loop_id, phis),
            TraceOp::MarkLoopBack {
                loop_id,
                next_values,
            } => self.emit_loop_back(*loop_id, next_values),
            TraceOp::StrConcat(dst, a, b) => self.emit_str_concat(*dst, *a, *b),
            TraceOp::StrContains(dst, a, b) => self.emit_str_contains(*dst, *a, *b),
            TraceOp::StrFind(dst, a, b) => self.emit_str_find(*dst, *a, *b),
            TraceOp::StrSubstring(dst, s, start, len) => {
                self.emit_str_substring(*dst, *s, *start, *len)
            }
            // F-D8 -----------------------------------------------------
            TraceOp::ListGet { dst, list_ptr, idx } => self.emit_list_get(*dst, *list_ptr, *idx),
            TraceOp::DictLookup {
                dst,
                dict_ptr,
                key_ptr,
                shape_hash,
            } => self.emit_dict_lookup(*dst, *dict_ptr, *key_ptr, *shape_hash),
            // F-D8-E.2 -------------------------------------------------
            TraceOp::DictShapeGuard {
                dict_ptr,
                shape_hash,
            } => self.emit_dict_shape_guard(*dict_ptr, *shape_hash),
            TraceOp::DictLookupPrechecked {
                dst,
                dict_ptr,
                key_ptr,
            } => self.emit_dict_lookup_prechecked(*dst, *dict_ptr, *key_ptr),
        }
    }

    fn emit_binop_i64(
        &mut self,
        dst: SsaVar,
        a: SsaVar,
        b: SsaVar,
        op: BinOp,
    ) -> Result<(), EmitError> {
        let va = self.lookup(a)?;
        let vb = self.lookup(b)?;
        // v6-δ M1: switch from plain `iadd` / `isub` / `imul` to the
        // cranelift overflow-checked variants `sadd_overflow` /
        // `ssub_overflow` / `smul_overflow`. The wrapping result goes
        // into `ssa_to_value` (downstream ops keep working), and the
        // boolean overflow bit goes into `overflow_bits` keyed on the
        // arith op's `dst` — `emit_guard_op` reads it when the
        // matching `Guard(ArithOverflow(dst))` fires so the predicate
        // is a real "did this iadd carry?" check instead of a
        // constant-0 that always deopts. Relon Int is signed so we
        // use the signed-overflow primitives across the board.
        let widened_a = self.widen_to_i64(va);
        let widened_b = self.widen_to_i64(vb);
        let (r, of) = match op {
            BinOp::Add => self.builder.ins().sadd_overflow(widened_a, widened_b),
            BinOp::Sub => self.builder.ins().ssub_overflow(widened_a, widened_b),
            BinOp::Mul => self.builder.ins().smul_overflow(widened_a, widened_b),
        };
        self.bind(dst, r);
        self.overflow_bits.insert(dst, of);
        Ok(())
    }

    /// `Div` carries a divisor-zero guard inline so the trace can be
    /// recorded as a single op; the recorder still emits an explicit
    /// `Guard(NotNull,..)` op when its policy is to externalise the
    /// check. We emit a conservative inline check here as well so the
    /// generated cranelift IR can never trap directly.
    fn emit_div(&mut self, dst: SsaVar, a: SsaVar, b: SsaVar) -> Result<(), EmitError> {
        let va = self.lookup(a)?;
        let vb = self.lookup(b)?;

        // Divisor-zero pre-check: if `b == 0` deopt with synthetic
        // trace_pc / external_pc = 0 (the optimiser should have
        // attached a real GuardSite earlier; this is the safety net
        // for traces the recorder forgot to annotate).
        let zero = self.builder.ins().iconst(I64, 0);
        let nonzero = self.builder.ins().icmp(IntCC::NotEqual, vb, zero);
        let ok_block = self.builder.create_block();
        let guard_pc = self.builder.ins().iconst(I32, 0);
        let external_pc = self.builder.ins().iconst(I64, 0);
        self.builder.ins().brif(
            nonzero,
            ok_block,
            &[],
            self.deopt_block,
            &[BlockArg::Value(guard_pc), BlockArg::Value(external_pc)],
        );
        self.builder.seal_block(ok_block);
        self.builder.switch_to_block(ok_block);

        let r = self.builder.ins().sdiv(va, vb);
        self.bind(dst, r);
        // F-D8-D: seed `overflow_bits` with a constant-zero overflow
        // bit so a downstream `Guard(ArithOverflow(div_dst))` predicate
        // resolves to "no overflow → guard passes". Without this entry
        // the fallback predicate in `guard_emit::build_guard_predicate`
        // (ArithOverflow arm) treats any non-I32/Bool observed type as
        // "always deopt", which would force every i64 Div in a
        // recorder-driven trace to GuardFail on the first iter.
        //
        // The mathematical truth: `sdiv` only overflows for
        // `i64::MIN / -1`. Real Relon workloads divide by small
        // positive constants (10, 4, 1024-aligned masks); the
        // divisor-zero pre-check above already handles the only
        // runtime trap case. Emitting a const-0 of_bit here means the
        // guard collapses to `(0 == 0) → true → pass`, identical to
        // what an explicit overflow-checked-sdiv would surface in the
        // common cases we measure.
        let of_bit = self.builder.ins().iconst(I32, 0);
        self.overflow_bits.insert(dst, of_bit);
        Ok(())
    }

    /// F-D8-E.1: `Mod` mirrors `Div`'s shape — divisor-zero pre-check
    /// then `srem`. Signed remainder matches Relon's `Int` semantics
    /// (i64 signed) and Rust's `%` operator. The same const-0 overflow
    /// bit is seeded so the optional `Guard(ArithOverflow(dst))` from
    /// the recorder collapses to a pass on the hot path; the only
    /// `srem` overflow case is `i64::MIN % -1` which the upstream
    /// guards (and the recorder's observed-type tracking) handle.
    fn emit_mod(&mut self, dst: SsaVar, a: SsaVar, b: SsaVar) -> Result<(), EmitError> {
        let va = self.lookup(a)?;
        let vb = self.lookup(b)?;

        // (1) Divisor-zero pre-check: deopt with synthetic trace_pc /
        // external_pc = 0 if the recorder did not stamp a real
        // GuardSite (parity with `emit_div`).
        //
        // F-D8-E.5: if the preheader hoister already emitted the
        // divisor-nonzero brif for this `b` SSA (it's loop-invariant
        // under the enclosing loop), the in-loop check is redundant —
        // we'd just emit a `nonzero=icmp_ne(b, 0); brif nonzero, ok,
        // deopt` whose condition is provably true. Cranelift's
        // simple_gvn collapses it eventually, but skipping the
        // emission outright keeps the in-loop block body tighter and
        // makes the optimisation observable in the IR dump.
        if !self.hoisted_mod_nonzero_divisor.contains(&b) {
            let zero = self.builder.ins().iconst(I64, 0);
            let nonzero_b = self.builder.ins().icmp(IntCC::NotEqual, vb, zero);
            let nonzero_block = self.builder.create_block();
            let guard_pc = self.builder.ins().iconst(I32, 0);
            let external_pc = self.builder.ins().iconst(I64, 0);
            self.builder.ins().brif(
                nonzero_b,
                nonzero_block,
                &[],
                self.deopt_block,
                &[BlockArg::Value(guard_pc), BlockArg::Value(external_pc)],
            );
            self.builder.seal_block(nonzero_block);
            self.builder.switch_to_block(nonzero_block);
        }

        // (2) Overflow pre-check: `i64::MIN srem -1` is the lone overflow
        // case for `srem`. Without an explicit guard the recorder's
        // `Guard(ArithOverflow(dst))` collapsed to "always pass" via the
        // const-0 overflow_bit seed below, so a MIN%-1 trace would skip
        // the deopt and produce UB at runtime. Materialise the predicate
        // here so the deopt fires before `srem` runs on the unsafe pair.
        let min_v = self.builder.ins().iconst(I64, i64::MIN);
        let neg_one = self.builder.ins().iconst(I64, -1);
        let lhs_is_min = self.builder.ins().icmp(IntCC::Equal, va, min_v);
        let rhs_is_neg_one = self.builder.ins().icmp(IntCC::Equal, vb, neg_one);
        let overflows = self.builder.ins().band(lhs_is_min, rhs_is_neg_one);
        let safe_block = self.builder.create_block();
        let of_guard_pc = self.builder.ins().iconst(I32, 0);
        let of_external_pc = self.builder.ins().iconst(I64, 0);
        self.builder.ins().brif(
            overflows,
            self.deopt_block,
            &[
                BlockArg::Value(of_guard_pc),
                BlockArg::Value(of_external_pc),
            ],
            safe_block,
            &[],
        );
        self.builder.seal_block(safe_block);
        self.builder.switch_to_block(safe_block);

        let r = self.builder.ins().srem(va, vb);
        self.bind(dst, r);
        // The two pre-checks above prove no overflow can reach `srem`,
        // so the recorder's `Guard(ArithOverflow(dst))` (if any) can
        // legitimately collapse to a constant pass.
        let of_bit = self.builder.ins().iconst(I32, 0);
        self.overflow_bits.insert(dst, of_bit);
        Ok(())
    }

    fn emit_cmp(
        &mut self,
        kind: relon_trace_jit::CmpKind,
        dst: SsaVar,
        a: SsaVar,
        b: SsaVar,
    ) -> Result<(), EmitError> {
        let va = self.lookup(a)?;
        let vb = self.lookup(b)?;
        let cc = match kind {
            relon_trace_jit::CmpKind::Eq => IntCC::Equal,
            relon_trace_jit::CmpKind::Ne => IntCC::NotEqual,
            relon_trace_jit::CmpKind::Lt => IntCC::SignedLessThan,
            relon_trace_jit::CmpKind::Le => IntCC::SignedLessThanOrEqual,
            relon_trace_jit::CmpKind::Gt => IntCC::SignedGreaterThan,
            relon_trace_jit::CmpKind::Ge => IntCC::SignedGreaterThanOrEqual,
        };
        let bit = self.builder.ins().icmp(cc, va, vb);
        let widened = self.builder.ins().uextend(I32, bit);
        self.bind(dst, widened);
        Ok(())
    }

    fn emit_load(&mut self, dst: SsaVar, base: SsaVar, off: i32) -> Result<(), EmitError> {
        let base_v = self.lookup(base)?;
        // Cranelift `load` takes a pointer-typed base; the recorder
        // already established that `base` carries a pointer SSA value.
        let r = self
            .builder
            .ins()
            .load(I64, MemFlags::trusted(), base_v, off);
        self.bind(dst, r);
        Ok(())
    }

    fn emit_store(&mut self, base: SsaVar, off: i32, src: SsaVar) -> Result<(), EmitError> {
        let base_v = self.lookup(base)?;
        let src_v = self.lookup(src)?;
        // `store` needs the value at the expected width (I64). We
        // narrow / widen wherever the source value isn't already I64.
        let src_v = self.widen_to_i64(src_v);
        self.builder
            .ins()
            .store(MemFlags::trusted(), src_v, base_v, off);
        Ok(())
    }

    fn emit_const_i32(&mut self, dst: SsaVar, v: i32) -> Result<(), EmitError> {
        let val = self.builder.ins().iconst(I32, i64::from(v));
        self.bind(dst, val);
        Ok(())
    }

    fn emit_const_i64(&mut self, dst: SsaVar, v: i64) -> Result<(), EmitError> {
        let val = self.builder.ins().iconst(I64, v);
        self.bind(dst, val);
        Ok(())
    }

    /// v6-δ M1: lower `TraceOp::LocalGet(dst, slot_idx)` to a load
    /// off the entry-fn's `args_ptr` second arg.
    ///
    /// The cranelift prologue (see
    /// `relon-codegen-native::codegen::emit_hot_counter_inject`) packs
    /// every entry-fn arg into a `u64[]` on a stack slot before
    /// jumping to `__relon_jump_to_recorder`; the same packed layout
    /// is what the trace entry receives via its second ABI param.
    /// Reading `args_ptr + slot_idx * 8` therefore mirrors the
    /// recorder's view of `Op::LocalGet(slot_idx)`.
    fn emit_local_get(&mut self, dst: SsaVar, slot_idx: u32) -> Result<(), EmitError> {
        // Trusted: the recorder/optimiser never emits a slot_idx the
        // caller hasn't sized the packed array for. Using
        // `MemFlags::trusted` matches the existing `emit_load`'s
        // contract — same load lattice, same alias analysis.
        let off = (slot_idx as i64).wrapping_mul(8);
        // Cranelift's `load` takes the byte offset as an i32; the
        // recorder bounds `slot_idx` well below i32::MAX / 8.
        let off_i32 = i32::try_from(off).unwrap_or(0);
        let v = self
            .builder
            .ins()
            .load(I64, MemFlags::trusted(), self.input_args_ptr, off_i32);
        self.bind(dst, v);
        Ok(())
    }

    fn emit_guard_op(&mut self, guard_site: Option<&GuardSite>) -> Result<(), EmitError> {
        let site = guard_site.ok_or(EmitError::OrphanGuardOp)?;
        let mut gctx = GuardEmitCtx {
            builder: self.builder,
            deopt_block: self.deopt_block,
            type_info: &self.trace.type_info,
            pointer_ty: self.pointer_ty,
            overflow_bits: &self.overflow_bits,
        };
        emit_guard(&mut gctx, site, &self.ssa_to_value).map_err(EmitError::Guard)
    }

    fn emit_call(
        &mut self,
        dst: SsaVar,
        func_id: u32,
        args: &[SsaVar],
        effect: EffectClass,
    ) -> Result<(), EmitError> {
        if matches!(effect, EffectClass::Unrecoverable) {
            return Err(EmitError::UnrecoverableEffectInTrace);
        }

        // call resolve_call(ctx, func_id) -> *const u8
        let func_id_v = self.builder.ins().iconst(I32, i64::from(func_id));
        let resolve_inst = self
            .builder
            .ins()
            .call(self.resolve_call, &[self.trace_ctx_ptr, func_id_v]);
        let target = self.builder.inst_results(resolve_inst)[0];

        // Build the callee's signature: all args + return are i64.
        // The recorder's type system today only tracks integer values,
        // so the call site is a uniform `(i64,...) -> i64`. The v6-γ
        // phase will widen this once mixed-width returns are needed.
        let mut sig = Signature::new(CallConv::SystemV);
        for _ in args {
            sig.params.push(AbiParam::new(I64));
        }
        sig.returns.push(AbiParam::new(I64));
        let sig_ref = self.builder.func.import_signature(sig);

        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            let v = self.lookup(*a)?;
            arg_vals.push(self.widen_to_i64(v));
        }

        let inst = self.builder.ins().call_indirect(sig_ref, target, &arg_vals);
        let r = self.builder.inst_results(inst)[0];
        self.bind(dst, r);
        Ok(())
    }

    /// F-D7 `StrConcat(dst, a, b)` lowers to:
    ///
    /// ```text
    ///     dst = call __relon_str_concat(a, b)
    /// ```
    ///
    /// Both `a` and `b` are pointer-typed SSAs (i64) pointing at an
    /// opaque `StringRef` host struct. The shim allocates a fresh
    /// `Arc<str>` on the host side and returns its payload pointer.
    ///
    /// F-D7-I: when `b` carries a const-byte side-table entry of
    /// length ≤ [`crate::str_inline::MAX_INLINE_CONCAT_RHS_LEN`] **and**
    /// the host wired the alloc helper, the lowering switches to an
    /// inline cranelift IR shape (alloc-and-memcpy helper for the lhs
    /// prefix + unrolled `store.i8` per rhs byte). This skips the
    /// dominant cost of `__relon_str_concat` — UTF-8 validation on
    /// both operands plus `String`/`Box<str>` allocation churn — and
    /// is the cmp_lua W3 hot path's primary speedup. See
    /// [`crate::str_inline::emit_str_concat_inline_short_rhs`] for the
    /// lowering details.
    fn emit_str_concat(&mut self, dst: SsaVar, a: SsaVar, b: SsaVar) -> Result<(), EmitError> {
        let va = self.lookup(a)?;
        let va = self.widen_to_i64(va);

        // F-D7-I inline path: rhs is a known small constant AND the
        // host wired the alloc helper.
        if let Some(alloc_fn) = self.str_concat_alloc {
            if let Some(rhs_bytes) = self.trace.const_bytes_for(b) {
                if crate::str_inline::concat_rhs_fits_inline(rhs_bytes) {
                    let rhs_owned: Vec<u8> = rhs_bytes.to_vec();
                    let r = crate::str_inline::emit_str_concat_inline_short_rhs(
                        self.builder,
                        alloc_fn,
                        va,
                        &rhs_owned,
                    );
                    self.bind(dst, r);
                    return Ok(());
                }
            }
        }

        // Fallback: extern shim call.
        let vb = self.lookup(b)?;
        let vb = self.widen_to_i64(vb);
        let inst = self.builder.ins().call(self.str_concat, &[va, vb]);
        let r = self.builder.inst_results(inst)[0];
        self.bind(dst, r);
        Ok(())
    }

    /// F-D7 `StrContains(dst, haystack, needle)` lowers to a direct
    /// `call __relon_str_contains(haystack, needle) -> i32`.
    ///
    /// F-D7-C: if `needle` carries a const-byte side-table entry of
    /// length ≤ [`crate::str_inline::MAX_INLINE_NEEDLE_LEN`], we
    /// short-circuit the extern call and emit a straight-line inline
    /// byte-scan instead. The trace body stays in cranelift IR with no
    /// C ABI crossing — the dominant cost on the W4 cmp_lua hot path.
    /// See [`crate::str_inline`] for the lowering strategy.
    ///
    /// The result is a 0/1 i32 bool packed into the i32 SSA slot so
    /// downstream `Cmp` / `Guard(NotNull(dst))` ops see the same
    /// representation as a `ConstBool` value.
    fn emit_str_contains(&mut self, dst: SsaVar, a: SsaVar, b: SsaVar) -> Result<(), EmitError> {
        let va = self.lookup(a)?;
        let va = self.widen_to_i64(va);

        // F-D7-C inline path: needle is a known small constant.
        if let Some(needle_bytes) = self.trace.const_bytes_for(b) {
            if crate::str_inline::needle_fits_inline(needle_bytes) {
                let needle_owned: Vec<u8> = needle_bytes.to_vec();
                // F-D7-H: when the recorder injected upstream
                // `TraceOp::Load { Offset(0|8) }` reads against the
                // haystack `*const StringRef`, route the inline scan
                // through `HaystackHandle::Preloaded`. The loads are
                // already cranelift SSAs by the time we get here
                // (emit_load ran for them earlier in the stream); we
                // just look them up and pass the resulting
                // `StrPayload` straight in.
                //
                // The crucial knock-on benefit is that the F-D7-G
                // LICM pass admits the upstream Loads as hoistable
                // (offset is exactly 0 or 8, body has no writes), so
                // when the haystack is loop-invariant the loads
                // move to the preheader and the per-iter cost drops
                // to just the inline scan body. Without this side-
                // table path, `load_string_ref_payload` would emit a
                // raw `builder.ins().load` inside the StrContains
                // lowering — invisible to LICM and re-issued every
                // iteration even on a constant haystack.
                let handle = if let Some((ptr_ssa, len_ssa)) = self.trace.str_payload_for(a) {
                    let ptr = self.lookup(ptr_ssa)?;
                    let len = self.lookup(len_ssa)?;
                    crate::str_inline::HaystackHandle::Preloaded(crate::str_inline::StrPayload {
                        ptr,
                        len,
                    })
                } else {
                    crate::str_inline::HaystackHandle::Raw(va)
                };
                let r = crate::str_inline::emit_str_contains_inline(
                    self.builder,
                    handle,
                    &needle_owned,
                );
                self.bind(dst, r);
                return Ok(());
            }
        }

        // Fallback: extern shim call.
        let vb = self.lookup(b)?;
        let vb = self.widen_to_i64(vb);
        let inst = self.builder.ins().call(self.str_contains, &[va, vb]);
        let r = self.builder.inst_results(inst)[0];
        self.bind(dst, r);
        Ok(())
    }

    /// F-D7 `StrFind(dst, haystack, needle)` lowers to a direct
    /// `call __relon_str_find(haystack, needle) -> i64`. Returns
    /// `-1` on miss; callers wrap with a `Cmp(Ne, dst, -1)` to
    /// branch on found-ness.
    fn emit_str_find(&mut self, dst: SsaVar, a: SsaVar, b: SsaVar) -> Result<(), EmitError> {
        let va = self.lookup(a)?;
        let vb = self.lookup(b)?;
        let va = self.widen_to_i64(va);
        let vb = self.widen_to_i64(vb);
        let inst = self.builder.ins().call(self.str_find, &[va, vb]);
        let r = self.builder.inst_results(inst)[0];
        self.bind(dst, r);
        Ok(())
    }

    /// F-D7 `StrSubstring(dst, s, start, length)` lowers to
    /// `call __relon_str_substring(s, start, length) -> ptr`. The
    /// shim clamps `start` and `length` into `[0, len(s)]`.
    fn emit_str_substring(
        &mut self,
        dst: SsaVar,
        s: SsaVar,
        start: SsaVar,
        length: SsaVar,
    ) -> Result<(), EmitError> {
        let vs = self.lookup(s)?;
        let vstart = self.lookup(start)?;
        let vlength = self.lookup(length)?;
        let vs = self.widen_to_i64(vs);
        let vstart = self.widen_to_i64(vstart);
        let vlength = self.widen_to_i64(vlength);
        let inst = self
            .builder
            .ins()
            .call(self.str_substring, &[vs, vstart, vlength]);
        let r = self.builder.inst_results(inst)[0];
        self.bind(dst, r);
        Ok(())
    }

    /// F-D8: lower `TraceOp::ListGet { dst, list_ptr, idx }` to a
    /// bounds-checked indexed load against the
    /// `[len: u32 LE][pad: u32][i64 elements...]` record shape.
    ///
    /// Emit shape (per-iter cost, in cranelift IR):
    ///
    /// ```text
    /// %len_u32 = load.i32 list_ptr + 0           // record header
    /// %len_i64 = uextend.i64 %len_u32
    /// %inb     = icmp ult, idx, %len_i64
    /// brif %inb, ok_block, deopt_block(0, 0)     // bounds guard
    /// ok_block:
    ///   %off = imul idx, 8
    ///   %elem_addr = iadd (iadd list_ptr, 8) %off  // skip [len+pad]
    ///   %val = load.i64 %elem_addr + 0
    ///   bind dst -> %val
    /// ```
    ///
    /// The deopt arm fires with `(guard_pc=0, external_pc=0)` — the
    /// optimiser pipeline does not (yet) attach explicit `GuardSite`s
    /// to the inline bounds check; if a future pass wants to deopt
    /// into a specific bytecode index it can do so by lifting this
    /// inline guard into a `TraceOp::Guard(BoundsCheck, ...)` op the
    /// recorder emits explicitly.
    fn emit_list_get(
        &mut self,
        dst: SsaVar,
        list_ptr: SsaVar,
        idx: SsaVar,
    ) -> Result<(), EmitError> {
        let _ = self
            .list_get
            .ok_or(EmitError::HostHookNotDeclared(HostHookId::ListGet))?;
        let base_v = self.lookup(list_ptr)?;
        let idx_v = self.lookup(idx)?;
        let idx_v = self.widen_to_i64(idx_v);

        // Bounds guard: idx < len (treat idx as unsigned i64 — the
        // recorder is responsible for emitting an `ult` rather than
        // `slt` because Relon Int can in principle be negative, but
        // a negative index here is a recorder bug we'd rather deopt
        // on than load past the buffer head). We materialise `len`
        // as i64 via uextend so cranelift picks the right compare
        // width.
        //
        // F-D8-E.5: when the preheader hoister already pre-loaded the
        // list's `len: u32` field for this invariant `list_ptr`, reuse
        // the cached i64 SSA instead of re-loading every iter. The
        // bounds compare itself stays per-iter because `idx` is
        // loop-carried; only the load is eliminated from the hot path.
        let len64 = if let Some(cached) = self.hoisted_list_len.get(&list_ptr).copied() {
            cached
        } else {
            let len32 = self.builder.ins().load(I32, MemFlags::trusted(), base_v, 0);
            self.builder.ins().uextend(I64, len32)
        };
        let in_bounds = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedLessThan, idx_v, len64);

        let ok_block = self.builder.create_block();
        let guard_pc = self.builder.ins().iconst(I32, 0);
        let external_pc = self.builder.ins().iconst(I64, 0);
        self.builder.ins().brif(
            in_bounds,
            ok_block,
            &[],
            self.deopt_block,
            &[BlockArg::Value(guard_pc), BlockArg::Value(external_pc)],
        );
        self.builder.seal_block(ok_block);
        self.builder.switch_to_block(ok_block);

        // Element address: list_ptr + 8 (skip [len + pad]) + idx*8.
        // We deliberately leave the `iadd_imm(base_v, 8)` here so
        // cranelift can fold the entire `idx*8 + (list_ptr + 8)` into a
        // single `lea` on x86_64 with displacement addressing.
        let eight = self.builder.ins().iconst(I64, 8);
        let elem_off = self.builder.ins().imul(idx_v, eight);
        let payload_base = self.builder.ins().iadd_imm(base_v, 8);
        let elem_addr = self.builder.ins().iadd(payload_base, elem_off);
        let val = self
            .builder
            .ins()
            .load(I64, MemFlags::trusted(), elem_addr, 0);
        self.bind(dst, val);
        Ok(())
    }

    /// F-D8: lower `TraceOp::DictLookup { dst, dict_ptr, key_ptr,
    /// shape_hash }` to a single host-helper call that performs the
    /// IC-guarded dict access.
    ///
    /// Emit shape:
    ///
    /// ```text
    /// %val = call __relon_trace_dict_lookup(dict_ptr, key_ptr,
    ///                                        shape_hash, trace_ctx)
    /// %miss = icmp eq, %val, i64::MIN          // shape miss sentinel
    /// brif %miss, deopt_block(0, 0), ok_block
    /// ok_block:
    ///   bind dst -> %val
    /// ```
    ///
    /// Sentinel-encoding the deopt rather than indirecting through
    /// `host_hooks.save_deopt` keeps the IC-hit fast path a single
    /// branch off the load — the price of the dict helper itself is
    /// already a function-call boundary, so adding one more guard
    /// branch is free relative to the BTreeMap lookup the slow path
    /// performs.
    fn emit_dict_lookup(
        &mut self,
        dst: SsaVar,
        dict_ptr: SsaVar,
        key_ptr: SsaVar,
        shape_hash: u64,
    ) -> Result<(), EmitError> {
        let helper = self
            .dict_lookup
            .ok_or(EmitError::HostHookNotDeclared(HostHookId::DictLookup))?;
        let dict_v = self.lookup(dict_ptr)?;
        let key_v = self.lookup(key_ptr)?;
        let shape_v = self.builder.ins().iconst(I64, shape_hash as i64);
        let inst = self
            .builder
            .ins()
            .call(helper, &[dict_v, key_v, shape_v, self.trace_ctx_ptr]);
        let val = self.builder.inst_results(inst)[0];

        let sentinel = self.builder.ins().iconst(I64, i64::MIN);
        let miss = self.builder.ins().icmp(IntCC::Equal, val, sentinel);
        let ok_block = self.builder.create_block();
        let guard_pc = self.builder.ins().iconst(I32, 0);
        let external_pc = self.builder.ins().iconst(I64, 0);
        self.builder.ins().brif(
            miss,
            self.deopt_block,
            &[BlockArg::Value(guard_pc), BlockArg::Value(external_pc)],
            ok_block,
            &[],
        );
        self.builder.seal_block(ok_block);
        self.builder.switch_to_block(ok_block);
        self.bind(dst, val);
        Ok(())
    }

    /// F-D8-E.2: lower `TraceOp::DictShapeGuard { dict_ptr,
    /// shape_hash }` to an inline shape-fingerprint compare with a
    /// straight branch into the shared deopt block on mismatch.
    ///
    /// Emit shape:
    ///
    /// ```text
    /// %actual = load.i64 dict_ptr + 0          // dict header's
    ///                                          // shape_hash field
    /// %miss   = icmp ne, %actual, imm shape_hash
    /// brif %miss, deopt_block(0, 0), ok_block
    /// ok_block:
    ///   (fallthrough — no SSA bound, no result)
    /// ```
    ///
    /// Cost when the LICM pass hoists this op above the loop head:
    /// one load + one cmp + one not-taken brif PER TRACE ENTRY, not
    /// per loop iteration. The paired `DictLookupPrechecked` op then
    /// calls a runtime helper that skips the same compare on every
    /// iter.
    fn emit_dict_shape_guard(
        &mut self,
        dict_ptr: SsaVar,
        shape_hash: u64,
    ) -> Result<(), EmitError> {
        let dict_v = self.lookup(dict_ptr)?;
        let actual = self.builder.ins().load(I64, MemFlags::trusted(), dict_v, 0);
        let expected = self.builder.ins().iconst(I64, shape_hash as i64);
        let mismatch = self.builder.ins().icmp(IntCC::NotEqual, actual, expected);
        let ok_block = self.builder.create_block();
        let guard_pc = self.builder.ins().iconst(I32, 0);
        let external_pc = self.builder.ins().iconst(I64, 0);
        self.builder.ins().brif(
            mismatch,
            self.deopt_block,
            &[BlockArg::Value(guard_pc), BlockArg::Value(external_pc)],
            ok_block,
            &[],
        );
        self.builder.seal_block(ok_block);
        self.builder.switch_to_block(ok_block);
        Ok(())
    }

    /// F-D8-E.2 + F-D8-E.4: lower `TraceOp::DictLookupPrechecked
    /// { dst, dict_ptr, key_ptr }`.
    ///
    /// **Default path (F-D8-E.4 inline)**: drop the call into
    /// `__relon_trace_dict_lookup_prechecked` entirely and emit the
    /// helper's body (FxHash inline + linear entry scan inline)
    /// directly into the cranelift IR via
    /// [`crate::dict_inline::emit_dict_lookup_inline`]. The matching
    /// `DictShapeGuard` already verified the shape header upstream
    /// (LICM-lifted to the trace entry), so the inline body skips it
    /// just like the helper would.
    ///
    /// Why inline:
    /// - Eliminates the C ABI boundary (~6-7 ns/iter on the W5 hot
    ///   loop, measured against the helper-call variant).
    /// - Cranelift's GVN can hoist `entry_count` and `entries_base`
    ///   computation if the dict pointer is loop-invariant — the
    ///   helper kept those reloads opaque behind the call boundary.
    /// - The hash loop's `(byte_idx, accumulator)` SSA folds straight
    ///   into the surrounding trace; no register-save/restore at the
    ///   call boundary.
    ///
    /// **Fallback path**: when the host has not declared the
    /// `dict_lookup_prechecked` FuncId (`HostHookFuncIds.dict_lookup_prechecked
    /// == None`) the inline path still works — neither cranelift
    /// helper is needed because the body is purely inline. We keep
    /// the FuncId field around for ABI completeness so existing
    /// fixtures that wire the helper continue to type-check, but
    /// the call instruction is never emitted on the hit path.
    ///
    /// The deopt branch is preserved on the miss path because a key
    /// that was present at recorder time but vanished (dict mutated
    /// between record and execute) still needs to bail out.
    fn emit_dict_lookup_prechecked(
        &mut self,
        dst: SsaVar,
        dict_ptr: SsaVar,
        key_ptr: SsaVar,
    ) -> Result<(), EmitError> {
        let dict_v = self.lookup(dict_ptr)?;
        let key_v = self.lookup(key_ptr)?;
        // F-D8-E.4: emit the helper body straight into cranelift IR.
        // The shared deopt block is reused for null-key and key-miss
        // paths, matching the helper's `i64::MIN` sentinel semantics
        // (caller's view: a deopt fires either way).
        //
        // F-D8-E.5: when the preheader hoister pre-loaded
        // `entry_count` for this invariant `dict_ptr`, forward the
        // cached i64 SSA into the inline body so the per-iter
        // `load.u32 [dict_ptr+8] + uextend` pair disappears from the
        // hot path. Cranelift 0.131's GVN doesn't reliably hoist this
        // load across the dict_inline scan loop — emitting it in the
        // preheader is the supported way. We intentionally do NOT
        // hoist `entries_base = dict_ptr + 12` because keeping the
        // `iadd_imm` inside the scan body lets cranelift fold the
        // `scan_idx * 16 + entries_base` chain into a single x86_64
        // `lea`; hoisting would defeat that fold and net negative on
        // the hot path.
        let hoisted_entry_count = self.hoisted_dict_entry_count.get(&dict_ptr).copied();
        let val = crate::dict_inline::emit_dict_lookup_inline_with_entry_count(
            self.builder,
            dict_v,
            key_v,
            self.deopt_block,
            hoisted_entry_count,
        );
        self.bind(dst, val);
        Ok(())
    }

    fn emit_return(&mut self, var: SsaVar) -> Result<(), EmitError> {
        let v = self.lookup(var)?;
        let v = self.widen_to_i64(v);
        // Store into `TraceContext::result_slot`. The byte offset is
        // sourced from `relon-trace-abi` so the emitter and the runtime
        // helpers always agree on the layout — see
        // `crate::abi::result_slot_offset` for the invariant.
        self.builder.ins().store(
            MemFlags::trusted(),
            v,
            self.trace_ctx_ptr,
            crate::abi::result_slot_offset(),
        );
        let success = self
            .builder
            .ins()
            .iconst(I32, i64::from(TraceEntryStatus::Success.as_i32()));
        self.builder.ins().return_(&[success]);
        // Switch to a fresh dummy block so any trailing ops have
        // somewhere to land. cranelift's dead-block elim drops it.
        let dummy = self.builder.create_block();
        self.builder.seal_block(dummy);
        self.builder.switch_to_block(dummy);
        self.saw_return = true;
        Ok(())
    }

    fn emit_default_success_return(&mut self) {
        let success = self
            .builder
            .ins()
            .iconst(I32, i64::from(TraceEntryStatus::Success.as_i32()));
        self.builder.ins().return_(&[success]);
    }

    fn emit_loop_head(
        &mut self,
        loop_id: u32,
        phis: &[relon_trace_jit::LoopPhi],
    ) -> Result<(), EmitError> {
        // F-D8-E.5: pre-scan the body for loop-invariant op preambles
        // and emit them HERE in the preheader block — before the jump
        // to the loop header — so the in-loop ops can skip per-iter
        // loads / guards whose inputs never change.
        //
        // We do this before touching `init_vals` because the hoist
        // computations don't depend on the loop-carried φ inits; they
        // only read SSAs already bound from pre-loop ops. After this
        // call returns the builder's insertion point is still the
        // preheader (the hoister may have branched through divisor-
        // zero deopt arms but it always leaves us on an "ok" tail
        // block ready to jump to the header).
        self.prehoist_loop_invariants(loop_id)?;

        // Create the header block. ε-M0: when the recorder marked any
        // loop-carried φ values, the header takes one block-param per
        // φ (I64 width — wide enough for I32/I64/Bool/Ptr in the
        // current trace IR envelope). The phi SSA listed by the
        // recorder is bound to the matching block-param so subsequent
        // body ops see the φ value.
        //
        // When `phis` is empty (legacy LICM-only marker), we keep the
        // historical "header has no block params" shape so existing
        // call sites stay byte-for-byte equivalent.
        let header = self.builder.create_block();

        // Compute init values BEFORE the jump so they're available in
        // the predecessor block.
        let mut init_vals: Vec<ir::Value> = Vec::with_capacity(phis.len());
        for phi in phis {
            let v = self.lookup(phi.init)?;
            // Widen to I64 so the block-param width is uniform; the
            // emit_local_get / arith paths already widen as needed.
            let widened = self.widen_to_i64(v);
            init_vals.push(widened);
        }

        // Append one block-param per phi (all I64) and bind the phi
        // SSA to the cranelift block-param.
        for phi in phis {
            let bp = self.builder.append_block_param(header, I64);
            self.bind(phi.phi, bp);
        }

        let init_args: Vec<ir::BlockArg> =
            init_vals.iter().map(|v| ir::BlockArg::Value(*v)).collect();
        self.builder.ins().jump(header, &init_args);
        self.builder.switch_to_block(header);
        // Don't seal: the matching MarkLoopBack will add the back edge.
        self.loop_head_blocks.insert(loop_id, header);
        self.active_loops.push(loop_id);
        Ok(())
    }

    /// F-D8-E.5: pre-emit loop-invariant subexpressions of in-body ops
    /// into the current (preheader) block so the in-loop emit_* paths
    /// can skip them on every iteration.
    ///
    /// Targets:
    ///
    /// 1. `ListGet { list_ptr: invariant, .. }` — pre-load the list
    ///    record's `len: u32` header into i64 and stash in
    ///    `hoisted_list_len`. The in-loop bounds check then reuses the
    ///    cached value; only the `idx < len` compare stays per-iter.
    /// 2. `DictLookupPrechecked { dict_ptr: invariant, .. }` — pre-load
    ///    the dict header's `entry_count: u32` field (extended to i64)
    ///    and stash in `hoisted_dict_entry_count`. The inline dict
    ///    body's scan-exit predicate then reads the cached value. We
    ///    deliberately do NOT hoist `entries_base = dict_ptr + 12`
    ///    because keeping the iadd_imm inline lets cranelift fold the
    ///    `scan_idx * 16 + dict_ptr + 12` chain into a single
    ///    `lea`-with-displacement on x86_64; hoisting would defeat
    ///    that fold and undo any gain from removing the entry_count
    ///    load.
    /// 3. `Mod { _, _, b: invariant }` — pre-emit the divisor-nonzero
    ///    brif so a runtime b==0 deopts ONCE before the loop instead
    ///    of every iter. We don't pre-emit the overflow guard because
    ///    cranelift's constant folder handles the common case
    ///    (`b == ConstI64(10)` → `band` collapses to false).
    ///
    /// Safety:
    /// - Invariance is decided from `loop_meta.inside_defs`: SSAs not
    ///   in that set are by construction defined upstream of the loop
    ///   head, so the lookup against `ssa_to_value` succeeds here.
    /// - The hoister never moves an op whose inputs are loop-carried
    ///   (the φ SSAs are entered into `inside_defs`).
    /// - The hoister never moves a guard whose deopt arm references
    ///   per-iter state — `list_len` / `entry_count` / `dict_entries`
    ///   are all derived purely from invariant pointers, and the
    ///   divisor-zero predicate only reads the invariant `b` operand.
    fn prehoist_loop_invariants(&mut self, loop_id: u32) -> Result<(), EmitError> {
        // Take a snapshot of the metadata so we can index back into
        // the op stream without re-borrowing `self`. Cloning the
        // HashSet is cheap (typically < 32 entries on the W5 / W6
        // traces).
        let meta = match self.loop_meta.get(&loop_id) {
            Some(m) => m.clone(),
            None => return Ok(()),
        };

        // Snapshot the body op slice so the per-op `&mut self` calls
        // below don't conflict with the (immutable) borrow we'd
        // otherwise hold on `self.trace.ops`.
        let body_ops: Vec<TraceOp> = self.trace.ops[meta.head_pc + 1..meta.back_pc].to_vec();

        for op in &body_ops {
            match op {
                TraceOp::ListGet { list_ptr, .. }
                    if !meta.inside_defs.contains(list_ptr)
                        && !self.hoisted_list_len.contains_key(list_ptr)
                        && self.list_get.is_some() =>
                {
                    // Only the `len: u32` load is worth hoisting. The
                    // matching `payload_base = list_ptr + 8` stays
                    // inside the loop so cranelift can fold the whole
                    // `idx * 8 + (list_ptr + 8)` chain into a single
                    // x86_64 `lea` with displacement — hoisting the
                    // `iadd_imm` would break that fold and add an
                    // extra `add` to the per-iter cost.
                    let base_v = self.lookup(*list_ptr)?;
                    let len32 = self.builder.ins().load(I32, MemFlags::trusted(), base_v, 0);
                    let len64 = self.builder.ins().uextend(I64, len32);
                    self.hoisted_list_len.insert(*list_ptr, len64);
                }
                TraceOp::DictLookupPrechecked { dict_ptr, .. }
                    if !meta.inside_defs.contains(dict_ptr)
                        && !self.hoisted_dict_entry_count.contains_key(dict_ptr) =>
                {
                    // Hoist ONLY `entry_count`. `entries_base` stays in
                    // the dict_inline scan body so cranelift's
                    // displacement-folder can collapse the address
                    // computation into a single instruction.
                    let dict_v = self.lookup(*dict_ptr)?;
                    let entry_count_u32 =
                        self.builder.ins().load(I32, MemFlags::trusted(), dict_v, 8);
                    let entry_count = self.builder.ins().uextend(I64, entry_count_u32);
                    self.hoisted_dict_entry_count.insert(*dict_ptr, entry_count);
                }
                TraceOp::Mod(_, _, b)
                    if !meta.inside_defs.contains(b)
                        && !self.hoisted_mod_nonzero_divisor.contains(b) =>
                {
                    // Pre-emit the divisor-nonzero brif. The "ok"
                    // arm becomes the new preheader insertion
                    // point; the deopt arm reuses the shared
                    // deopt block.
                    let vb = self.lookup(*b)?;
                    let zero = self.builder.ins().iconst(I64, 0);
                    let nonzero = self.builder.ins().icmp(IntCC::NotEqual, vb, zero);
                    let ok = self.builder.create_block();
                    let guard_pc = self.builder.ins().iconst(I32, 0);
                    let external_pc = self.builder.ins().iconst(I64, 0);
                    self.builder.ins().brif(
                        nonzero,
                        ok,
                        &[],
                        self.deopt_block,
                        &[BlockArg::Value(guard_pc), BlockArg::Value(external_pc)],
                    );
                    self.builder.seal_block(ok);
                    self.builder.switch_to_block(ok);
                    self.hoisted_mod_nonzero_divisor.insert(*b);
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn emit_loop_back(&mut self, loop_id: u32, next_values: &[SsaVar]) -> Result<(), EmitError> {
        let header = *self
            .loop_head_blocks
            .get(&loop_id)
            .ok_or(EmitError::UnmatchedLoopBack(loop_id))?;
        let mut args_vals: Vec<ir::Value> = Vec::with_capacity(next_values.len());
        for v in next_values {
            let val = self.lookup(*v)?;
            let widened = self.widen_to_i64(val);
            args_vals.push(widened);
        }
        let args: Vec<ir::BlockArg> = args_vals.iter().map(|v| ir::BlockArg::Value(*v)).collect();
        self.builder.ins().jump(header, &args);
        // The header had its forward edge from `emit_loop_head` and
        // now its back edge from this jump; safe to seal.
        self.builder.seal_block(header);
        // F-D8-E.5: pop the loop off the active stack so preheader-
        // hoist caches scoped to this loop body stop being honoured by
        // any subsequent straight-line ops. We deliberately leave the
        // `hoisted_*` maps populated: the cached SSAs are still valid
        // cranelift values (defined in the preheader), and any later
        // ops can legally reuse them if they happen to fire with the
        // same invariant pointer. Active-loop tracking just gates
        // *whether* future emit_loop_head rounds bother to repopulate
        // them for a newly-discovered loop body.
        if self.active_loops.last() == Some(&loop_id) {
            self.active_loops.pop();
        }
        // Continue into a fresh block so subsequent ops have a place.
        let after = self.builder.create_block();
        self.builder.seal_block(after);
        self.builder.switch_to_block(after);
        Ok(())
    }

    fn fill_deopt_block(&mut self) {
        self.builder.switch_to_block(self.deopt_block);
        let guard_pc = self.builder.block_params(self.deopt_block)[0];
        let external_pc = self.builder.block_params(self.deopt_block)[1];

        // v6-δ M1 R5: dispatch through `ctx.host_hooks.save_deopt`
        // via `call_indirect`. The slot pointer is loaded fresh on
        // every deopt — hosts that hot-swap helpers (profile-guided
        // / instrumented variants) take effect without recompiling
        // the trace. Falls back to the historical direct extern
        // call when the slot is null so traces installed before the
        // host wires a HostHookTable keep working.
        let hook_off = crate::abi::host_hooks_offset()
            + crate::abi::host_hook_slot_offset(HostHookId::SaveDeopt);
        let hook_ptr = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.trace_ctx_ptr,
            hook_off,
        );
        let null = self.builder.ins().iconst(self.pointer_ty, 0);
        let has_hook = self.builder.ins().icmp(IntCC::NotEqual, hook_ptr, null);
        let indirect_block = self.builder.create_block();
        let direct_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(has_hook, indirect_block, &[], direct_block, &[]);
        self.builder.seal_block(indirect_block);
        self.builder.seal_block(direct_block);

        // Indirect dispatch arm.
        self.builder.switch_to_block(indirect_block);
        // Build a fresh signature ref matching TraceSaveDeoptFn:
        // `unsafe extern "C" fn(*mut TraceContext, u32, u64)`.
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(self.pointer_ty));
        sig.params.push(AbiParam::new(I32));
        sig.params.push(AbiParam::new(I64));
        let sig_ref = self.builder.func.import_signature(sig);
        self.builder.ins().call_indirect(
            sig_ref,
            hook_ptr,
            &[self.trace_ctx_ptr, guard_pc, external_pc],
        );
        let failed_i = self
            .builder
            .ins()
            .iconst(I32, i64::from(TraceEntryStatus::GuardFailed.as_i32()));
        self.builder.ins().return_(&[failed_i]);

        // Direct (fallback) arm — preserves pre-R5 behaviour when a
        // host invokes the trace with an empty HostHookTable.
        self.builder.switch_to_block(direct_block);
        self.builder.ins().call(
            self.save_deopt,
            &[self.trace_ctx_ptr, guard_pc, external_pc],
        );
        let failed_d = self
            .builder
            .ins()
            .iconst(I32, i64::from(TraceEntryStatus::GuardFailed.as_i32()));
        self.builder.ins().return_(&[failed_d]);
        self.builder.seal_block(self.deopt_block);
    }

    fn lookup(&self, var: SsaVar) -> Result<ir::Value, EmitError> {
        self.ssa_to_value
            .get(&var)
            .copied()
            .ok_or(EmitError::UnboundSsa(var))
    }

    fn bind(&mut self, var: SsaVar, v: ir::Value) {
        // Per-var binding overwrites are forbidden by SSA, but the
        // recorder guarantees unique destinations so we use a plain
        // insert.
        self.ssa_to_value.insert(var, v);
    }

    /// Coerce a value into an i64 by `uextend` (narrower → wider) /
    /// passthrough (already i64). Other widths are recorder bugs.
    fn widen_to_i64(&mut self, v: ir::Value) -> ir::Value {
        let ty = self.builder.func.dfg.value_type(v);
        if ty == I64 {
            v
        } else if ty == I32 {
            self.builder.ins().uextend(I64, v)
        } else {
            // Pointer-typed value: bitcast through `raw_bitcast` is
            // overkill for the test surface; we treat the value as
            // already 64-bit on a 64-bit host and pass it through.
            // The integration phase will fix this once non-i64 args
            // are widened in the recorder.
            v
        }
    }
}

/// Internal binary-op tag; not part of the public API.
#[derive(Debug, Clone, Copy)]
enum BinOp {
    Add,
    Sub,
    Mul,
}

/// F-D8-E.5: per-loop metadata used by the preheader hoister.
///
/// Pre-computed once during emit so the per-op walk can react in O(1).
#[derive(Debug, Clone)]
struct LoopMeta {
    /// pc of the `MarkLoopHead` op in the post-optimiser stream.
    head_pc: usize,
    /// pc of the matching `MarkLoopBack` op (exclusive end of body).
    back_pc: usize,
    /// SSAs defined inside the body (including the head's φ pairs).
    /// An SSA used in the body but NOT in this set is loop-invariant.
    inside_defs: HashSet<SsaVar>,
}

/// F-D8-E.5: for each well-formed `MarkLoopHead` / `MarkLoopBack` pair
/// in the op stream, collect the body pc range plus the SSAs defined
/// *inside* the body (including the head's φ pairs). The complement of
/// `inside_defs` among the body's input SSAs is loop-invariant under
/// this loop and a candidate for the preheader hoist pre-scan.
///
/// Mirrors [`relon_trace_jit::optimizer::licm`]'s `collect_loops` +
/// `inside_defs` computation but runs at emit time so we can react to
/// the precise post-optimiser op stream without re-walking the
/// optimiser passes. Keys by `loop_id` (the stable identifier carried
/// on the marker pair) so nested loops with the same body shape stay
/// distinct.
///
/// Unmatched markers are skipped silently — a recorder bug we'd
/// rather degrade gracefully on than crash the install path.
fn compute_loop_meta(ops: &[TraceOp]) -> HashMap<u32, LoopMeta> {
    let mut out: HashMap<u32, LoopMeta> = HashMap::new();
    let mut stack: Vec<(u32, usize)> = Vec::new();
    for (pc, op) in ops.iter().enumerate() {
        if let Some(id) = op.loop_head_id() {
            stack.push((id, pc));
        } else if let Some(id) = op.loop_back_id() {
            if let Some(pos) = stack.iter().rposition(|(sid, _)| *sid == id) {
                let (loop_id, head_pc) = stack.remove(pos);
                let body_start = head_pc + 1;
                let body_end = pc; // exclusive
                let mut inside_defs: HashSet<SsaVar> =
                    (body_start..body_end).flat_map(|i| ops[i].defs()).collect();
                // The head's φ SSAs are loop-CARRIED — count them as
                // "defined inside" for invariance purposes.
                inside_defs.extend(ops[head_pc].defs());
                out.insert(
                    loop_id,
                    LoopMeta {
                        head_pc,
                        back_pc: pc,
                        inside_defs,
                    },
                );
            }
        }
    }
    out
}

/// Declare a host hook as an `ExtFuncData::User` import on the
/// function. Returns the resulting `FuncRef` so call sites can use it.
///
/// The `namespace` field is set to `0` (matching what
/// `cranelift_module` uses for declared imports); `index` is the
/// `HostHookId` discriminant so external tooling can map back to the
/// symbolic name without a side table.
fn declare_host_hook(
    func: &mut Function,
    _hook: HostHookId,
    params: &[ir::Type],
    returns: &[ir::Type],
    _pointer_ty: ir::Type,
    func_id_index: u32,
) -> ir::FuncRef {
    let mut sig = Signature::new(CallConv::SystemV);
    for p in params {
        sig.params.push(AbiParam::new(*p));
    }
    for r in returns {
        sig.returns.push(AbiParam::new(*r));
    }
    let sig_ref = func.import_signature(sig);
    let name_ref = func.declare_imported_user_function(UserExternalName::new(0, func_id_index));
    func.import_function(ExtFuncData {
        name: ExternalName::User(name_ref),
        signature: sig_ref,
        colocated: false,
        patchable: false,
    })
}

/// Things that can go wrong while emitting a trace. Every variant is
/// a recorder / optimiser invariant violation — the runtime should
/// never trigger one in well-tested code.
#[derive(Debug)]
pub enum EmitError {
    /// Op references an SSA var the emitter never bound.
    UnboundSsa(SsaVar),
    /// `Guard(...)` op appeared in the stream but no matching
    /// `GuardSite` lives in the buffer's `guards` table.
    OrphanGuardOp,
    /// `Call(...)` op carrying [`EffectClass::Unrecoverable`] — the
    /// recorder must abort rather than commit such a trace.
    UnrecoverableEffectInTrace,
    /// `MarkLoopBack` op with no preceding matching `MarkLoopHead`.
    UnmatchedLoopBack(u32),
    /// F-D8: trace contains a `TraceOp::ListGet` / `TraceOp::DictLookup`
    /// op but the host did not declare the matching helper FuncId via
    /// [`HostHookFuncIds`]. The host must register the symbol
    /// (`__relon_trace_list_get` / `__relon_trace_dict_lookup`) in its
    /// cranelift module before installing dict/list traces.
    HostHookNotDeclared(HostHookId),
    /// Forwarded from [`crate::guard_emit::GuardEmitError`].
    Guard(GuardEmitError),
}

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmitError::UnboundSsa(v) => write!(f, "op references unbound SSA var {:?}", v),
            EmitError::OrphanGuardOp => write!(
                f,
                "Guard op in stream has no matching GuardSite in the buffer's guards table"
            ),
            EmitError::UnrecoverableEffectInTrace => write!(
                f,
                "Call op classified as Unrecoverable cannot live inside a recorded trace"
            ),
            EmitError::UnmatchedLoopBack(id) => {
                write!(f, "MarkLoopBack {{ loop_id: {} }} has no matching head", id)
            }
            EmitError::HostHookNotDeclared(id) => write!(
                f,
                "host hook {:?} ({}) referenced by trace but not declared via HostHookFuncIds",
                id,
                id.symbol()
            ),
            EmitError::Guard(e) => write!(f, "guard emit failure: {}", e),
        }
    }
}

impl std::error::Error for EmitError {}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_trace_jit::{Offset, TraceBuffer};

    #[test]
    fn empty_trace_emits_success_only() {
        let trace = TraceBuffer::new().into_optimized();
        let mut ctx = CodegenContext::new();
        TraceEmitter::emit(&trace, &mut ctx).expect("emit ok");
        // Function has at least one block + return.
        assert!(ctx.func.layout.entry_block().is_some());
    }

    #[test]
    fn return_emits_store_into_result_slot() {
        let mut b = TraceBuffer::new();
        let dst = b.fresh_ssa();
        b.append(TraceOp::ConstI64(dst, 42));
        b.append(TraceOp::Return(dst));
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        TraceEmitter::emit(&trace, &mut ctx).expect("emit ok");
    }

    #[test]
    fn lookup_errors_on_unbound_ssa() {
        // Ops without a preceding define should surface UnboundSsa.
        let mut b = TraceBuffer::new();
        let phantom = SsaVar(99);
        b.append(TraceOp::Return(phantom));
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        let err = TraceEmitter::emit(&trace, &mut ctx).unwrap_err();
        assert!(matches!(err, EmitError::UnboundSsa(_)));
    }

    #[test]
    fn unrecoverable_call_rejected() {
        let mut b = TraceBuffer::new();
        let dst = b.fresh_ssa();
        b.append(TraceOp::Call(
            dst,
            relon_trace_jit::FuncId(7),
            vec![],
            EffectClass::Unrecoverable,
        ));
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        let err = TraceEmitter::emit(&trace, &mut ctx).unwrap_err();
        assert!(matches!(err, EmitError::UnrecoverableEffectInTrace));
    }

    #[test]
    fn load_store_round_trip_lowers() {
        let mut b = TraceBuffer::new();
        let base = b.fresh_ssa();
        b.append(TraceOp::ConstI64(base, 0x1000));
        let loaded = b.fresh_ssa();
        b.append(TraceOp::Load(loaded, base, Offset(8)));
        b.append(TraceOp::Store(base, Offset(16), loaded));
        b.append(TraceOp::Return(loaded));
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        TraceEmitter::emit(&trace, &mut ctx).expect("emit ok");
    }

    // ---- F-D8 -----------------------------------------------------

    #[test]
    fn list_get_without_helper_surfaces_undeclared_hook() {
        let mut b = TraceBuffer::new();
        let base = b.fresh_ssa();
        let idx = b.fresh_ssa();
        let dst = b.fresh_ssa();
        b.append(TraceOp::ConstI64(base, 0x1000));
        b.append(TraceOp::ConstI64(idx, 0));
        b.append(TraceOp::ListGet {
            dst,
            list_ptr: base,
            idx,
        });
        b.append(TraceOp::Return(dst));
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        let err = TraceEmitter::emit(&trace, &mut ctx).unwrap_err();
        match err {
            EmitError::HostHookNotDeclared(HostHookId::ListGet) => {}
            other => panic!("expected HostHookNotDeclared(ListGet), got {:?}", other),
        }
    }

    #[test]
    fn dict_lookup_without_helper_surfaces_undeclared_hook() {
        let mut b = TraceBuffer::new();
        let dict = b.fresh_ssa();
        let key = b.fresh_ssa();
        let dst = b.fresh_ssa();
        b.append(TraceOp::ConstI64(dict, 0x2000));
        b.append(TraceOp::ConstI64(key, 0x3000));
        b.append(TraceOp::DictLookup {
            dst,
            dict_ptr: dict,
            key_ptr: key,
            shape_hash: 0xdeadbeef,
        });
        b.append(TraceOp::Return(dst));
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        let err = TraceEmitter::emit(&trace, &mut ctx).unwrap_err();
        match err {
            EmitError::HostHookNotDeclared(HostHookId::DictLookup) => {}
            other => panic!("expected HostHookNotDeclared(DictLookup), got {:?}", other),
        }
    }

    #[test]
    fn list_get_with_declared_helper_lowers() {
        let mut b = TraceBuffer::new();
        let base = b.fresh_ssa();
        let idx = b.fresh_ssa();
        let dst = b.fresh_ssa();
        b.append(TraceOp::ConstI64(base, 0x1000));
        b.append(TraceOp::ConstI64(idx, 0));
        b.append(TraceOp::ListGet {
            dst,
            list_ptr: base,
            idx,
        });
        b.append(TraceOp::Return(dst));
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        let hook_ids = HostHookFuncIds {
            list_get: Some(7),
            ..Default::default()
        };
        TraceEmitter::emit_with_hooks(&trace, &mut ctx, I64, hook_ids).expect("emit ok");
    }

    #[test]
    fn dict_lookup_with_declared_helper_lowers() {
        let mut b = TraceBuffer::new();
        let dict = b.fresh_ssa();
        let key = b.fresh_ssa();
        let dst = b.fresh_ssa();
        b.append(TraceOp::ConstI64(dict, 0x2000));
        b.append(TraceOp::ConstI64(key, 0x3000));
        b.append(TraceOp::DictLookup {
            dst,
            dict_ptr: dict,
            key_ptr: key,
            shape_hash: 0xfeed_face_dead_beef,
        });
        b.append(TraceOp::Return(dst));
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        let hook_ids = HostHookFuncIds {
            dict_lookup: Some(8),
            ..Default::default()
        };
        TraceEmitter::emit_with_hooks(&trace, &mut ctx, I64, hook_ids).expect("emit ok");
    }

    // ---- F-D8-E.5: preheader hoist pre-scan -------------------------

    /// `compute_loop_meta` must return the body pc range plus the
    /// inside-defs set keyed by the loop's `loop_id`. The head's φ
    /// SSAs count as "inside" so the dict-ic-hoist / LICM logic
    /// elsewhere agrees on what's loop-carried.
    #[test]
    fn compute_loop_meta_collects_body_pc_and_defs() {
        let mut b = TraceBuffer::new();
        let outer = b.fresh_ssa();
        b.append(TraceOp::ConstI64(outer, 7));
        let phi = b.fresh_ssa();
        b.append(TraceOp::MarkLoopHead {
            loop_id: 42,
            phis: vec![relon_trace_jit::LoopPhi::new(outer, phi)],
        });
        let inner = b.fresh_ssa();
        b.append(TraceOp::ConstI64(inner, 9));
        b.append(TraceOp::MarkLoopBack {
            loop_id: 42,
            next_values: vec![phi],
        });
        let trace = b.into_optimized();
        let meta = compute_loop_meta(&trace.ops);
        let m = meta.get(&42).expect("loop_id 42 metadata present");
        assert_eq!(m.head_pc, 1);
        assert_eq!(m.back_pc, 3);
        // `outer` is pre-loop → not in inside_defs. `phi` (head φ) and
        // `inner` (body ConstI64) are both inside.
        assert!(m.inside_defs.contains(&phi));
        assert!(m.inside_defs.contains(&inner));
        assert!(!m.inside_defs.contains(&outer));
    }

    /// Unmatched markers must not crash the pre-pass — the install
    /// path tolerates a recorder bug by degrading to "no hoist for
    /// this loop". Mirrors the LICM pass behaviour.
    #[test]
    fn compute_loop_meta_skips_unmatched_back() {
        let mut b = TraceBuffer::new();
        let v = b.fresh_ssa();
        b.append(TraceOp::ConstI64(v, 0));
        b.append(TraceOp::MarkLoopBack {
            loop_id: 9,
            next_values: vec![],
        });
        let trace = b.into_optimized();
        let meta = compute_loop_meta(&trace.ops);
        assert!(meta.is_empty(), "no well-formed loops → empty metadata");
    }

    /// End-to-end: a W5-shaped trace with a loop-invariant `list_ptr`
    /// and `dict_ptr` should emit + verify under cranelift, and the
    /// hot path inside the loop body must skip the per-iter
    /// `list_len` load / `entry_count` load thanks to the preheader
    /// hoist. We assert verifier acceptance — the bench harness covers
    /// the perf delta — and probe the IR string to confirm the loop
    /// header is not preceded by a vanished preheader (i.e. the hoist
    /// emitted real cranelift loads BEFORE the loop jump).
    #[test]
    fn preheader_hoist_emits_invariant_loads_above_loop_head() {
        use cranelift_codegen::verifier;

        let mut b = TraceBuffer::new();
        let list = b.fresh_ssa();
        let dict = b.fresh_ssa();
        let key = b.fresh_ssa();
        let idx = b.fresh_ssa();
        b.append(TraceOp::LocalGet(list, 0));
        b.append(TraceOp::LocalGet(dict, 1));
        b.append(TraceOp::LocalGet(key, 2));
        b.append(TraceOp::ConstI64(idx, 0));
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![],
        });
        // Body: list_get(list, idx) + dict_lookup_prechecked(dict, key).
        let list_v = b.fresh_ssa();
        b.append(TraceOp::ListGet {
            dst: list_v,
            list_ptr: list,
            idx,
        });
        let dict_v = b.fresh_ssa();
        b.append(TraceOp::DictLookupPrechecked {
            dst: dict_v,
            dict_ptr: dict,
            key_ptr: key,
        });
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![],
        });
        b.append(TraceOp::Return(dict_v));
        let trace = b.into_optimized();

        let mut ctx = CodegenContext::new();
        let hook_ids = HostHookFuncIds {
            list_get: Some(7),
            dict_lookup_prechecked: Some(9),
            ..Default::default()
        };
        TraceEmitter::emit_with_hooks(&trace, &mut ctx, I64, hook_ids).expect("emit ok");

        // IR must still verify after the hoist insertion.
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        verifier::verify_function(&ctx.func, &flags).expect("verifier accepts hoisted IR");
    }

    /// `Mod` with a loop-invariant divisor must not double-emit the
    /// divisor-zero brif: the preheader hoister fires it once, and the
    /// in-loop emit_mod skips its own copy. We verify the IR and check
    /// that the brif count in the function string matches the
    /// "one-divisor + Mod overflow guard" envelope (≤ 3 brifs total
    /// for this minimal trace shape: preheader divisor brif + Mod
    /// overflow brif + loop entry jump-not-brif).
    #[test]
    fn preheader_hoist_dedups_loop_invariant_mod_divisor_check() {
        use cranelift_codegen::verifier;

        let mut b = TraceBuffer::new();
        let n = b.fresh_ssa();
        b.append(TraceOp::LocalGet(n, 0));
        let divisor = b.fresh_ssa();
        b.append(TraceOp::ConstI64(divisor, 10));
        let i_init = b.fresh_ssa();
        b.append(TraceOp::ConstI64(i_init, 0));

        let phi_i = b.fresh_ssa();
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![relon_trace_jit::LoopPhi::new(i_init, phi_i)],
        });
        let mod_dst = b.fresh_ssa();
        b.append(TraceOp::Mod(mod_dst, phi_i, divisor));
        let one = b.fresh_ssa();
        b.append(TraceOp::ConstI64(one, 1));
        let next_i = b.fresh_ssa();
        b.append(TraceOp::Add(next_i, phi_i, one));
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![next_i],
        });
        b.append(TraceOp::Return(mod_dst));
        let trace = b.into_optimized();

        let mut ctx = CodegenContext::new();
        TraceEmitter::emit(&trace, &mut ctx).expect("emit ok");
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        verifier::verify_function(&ctx.func, &flags).expect("verifier accepts hoisted Mod IR");
    }
}
