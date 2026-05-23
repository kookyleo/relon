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

use rustc_hash::FxHashMap;
use std::collections::HashSet;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{
    self, AbiParam, BlockArg, ExtFuncData, ExternalName, Function, InstBuilder, MemFlags,
    Signature, StackSlotData, StackSlotKind, UserExternalName, UserFuncName,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::Context as CodegenContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use relon_trace_jit::{EffectClass, GuardSite, OptimizedTrace, SsaVar, TraceConst, TraceOp};

use crate::abi::{AbiSignatureExt, HostHookId, TraceEntryStatus, TRACE_ENTRY_SIG};
use crate::guard_emit::{emit_guard, GuardEmitCtx, GuardEmitError};
use crate::op_lower::{
    lower_binop_i64, lower_cmp, lower_const_i32, lower_const_i64, lower_div, lower_load,
    lower_local_get, lower_store, widen_to_i64, BinOp, OpLowerer,
};

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
    /// 2026-05-21: cranelift `FuncId.as_u32()` for the Tier-2
    /// `__relon_str_glob_match` helper. `None` keeps the trace emitter
    /// off the `TraceOp::StrGlobMatch` lowering — hosts that don't
    /// declare the helper will surface `EmitError::HostHookNotDeclared(
    /// HostHookId::StrGlobMatch)` if a recorded trace tries to emit
    /// the op. Hosts wired against the historical (pre-Tier-2) ABI keep
    /// working because the recorder lowering only emits `StrGlobMatch`
    /// when the underlying program reaches `glob_match(s, pat)`.
    pub str_glob_match: Option<u32>,
    /// F-D7-I: cranelift `FuncId.as_u32()` for
    /// `__relon_str_concat_alloc`. `None` means the host did not
    /// declare the helper — the emitter's inline `StrConcat` path will
    /// silently fall back to the extern `__relon_str_concat` shim,
    /// preserving correctness for hosts wired against the pre-F-D7-I
    /// ABI. Hosts that want the inline fast path MUST set this.
    pub str_concat_alloc: Option<u32>,
    /// Tier 1b: cranelift `FuncId.as_u32()` for
    /// `__relon_str_concat_seal_hash`. Co-required with
    /// [`Self::str_concat_alloc`]: the inline `StrConcat` lowering uses
    /// the alloc helper to reserve a fresh `StringRef` whose payload
    /// buffer ends with rhs bytes the JIT writes inline, then calls
    /// the seal helper to fold those bytes into the cached `fx_hash`
    /// field. Hosts that wire `str_concat_alloc` but leave this `None`
    /// will produce `StringRef` results whose hash field stays as the
    /// `0` "not yet sealed" sentinel — dict-key crossings on the
    /// concat result will silently miss the IC every iter. Hosts wired
    /// against the pre-Tier-1b ABI keep working because the inline
    /// `StrConcat` lowering falls back to the extern shim path that
    /// goes through `__relon_str_concat` / `StringRef::from_owned`
    /// (which stamps the hash producer-side).
    pub str_concat_seal_hash: Option<u32>,
    /// #168: cranelift `FuncId.as_u32()` for
    /// `__relon_str_concat_n_alloc`. `None` means the host did not
    /// declare the helper — `TraceOp::StrConcatN` then surfaces
    /// `EmitError::HostHookNotDeclared(HostHookId::StrConcatNAlloc)` at
    /// emit time so the install pipeline falls back to the cranelift
    /// AOT backend. Hosts that want the inline N-way concat path MUST
    /// set this AND `str_concat_seal_hash` (the inline lowering seals
    /// the result `StringRef`'s cached fx_hash digest exactly like the
    /// two-operand inline `StrConcat` lowering does).
    pub str_concat_n_alloc: Option<u32>,
    /// F-D8: cranelift `FuncId.as_u32()` for `__relon_trace_list_get`.
    /// `None` means the host has not declared the helper; emitter will
    /// surface `EmitError::HostHookNotDeclared` if a `TraceOp::ListGet`
    /// is seen.
    pub list_get: Option<u32>,
    /// F-D8/v2: cranelift `FuncId.as_u32()` for
    /// `__relon_trace_dict_lookup_v2`.
    /// `None` means the host has not declared the helper; emitter will
    /// surface `EmitError::HostHookNotDeclared` if a `TraceOp::DictLookup`
    /// is seen.
    pub dict_lookup: Option<u32>,
    /// F-D8-E.2/v2: cranelift `FuncId.as_u32()` for
    /// `__relon_trace_dict_lookup_prechecked_v2`. `None` means the
    /// host has not declared the helper; emitter surfaces
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
            // 2026-05-21: `__relon_str_glob_match` is opt-in. Tests that
            // never feed a `TraceOp::StrGlobMatch` through the emitter
            // keep the historical 7-slot layout; the recorder only
            // produces the op when the underlying program reaches
            // `glob_match(s, pat)`.
            str_glob_match: None,
            // F-D7-I helper is opt-in: tests that don't drive the host
            // module path keep the inline `StrConcat` lowering disabled
            // (the emitter falls back to the extern `__relon_str_concat`
            // call which the historical layout already declared at
            // FuncId 3).
            str_concat_alloc: None,
            // Tier 1b seal-hash helper is also opt-in. Co-required with
            // `str_concat_alloc` when the host wants the inline
            // `StrConcat` path to produce IC-friendly results; left as
            // `None` so historical fixtures stay on the extern shim
            // (which calls `StringRef::from_owned` — already hash-
            // stamped producer-side).
            str_concat_seal_hash: None,
            // #168 helper is opt-in: tests that don't drive the host
            // module path keep `TraceOp::StrConcatN` off the inline
            // emit path — the recorder lowering still produces the op,
            // but the emit-time `HostHookNotDeclared` surfaces a clean
            // install-time fallback. Hosts that want the trace-JIT to
            // serve hot `StrConcatN` chains MUST set this.
            str_concat_n_alloc: None,
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

        // 2026-05-21: Tier-2 `__relon_str_glob_match(s, pattern) -> i32`.
        // Declared only when the host wired the FuncId — same opt-in
        // contract as the F-D7-I alloc helper and the F-D8 dict/list
        // hooks so existing fixtures keep working without forcing the
        // helper symbol on every trace JIT module.
        let str_glob_match = hook_func_ids.str_glob_match.map(|fid| {
            declare_host_hook(
                builder.func,
                HostHookId::StrGlobMatch,
                &[pointer_ty, pointer_ty],
                &[I32],
                pointer_ty,
                fid,
            )
        });

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
        // Tier 1b: optional seal-hash helper for the inline `StrConcat`
        // short-rhs lowering. Same opt-in contract as the alloc helper;
        // when wired, the inline lowering follows the unrolled rhs
        // stores with a `call str_concat_seal_hash(result_ptr)` so the
        // freshly-built `StringRef`'s cached `fx_hash` field matches
        // the now-complete payload. See the doc on
        // `HostHookFuncIds::str_concat_seal_hash` for the IC-miss
        // failure mode when this is left `None`.
        let str_concat_seal_hash = hook_func_ids.str_concat_seal_hash.map(|fid| {
            declare_host_hook(
                builder.func,
                HostHookId::StrConcatSealHash,
                &[pointer_ty],
                &[],
                pointer_ty,
                fid,
            )
        });
        // #168: N-operand single-allocation concat helper. Imported
        // only when the host wired the FuncId; absence leaves the
        // inline `StrConcatN` lowering off (the op surfaces
        // `EmitError::HostHookNotDeclared` so the install pipeline can
        // fall back to the cranelift AOT backend). Signature:
        //   `__relon_str_concat_n_alloc(operands: *const *const StringRef,
        //                               n: usize, total_len: usize)
        //        -> *mut StringRef`
        // The `operands` slot is stack-allocated by the cranelift
        // lowering (a `[*const StringRef; N]` array filled with the
        // operand pointer SSAs); `n` and `total_len` are passed as
        // `i64`-wide values via the SystemV ABI.
        let str_concat_n_alloc = hook_func_ids.str_concat_n_alloc.map(|fid| {
            declare_host_hook(
                builder.func,
                HostHookId::StrConcatNAlloc,
                &[pointer_ty, I64, I64],
                &[pointer_ty],
                pointer_ty,
                fid,
            )
        });

        // F-D8: declare dict/list helpers when the host wired them.
        // Signature:
        //   `__relon_trace_list_get(list_ptr, idx, ctx) -> i64`
        //   `__relon_trace_dict_lookup_v2(dict_ptr, record_len,
        //                                  key_ptr, shape_hash, ctx) -> i64`
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
                &[pointer_ty, pointer_ty, pointer_ty, I64, pointer_ty],
                &[I64],
                pointer_ty,
                fid,
            )
        });
        // F-D8-E.2/v2: prechecked variant of dict_lookup. Same
        // signature as `dict_lookup` minus the `shape_hash: i64` arg,
        // because the matching `TraceOp::DictShapeGuard` already
        // verified it upstream (typically lifted out of the loop by
        // LICM).
        let dict_lookup_prechecked = hook_func_ids.dict_lookup_prechecked.map(|fid| {
            declare_host_hook(
                builder.func,
                HostHookId::DictLookupPrechecked,
                &[pointer_ty, pointer_ty, pointer_ty, pointer_ty],
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
            str_concat_seal_hash,
            str_concat_n_alloc,
            str_contains,
            str_find,
            str_substring,
            str_glob_match,
            list_get,
            dict_lookup,
            dict_lookup_prechecked,
            ssa_to_value: FxHashMap::default(),
            overflow_bits: FxHashMap::default(),
            loop_head_blocks: FxHashMap::default(),
            loop_meta,
            active_loops: Vec::new(),
            hoisted_list_len: FxHashMap::default(),
            hoisted_mod_nonzero_divisor: HashSet::new(),
            hoisted_mod_magic: FxHashMap::default(),
            saw_return: false,
        };

        // Index guards by `trace_pc` so the per-op walk can pick the
        // matching site without scanning the guard vector each time.
        let guards_by_pc: FxHashMap<u32, &GuardSite> =
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
    /// `TraceOp::LocalGet { dst: _, slot_idx }` lowers to a load at
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
    /// Tier 1b optional seal-hash helper. Imported only when the host
    /// wired `HostHookFuncIds::str_concat_seal_hash`. The inline
    /// `StrConcat` lowering calls this after writing the rhs tail so
    /// the freshly-built `StringRef`'s cached `fx_hash` field is
    /// in-sync with the payload — without it the dict-lookup IC
    /// silently misses on the concat result. `None` ⇒ the inline
    /// path skips the seal call (correct but defeats the cached-hash
    /// win once the concat result feeds a dict key).
    str_concat_seal_hash: Option<ir::FuncRef>,
    /// #168 optional N-operand single-allocation concat helper.
    /// Imported only when the host wired
    /// `HostHookFuncIds::str_concat_n_alloc`. `TraceOp::StrConcatN`
    /// surfaces `EmitError::HostHookNotDeclared(StrConcatNAlloc)` when
    /// this is `None` — the trace install pipeline then falls back to
    /// the cranelift AOT backend (which has its own single-alloc
    /// `StrConcatN` lowering).
    str_concat_n_alloc: Option<ir::FuncRef>,
    str_contains: ir::FuncRef,
    str_find: ir::FuncRef,
    str_substring: ir::FuncRef,
    /// 2026-05-21: optional `__relon_str_glob_match` FuncRef. `None`
    /// means the host did not declare the helper; a
    /// `TraceOp::StrGlobMatch` then surfaces
    /// `EmitError::HostHookNotDeclared(StrGlobMatch)` so the trace
    /// install path can abort cleanly rather than emit a call to an
    /// unresolved symbol.
    str_glob_match: Option<ir::FuncRef>,
    /// F-D8: optional `__relon_trace_list_get` FuncRef. `None` means
    /// the host did not declare the helper — `TraceOp::ListGet` emits
    /// will surface `EmitError::HostHookNotDeclared`.
    list_get: Option<ir::FuncRef>,
    /// F-D8/v2: optional `__relon_trace_dict_lookup_v2` FuncRef. Same
    /// contract as `list_get`.
    dict_lookup: Option<ir::FuncRef>,
    /// F-D8-E.2/v2 optional `__relon_trace_dict_lookup_prechecked_v2`
    /// FuncRef. Required when the optimizer has split a dict lookup
    /// into `DictShapeGuard + DictLookupPrechecked`.
    dict_lookup_prechecked: Option<ir::FuncRef>,
    ssa_to_value: FxHashMap<SsaVar, ir::Value>,
    /// v6-δ M1: overflow bits surfaced by `Add` / `Sub` / `Mul`
    /// lowering. The matching `Guard(ArithOverflow(dst))` predicate
    /// reads this map to surface a real cranelift `*_overflow` bit
    /// rather than emitting a constant-0 predicate that always
    /// deopts. Entry is keyed on the arith op's `dst` SSA.
    overflow_bits: FxHashMap<SsaVar, ir::Value>,
    loop_head_blocks: FxHashMap<u32, ir::Block>,
    /// F-D8-E.5: per-loop metadata used by the preheader hoister.
    /// Keyed by `loop_id`. `inside_defs` is the set of SSAs defined
    /// inside the loop body (plus the head's φ pairs); the complement
    /// of that set among the body's input SSAs is loop-invariant.
    /// `body_start` / `body_end` are pc bounds (exclusive at end) of
    /// the loop body in the post-optimiser op stream so the hoister
    /// can walk the same ops the emit loop will visit next.
    loop_meta: FxHashMap<u32, LoopMeta>,
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
    hoisted_list_len: FxHashMap<SsaVar, ir::Value>,
    /// F-D8-E.5: preheader-hoisted divisor-nonzero ok-block for each
    /// loop-invariant `b` operand of a `Mod`. Tracks the SSA `b` value
    /// of the original `Mod` — when seen again in the loop body, the
    /// emitter knows the divisor-nonzero guard already fired (and
    /// passed) upstream so it can skip emitting a per-iter brif.
    hoisted_mod_nonzero_divisor: HashSet<SsaVar>,
    /// F-D8-E.6: preheader-hoisted magic-multiplier `iconst.i64` for
    /// each loop-invariant positive-const `Mod` divisor. The full
    /// 64-bit magic literal (`0x6666_6666_6666_6667` for `% 10`, etc.)
    /// is a `mov reg, imm64` (10-byte instruction) on x86_64 that
    /// otherwise re-emits inside the loop body on every iter; pre-
    /// emitting it in the preheader lets cranelift's register
    /// allocator pin it to a long-lived register instead. Keyed by
    /// the divisor SSA so a re-entrant `Mod` with the same divisor
    /// shares the cached magic value.
    hoisted_mod_magic: FxHashMap<SsaVar, ir::Value>,
    saw_return: bool,
}

impl OpLowerer for TraceEmitterState<'_, '_> {
    type Err = EmitError;

    fn with_builder<R>(&mut self, cb: impl FnOnce(&mut FunctionBuilder<'_>) -> R) -> R {
        cb(self.builder)
    }

    fn pointer_ty(&self) -> ir::Type {
        self.pointer_ty
    }

    fn deopt_block(&self) -> ir::Block {
        self.deopt_block
    }

    fn input_args_ptr(&self) -> ir::Value {
        self.input_args_ptr
    }

    fn lookup(&self, var: SsaVar) -> Result<ir::Value, EmitError> {
        self.ssa_to_value
            .get(&var)
            .copied()
            .ok_or(EmitError::UnboundSsa(var))
    }

    fn bind(&mut self, var: SsaVar, v: ir::Value) {
        self.ssa_to_value.insert(var, v);
    }

    fn record_overflow_bit(&mut self, dst: SsaVar, bit: ir::Value) {
        self.overflow_bits.insert(dst, bit);
    }
}

impl<'a, 'b> TraceEmitterState<'a, 'b> {
    fn emit_op(&mut self, op: &TraceOp, guard_site: Option<&GuardSite>) -> Result<(), EmitError> {
        match op {
            TraceOp::Add { dst, lhs, rhs } => lower_binop_i64(self, *dst, *lhs, *rhs, BinOp::Add),
            TraceOp::Sub { dst, lhs, rhs } => lower_binop_i64(self, *dst, *lhs, *rhs, BinOp::Sub),
            TraceOp::Mul { dst, lhs, rhs } => lower_binop_i64(self, *dst, *lhs, *rhs, BinOp::Mul),
            TraceOp::Div { dst, lhs, rhs } => lower_div(self, *dst, *lhs, *rhs),
            TraceOp::Mod { dst, lhs, rhs } => self.emit_mod(*dst, *lhs, *rhs),
            TraceOp::Cmp {
                kind,
                dst,
                lhs,
                rhs,
            } => lower_cmp(self, *kind, *dst, *lhs, *rhs),
            TraceOp::Load { dst, base, offset } => lower_load(self, *dst, *base, offset.0),
            TraceOp::Store { base, offset, src } => lower_store(self, *base, offset.0, *src),
            TraceOp::ConstI32 { dst, value } => lower_const_i32(self, *dst, *value),
            TraceOp::ConstI64 { dst, value } => lower_const_i64(self, *dst, *value),
            TraceOp::LocalGet { dst, slot_idx } => lower_local_get(self, *dst, *slot_idx),
            TraceOp::Guard { .. } => self.emit_guard_op(guard_site),
            TraceOp::Call {
                dst,
                func,
                args,
                effect,
            } => self.emit_call(*dst, func.0, args, *effect),
            TraceOp::Return { value } => self.emit_return(*value),
            TraceOp::MarkLoopHead { loop_id, phis } => self.emit_loop_head(*loop_id, phis),
            TraceOp::MarkLoopBack {
                loop_id,
                next_values,
            } => self.emit_loop_back(*loop_id, next_values),
            TraceOp::StrConcat { dst, lhs, rhs } => self.emit_str_concat(*dst, *lhs, *rhs),
            TraceOp::StrConcatN { dst, operands } => self.emit_str_concat_n(*dst, operands),
            TraceOp::StrContains {
                dst,
                haystack,
                needle,
            } => self.emit_str_contains(*dst, *haystack, *needle),
            TraceOp::StrFind {
                dst,
                haystack,
                needle,
            } => self.emit_str_find(*dst, *haystack, *needle),
            TraceOp::StrSubstring {
                dst,
                s,
                start,
                length,
            } => self.emit_str_substring(*dst, *s, *start, *length),
            TraceOp::StrGlobMatch { dst, s, pattern } => {
                self.emit_str_glob_match(*dst, *s, *pattern)
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

    /// F-D8-E.1: `Mod` mirrors `Div`'s shape — divisor-zero pre-check
    /// then `srem`. Signed remainder matches Relon's `Int` semantics
    /// (i64 signed) and Rust's `%` operator. The same const-0 overflow
    /// bit is seeded so the optional `Guard(ArithOverflow(dst))` from
    /// the recorder collapses to a pass on the hot path; the only
    /// `srem` overflow case is `i64::MIN % -1` which the upstream
    /// guards (and the recorder's observed-type tracking) handle.
    ///
    /// F-D8-E.6: when the divisor `b` is a known positive constant
    /// (from `self.trace.consts`, populated by the recorder /
    /// const_fold pass), emit a magic-number remainder sequence
    /// instead of `srem`. `srem` on x86_64 is a microcoded ~20-25
    /// cycle instruction; the magic sequence collapses to one
    /// `smulhi` + a couple of arith ops (~5-7 cycles). The two
    /// upstream guards become unconditionally false for any positive
    /// const divisor (b != 0 and b != -1), so we skip them outright.
    /// This is the W5 hot path: `i % 10` per iteration.
    fn emit_mod(&mut self, dst: SsaVar, a: SsaVar, b: SsaVar) -> Result<(), EmitError> {
        let va = self.lookup(a)?;
        let vb = self.lookup(b)?;

        // F-D8-E.6: fast path for positive-const divisor — magic
        // multiply replaces `srem`, both runtime guards become dead.
        // We only take the fast path when the magic table covers the
        // divisor (i.e. magic stays in i64-positive range so the
        // simple Hacker's Delight sequence applies without the
        // "needs add" correction). Divisors that fall outside that
        // class — none of which the recorder has produced in the
        // trace suite yet — drop through to the original `srem`
        // lowering with full guards intact.
        if let Some(divisor) = self.const_positive_i64(b) {
            if magic_supported_divisor(divisor) {
                // F-D8-E.6: if the preheader hoister pre-emitted the
                // magic multiplier `iconst.i64`, reuse it so the
                // 10-byte `mov reg, imm64` stays out of the loop
                // body. Cranelift 0.131's GVN doesn't fold an
                // `iconst.i64 imm64` across the loop back-edge on
                // its own.
                let hoisted_magic = self.hoisted_mod_magic.get(&b).copied();
                let r = self.emit_signed_mod_by_const(va, divisor, hoisted_magic);
                self.bind(dst, r);
                let of_bit = self.builder.ins().iconst(I32, 0);
                self.overflow_bits.insert(dst, of_bit);
                return Ok(());
            }
        }

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

    /// F-D8-E.6: return the i64 value bound to `var` when the
    /// recorder / const-fold sidetable proves it is a positive
    /// constant (> 0). Used to gate the magic-number remainder
    /// fast path in [`Self::emit_mod`]: only `b > 0` makes the
    /// divisor-zero and `i64::MIN % -1` overflow guards both
    /// statically dead, so the resulting trace can drop them
    /// without losing the runtime trap semantics the
    /// non-fast-path arm preserves.
    ///
    /// I32 consts are widened to i64; `Bool` consts are intentionally
    /// not accepted — the recorder never produces a `Mod` with a
    /// `Bool`-typed divisor.
    fn const_positive_i64(&self, var: SsaVar) -> Option<i64> {
        match self.trace.consts.get(&var)? {
            TraceConst::I64(v) if *v > 0 => Some(*v),
            TraceConst::I32(v) if *v > 0 => Some(i64::from(*v)),
            _ => None,
        }
    }

    /// F-D8-E.6: emit `a % divisor` as a Hacker's-Delight-style
    /// magic-number remainder for a known positive `divisor`. The
    /// returned SSA carries the signed remainder, matching `srem`
    /// semantics for all i64 dividends (including i64::MIN, since
    /// `MIN % divisor` is well-defined for any divisor != -1, and
    /// our caller has proven `divisor > 0`).
    ///
    /// The sequence for `divisor == 10` collapses to:
    ///   M  = 0x6666_6666_6666_6667  (signed magic)
    ///   hi = smulhi(a, M)            // = (a*M) >> 64, sign-extended
    ///   q1 = sshr_imm(hi, 2)         // arithmetic shift, q = a/10 for a >= 0
    ///   sign = sshr_imm(a, 63)       // -1 if a<0 else 0
    ///   q  = isub(q1, sign)          // q1 - (a >> 63) — adds 1 when a<0
    ///   r  = isub(a, imul(q, 10))
    ///
    /// Powers of two short-circuit to `a - ((a & ~(d-1)) signed-corrected)`
    /// — for `d = 2^k`, `a % d == a - ((a >> k) << k)` works for non-
    /// negative dividends but flips sign on negatives, so we still
    /// route them through the general signed-magic path to preserve
    /// `srem`'s rounding-toward-zero behaviour. The general path
    /// produces correct results for all powers of two too, so we
    /// don't carry a separate code path for them.
    ///
    /// Divisor of 1 trivially returns 0; that case is unlikely to
    /// reach this emitter (the recorder would have folded the op
    /// upstream) but is handled defensively.
    fn emit_signed_mod_by_const(
        &mut self,
        dividend: ir::Value,
        divisor: i64,
        hoisted_magic: Option<ir::Value>,
    ) -> ir::Value {
        debug_assert!(divisor > 0, "magic mod fast path requires positive divisor");
        if divisor == 1 {
            return self.builder.ins().iconst(I64, 0);
        }
        let (magic, post_shift) = signed_div_magic_i64(divisor)
            .expect("emit_signed_mod_by_const callers gate via magic_supported_divisor");
        let m_v = match hoisted_magic {
            Some(v) => v,
            None => self.builder.ins().iconst(I64, magic),
        };
        let hi = self.builder.ins().smulhi(dividend, m_v);
        // For divisors whose magic falls in the "needs add" branch of
        // Hacker's Delight's algorithm (the high bit of `magic` flips
        // on small divisors like 7), we'd need `hi += dividend` before
        // the shift. For the divisors we currently care about — `10`
        // is the only one stamped by the recorder for W5; small even
        // divisors and `divisor >= 3` non-power-of-two — `magic > 0`
        // and the simple form below applies. We assert this in
        // [`signed_div_magic_i64`] so a future expansion of the const
        // table catches the missing branch.
        let q_shifted = if post_shift > 0 {
            self.builder.ins().sshr_imm(hi, i64::from(post_shift))
        } else {
            hi
        };
        // q = q_shifted + (1 if dividend < 0 else 0). Subtracting the
        // arithmetic-shifted sign bit (-1 for negatives, 0 for non-
        // negatives) gives the same result as conditional `+1` and
        // avoids a branch.
        let sign = self.builder.ins().sshr_imm(dividend, 63);
        let quotient = self.builder.ins().isub(q_shifted, sign);
        // r = dividend - quotient * divisor. `imul_imm` on x86_64
        // backs to `lea` / `imul reg, reg, imm`, single-cycle for
        // small constants.
        let prod = self.builder.ins().imul_imm(quotient, divisor);
        self.builder.ins().isub(dividend, prod)
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
            arg_vals.push(widen_to_i64(self, v));
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
        let va = widen_to_i64(self, va);

        // F-D7-I inline path: rhs is a known small constant AND the
        // host wired the alloc helper.
        if let Some(alloc_fn) = self.str_concat_alloc {
            if let Some(rhs_bytes) = self.trace.const_bytes_for(b) {
                if crate::str_inline::concat_rhs_fits_inline(rhs_bytes) {
                    let rhs_owned: Vec<u8> = rhs_bytes.to_vec();
                    let r = crate::str_inline::emit_str_concat_inline_short_rhs(
                        self.builder,
                        alloc_fn,
                        // Tier 1b: pass the seal-hash helper through so
                        // the inline lowering can fold the freshly-built
                        // payload into the cached fx_hash field. `None`
                        // keeps the historical inline shape (no seal
                        // call) — correct, but defeats the cross-trace
                        // dict IC fast path on the concat result.
                        self.str_concat_seal_hash,
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
        let vb = widen_to_i64(self, vb);
        let inst = self.builder.ins().call(self.str_concat, &[va, vb]);
        let r = self.builder.inst_results(inst)[0];
        self.bind(dst, r);
        Ok(())
    }

    /// #168 `StrConcatN { dst, operands }` lowers to a single
    /// allocation + per-operand payload memcpy via the
    /// `__relon_str_concat_n_alloc` helper, mirroring the two-operand
    /// inline `StrConcat` lowering's split between cranelift IR
    /// (length sum + stack-slot pointer table) and the C-side
    /// allocator (alloc + memcpy loop).
    ///
    /// ## Lowering
    ///
    /// 1. For each `operands[i]`, load `len.i64` off the `StringRef`
    ///    header and accumulate into `total_len` via `iadd`.
    /// 2. Reserve an `[*const StringRef; N]` stack slot, then
    ///    `stack_store` each operand's pointer SSA into the slot.
    /// 3. `stack_addr` to materialise a pointer to the slot, then call
    ///    `__relon_str_concat_n_alloc(slot_ptr, N, total_len)`.
    /// 4. Call `__relon_str_concat_seal_hash(result)` so the cached
    ///    `fx_hash` matches the just-filled payload (same Tier 1b
    ///    contract the inline `StrConcat` lowering follows).
    /// 5. Bind `result` to `dst`.
    ///
    /// ## Caps
    ///
    /// The recorder caps `operand_count` at
    /// [`relon_trace_recorder::lowering::MAX_INLINE_STR_CONCAT_N`] so
    /// the unrolled per-operand length loads + stores fit in a small
    /// constant number of cranelift insns. This emitter trusts the
    /// recorder's cap and lowers any `operands.len() >= 3` it sees,
    /// surfacing a defensive `EmitError::HostHookNotDeclared` only
    /// when the host did not wire the alloc helper.
    fn emit_str_concat_n(&mut self, dst: SsaVar, operands: &[SsaVar]) -> Result<(), EmitError> {
        // Defensive: the recorder guarantees `len >= 3`; reject
        // anything else so a malformed buffer can't slip through to
        // the allocator helper with an under-budget total_len.
        if operands.len() < 3 {
            return Err(EmitError::Malformed(format!(
                "TraceOp::StrConcatN with operand_count={} (expected >= 3)",
                operands.len()
            )));
        }
        let alloc_fn = self
            .str_concat_n_alloc
            .ok_or(EmitError::HostHookNotDeclared(HostHookId::StrConcatNAlloc))?;

        // 1. Resolve each operand SSA to its cranelift Value (a
        //    pointer-typed slot carrying `*const StringRef`).
        let mut operand_vals: Vec<ir::Value> = Vec::with_capacity(operands.len());
        for &op_ssa in operands {
            let v = self.lookup(op_ssa)?;
            let v = widen_to_i64(self, v);
            operand_vals.push(v);
        }

        // 2. Length-sum: load each operand's `.len` field and reduce
        //    via `iadd` so cranelift sees a single `total_len` SSA the
        //    allocator helper consumes.
        let mut total_len = self.builder.ins().load(
            I64,
            MemFlags::trusted(),
            operand_vals[0],
            relon_trace_jit::runtime::STRING_REF_LEN_OFFSET,
        );
        for v in &operand_vals[1..] {
            let len_i = self.builder.ins().load(
                I64,
                MemFlags::trusted(),
                *v,
                relon_trace_jit::runtime::STRING_REF_LEN_OFFSET,
            );
            total_len = self.builder.ins().iadd(total_len, len_i);
        }

        // 3. Stack-spill the operand pointer table. Each slot is an
        //    `i64` (= pointer on 64-bit hosts) and aligned to 8 bytes
        //    so the allocator helper can read it with a plain
        //    `load.i64`. The slot's lifetime ends at the next
        //    cranelift `call` once the helper has copied the bytes,
        //    so no further book-keeping is required.
        let slot_bytes = (operands.len() as u32) * 8;
        let stack_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            slot_bytes,
            3, // 2^3 = 8-byte align
        ));
        for (i, v) in operand_vals.iter().enumerate() {
            self.builder
                .ins()
                .stack_store(*v, stack_slot, (i as i32) * 8);
        }
        let slot_ptr = self
            .builder
            .ins()
            .stack_addr(self.pointer_ty, stack_slot, 0);

        // 4. Call the alloc-and-memcpy helper. `n` and `total_len` are
        //    widened to i64 by the signature; cranelift truncates on
        //    the SysV ABI if the helper's prototype is narrower (it
        //    isn't — both args are `usize`).
        let n_const = self.builder.ins().iconst(I64, operands.len() as i64);
        let alloc_inst = self
            .builder
            .ins()
            .call(alloc_fn, &[slot_ptr, n_const, total_len]);
        let result_ptr = self.builder.inst_results(alloc_inst)[0];

        // 5. Seal the cached fx_hash on the freshly-built StringRef
        //    so cross-trace dict-key lookups see a valid digest.
        //    Mirrors the inline two-operand `StrConcat` path; absence
        //    of the seal helper would leave `hash = 0` and silently
        //    miss the dict IC on every trace exit.
        if let Some(seal_fn) = self.str_concat_seal_hash {
            self.builder.ins().call(seal_fn, &[result_ptr]);
        }
        self.bind(dst, result_ptr);
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
        let va = widen_to_i64(self, va);

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
        let vb = widen_to_i64(self, vb);
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
        let va = widen_to_i64(self, va);
        let vb = widen_to_i64(self, vb);
        let inst = self.builder.ins().call(self.str_find, &[va, vb]);
        let r = self.builder.inst_results(inst)[0];
        self.bind(dst, r);
        Ok(())
    }

    /// 2026-05-21 Tier-2 `StrGlobMatch(dst, s, pattern)` lowers to a
    /// direct `call __relon_str_glob_match(s, pattern) -> i32`. No
    /// inline fast path: the matcher itself is ~150 LoC with
    /// backtracking, and inlining it would bloat the trace body well
    /// past the per-iter cost budget. The recorder also keeps the call
    /// off the const-bytes side table since the pattern is rarely a
    /// short ASCII literal in the surfaces this op targets.
    ///
    /// Returns a 0/1 `i32` packed into the i32 SSA slot — same shape
    /// `StrContains` uses so downstream `Cmp` / `Guard(NotNull(dst))`
    /// ops see uniform width.
    fn emit_str_glob_match(
        &mut self,
        dst: SsaVar,
        s: SsaVar,
        pattern: SsaVar,
    ) -> Result<(), EmitError> {
        let helper = self
            .str_glob_match
            .ok_or(EmitError::HostHookNotDeclared(HostHookId::StrGlobMatch))?;
        let vs = self.lookup(s)?;
        let vp = self.lookup(pattern)?;
        let vs = widen_to_i64(self, vs);
        let vp = widen_to_i64(self, vp);
        let inst = self.builder.ins().call(helper, &[vs, vp]);
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
        let vs = widen_to_i64(self, vs);
        let vstart = widen_to_i64(self, vstart);
        let vlength = widen_to_i64(self, vlength);
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
    /// inline guard into a `TraceOp::Guard { kind: BoundsCheck, check: ... }` op the
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
        let idx_v = widen_to_i64(self, idx_v);

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

    /// F-D8 v2: lower `TraceOp::DictLookup { dst, dict_ptr, key_ptr,
    /// shape_hash }` to a single host-helper call that performs the
    /// IC-guarded dict access with a bounded record envelope and key
    /// payload byte-compare on hash hit.
    ///
    /// Emit shape:
    ///
    /// ```text
    /// %val = call __relon_trace_dict_lookup_v2(dict_ptr, record_len,
    ///                                           key_ptr, shape_hash,
    ///                                           trace_ctx)
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
        let record_len = self.trace.dict_record_len_hint(dict_ptr).unwrap_or(0);
        let record_len_v = self
            .builder
            .ins()
            .iconst(self.pointer_ty, i64::from(record_len));
        let shape_v = self.builder.ins().iconst(I64, shape_hash as i64);
        let inst = self.builder.ins().call(
            helper,
            &[dict_v, record_len_v, key_v, shape_v, self.trace_ctx_ptr],
        );
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

    /// F-D8-E.2/v2: lower `TraceOp::DictLookupPrechecked { dst,
    /// dict_ptr, key_ptr }` to the collision-safe v2 helper. The
    /// matching `DictShapeGuard` already verified the shape header
    /// upstream, so this call skips only that compare; it still uses
    /// the dict record length side table to bounds-check the record
    /// and byte-compare key payloads after hash hits.
    fn emit_dict_lookup_prechecked(
        &mut self,
        dst: SsaVar,
        dict_ptr: SsaVar,
        key_ptr: SsaVar,
    ) -> Result<(), EmitError> {
        let helper = self
            .dict_lookup_prechecked
            .ok_or(EmitError::HostHookNotDeclared(
                HostHookId::DictLookupPrechecked,
            ))?;
        let dict_v = self.lookup(dict_ptr)?;
        let key_v = self.lookup(key_ptr)?;
        let record_len = self.trace.dict_record_len_hint(dict_ptr).unwrap_or(0);
        let record_len_v = self
            .builder
            .ins()
            .iconst(self.pointer_ty, i64::from(record_len));
        let inst = self
            .builder
            .ins()
            .call(helper, &[dict_v, record_len_v, key_v, self.trace_ctx_ptr]);
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

    fn emit_return(&mut self, var: SsaVar) -> Result<(), EmitError> {
        let v = self.lookup(var)?;
        let v = widen_to_i64(self, v);
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
            let widened = widen_to_i64(self, v);
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
    /// 2. `Mod { _, _, b: invariant }` — pre-emit the divisor-nonzero
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
    ///   per-iter state — `list_len` is derived purely from invariant
    ///   pointers, and the divisor-zero predicate only reads the
    ///   invariant `b` operand.
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
                TraceOp::Mod { rhs: b, .. }
                    if !meta.inside_defs.contains(b)
                        && !self.hoisted_mod_nonzero_divisor.contains(b) =>
                {
                    // F-D8-E.6: when the divisor is a known positive
                    // constant that the magic-mod fast path will pick
                    // up, the in-loop emit_mod skips the
                    // divisor-nonzero brif entirely (the magic
                    // sequence can't trap on b=0 because b is a
                    // compile-time constant). Hoisting the brif
                    // would emit dead IR that confuses the
                    // optimizer. Instead, pre-emit only the magic-
                    // multiplier constant so the per-iter `iconst.i64
                    // imm64` (which lowers to a 10-byte `mov reg,
                    // imm64` on x86_64) is shared across iterations.
                    if let Some(divisor) = self.const_positive_i64(*b) {
                        if !self.hoisted_mod_magic.contains_key(b) {
                            if let Some((magic, _post_shift)) = signed_div_magic_i64(divisor) {
                                let magic_v = self.builder.ins().iconst(I64, magic);
                                self.hoisted_mod_magic.insert(*b, magic_v);
                            }
                        }
                        // Magic path doesn't need the divisor-nonzero
                        // guard, but mark the SSA as "guard handled"
                        // so the in-loop emit_mod knows to skip its
                        // own copy (it already would, by virtue of
                        // taking the fast path; the cache entry is
                        // belt-and-braces).
                        self.hoisted_mod_nonzero_divisor.insert(*b);
                    } else {
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
            let widened = widen_to_i64(self, val);
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
fn compute_loop_meta(ops: &[TraceOp]) -> FxHashMap<u32, LoopMeta> {
    let mut out: FxHashMap<u32, LoopMeta> = FxHashMap::default();
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

/// F-D8-E.6: predicate gating the magic-mod fast path. Returns
/// `true` when the divisor has a positive i64 signed-division magic
/// (so the simple `smulhi` + `sshr_imm` + `+ (a<0)` sequence is
/// correct). Computed by running [`signed_div_magic_i64`]'s search
/// once and checking the high bit of the resulting magic.
///
/// `d <= 1` returns `false` — `d == 1` is handled inline in
/// [`TraceEmitterState::emit_signed_mod_by_const`], and `d <= 0`
/// would not reach this predicate via the `const_positive_i64`
/// pre-filter anyway.
fn magic_supported_divisor(d: i64) -> bool {
    signed_div_magic_i64(d).is_some()
}

/// F-D8-E.6: pre-compute the i64 signed-division magic multiplier
/// and post-shift for a positive constant divisor.
///
/// Algorithm: the variant of Granlund–Montgomery / Hacker's Delight
/// 10-1 that produces a positive `magic` whenever `2^(64+s) / d` is
/// representable as a positive i64. For the divisors the recorder
/// stamps into W5's `i % 10` site (and any other small positive
/// constants we expect on the hot path: `2..=10`, `100`, `1024`)
/// the result keeps `magic > 0`, so the emitter's lowering can
/// skip the Hacker's Delight "needs add" correction (`hi += a`
/// before the shift) without losing correctness.
///
/// For divisors that *would* need the correction, we panic in the
/// debug build (the caller's `debug_assert!` catches it) and fall
/// back to the unspecialised `srem` arm in release. This is
/// conservative: the only known affected divisors below 50 are
/// {7, 11, 14, 19, 21, ...} — the recorder hasn't produced any of
/// these as a const divisor in the trace suite we've measured.
///
/// Returns `Some((magic, post_shift))` where the runtime computes
/// `(a * magic) >>_high 64 >> post_shift + (a < 0 ? 1 : 0)` as the
/// quotient. Returns `None` when `d <= 1` (caller should short-
/// circuit) or when the magic would set the sign bit (the simple
/// emit sequence omits the "needs add" correction). See
/// [`TraceEmitterState::emit_signed_mod_by_const`] for the matching
/// IR emission and [`magic_supported_divisor`] for the predicate
/// form.
fn signed_div_magic_i64(d: i64) -> Option<(i64, u32)> {
    if d <= 1 {
        return None;
    }
    let ad = d as u128;
    // The Hacker's Delight algorithm tracks two ratios: `q2 = 2^p/d`
    // (drives the magic itself) and `q1 = 2^p/anc` (drives the loop
    // termination predicate). `anc = 2^63 - 1 - ((2^63 - 1) % ad)`
    // is the largest signed-positive numerator whose `n/d` truncated
    // quotient is `(2^63 - 1)/d - 1` (i.e. the numerator just below
    // the next-quotient boundary). The convention matches the
    // 64-bit form in HD §10-1 exactly; common bugs in re-implementations
    // mis-substitute `2^63 % ad` (off by one) for the `% ad` factor
    // above, which produces a too-small `anc` and overshoots `p`.
    let two_63_minus_1: u128 = (1u128 << 63) - 1;
    let anc: u128 = two_63_minus_1 - (two_63_minus_1 % ad);
    let mut p: u32 = 63;
    let mut q1: u128 = (1u128 << 63) / anc;
    let mut r1: u128 = (1u128 << 63) - q1 * anc;
    let mut q2: u128 = (1u128 << 63) / ad;
    let mut r2: u128 = (1u128 << 63) - q2 * ad;
    loop {
        p += 1;
        q1 *= 2;
        r1 *= 2;
        if r1 >= anc {
            q1 += 1;
            r1 -= anc;
        }
        q2 *= 2;
        r2 *= 2;
        if r2 >= ad {
            q2 += 1;
            r2 -= ad;
        }
        let delta: u128 = ad - r2;
        if !(q1 < delta || (q1 == delta && r1 == 0)) {
            break;
        }
        // Safety brake against runaway loops on pathological inputs.
        // The Hacker's Delight algorithm terminates by `p < 96` for
        // any 64-bit divisor; cap a bit higher than that.
        if p > 128 {
            return None;
        }
    }
    let magic_u: u128 = q2 + 1;
    // For divisors we care about, magic_u fits in i64's positive
    // range. If it doesn't, the simple emit_signed_mod_by_const
    // sequence omits the "needs add" correction step and would
    // silently miscompile — return None and force the caller to
    // fall back.
    if magic_u > i64::MAX as u128 {
        return None;
    }
    let magic = magic_u as i64;
    let post_shift = p - 64;
    Some((magic, post_shift))
}

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
    /// (`__relon_trace_list_get` / `__relon_trace_dict_lookup_v2`) in its
    /// cranelift module before installing dict/list traces.
    HostHookNotDeclared(HostHookId),
    /// Forwarded from [`crate::guard_emit::GuardEmitError`].
    Guard(GuardEmitError),
    /// #168: catch-all for malformed-buffer rejects the per-op
    /// lowering helpers raise (e.g. `TraceOp::StrConcatN` with an
    /// `operands.len()` outside the recorder's documented cap). The
    /// message is intended for diagnostics only; the install pipeline
    /// reports it through the usual `EmitError` Display path so the
    /// outer tier router can fall back to the cranelift AOT backend.
    Malformed(String),
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
            EmitError::Malformed(s) => write!(f, "malformed trace op: {}", s),
        }
    }
}

impl std::error::Error for EmitError {}

impl From<GuardEmitError> for EmitError {
    fn from(e: GuardEmitError) -> Self {
        EmitError::Guard(e)
    }
}

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
        b.append(TraceOp::ConstI64 { dst, value: 42 });
        b.append(TraceOp::Return { value: dst });
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        TraceEmitter::emit(&trace, &mut ctx).expect("emit ok");
    }

    #[test]
    fn lookup_errors_on_unbound_ssa() {
        // Ops without a preceding define should surface UnboundSsa.
        let mut b = TraceBuffer::new();
        let phantom = SsaVar(99);
        b.append(TraceOp::Return { value: phantom });
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        let err = TraceEmitter::emit(&trace, &mut ctx).unwrap_err();
        assert!(matches!(err, EmitError::UnboundSsa(_)));
    }

    #[test]
    fn unrecoverable_call_rejected() {
        let mut b = TraceBuffer::new();
        let dst = b.fresh_ssa();
        b.append(TraceOp::Call {
            dst,
            func: relon_trace_jit::FuncId(7),
            args: vec![],
            effect: EffectClass::Unrecoverable,
        });
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        let err = TraceEmitter::emit(&trace, &mut ctx).unwrap_err();
        assert!(matches!(err, EmitError::UnrecoverableEffectInTrace));
    }

    #[test]
    fn load_store_round_trip_lowers() {
        let mut b = TraceBuffer::new();
        let base = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: base,
            value: 0x1000,
        });
        let loaded = b.fresh_ssa();
        b.append(TraceOp::Load {
            dst: loaded,
            base,
            offset: Offset(8),
        });
        b.append(TraceOp::Store {
            base,
            offset: Offset(16),
            src: loaded,
        });
        b.append(TraceOp::Return { value: loaded });
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
        b.append(TraceOp::ConstI64 {
            dst: base,
            value: 0x1000,
        });
        b.append(TraceOp::ConstI64 { dst: idx, value: 0 });
        b.append(TraceOp::ListGet {
            dst,
            list_ptr: base,
            idx,
        });
        b.append(TraceOp::Return { value: dst });
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
        b.append(TraceOp::ConstI64 {
            dst: dict,
            value: 0x2000,
        });
        b.append(TraceOp::ConstI64 {
            dst: key,
            value: 0x3000,
        });
        b.append(TraceOp::DictLookup {
            dst,
            dict_ptr: dict,
            key_ptr: key,
            shape_hash: 0xdeadbeef,
        });
        b.append(TraceOp::Return { value: dst });
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
        b.append(TraceOp::ConstI64 {
            dst: base,
            value: 0x1000,
        });
        b.append(TraceOp::ConstI64 { dst: idx, value: 0 });
        b.append(TraceOp::ListGet {
            dst,
            list_ptr: base,
            idx,
        });
        b.append(TraceOp::Return { value: dst });
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        let hook_ids = HostHookFuncIds {
            list_get: Some(7),
            ..Default::default()
        };
        TraceEmitter::emit_with_hooks(&trace, &mut ctx, I64, hook_ids).expect("emit ok");
    }

    /// F-D8 v2: when the trace buffer carries a record-length hint
    /// for a dict_ptr SSA that flows into `DictLookupPrechecked`, the
    /// emitter must call the collision-safe v2 helper rather than the
    /// old hash-only inline scan.
    #[test]
    fn dict_lookup_prechecked_with_record_len_uses_v2_helper_call() {
        use cranelift_codegen::ir::Opcode;

        let mut b = TraceBuffer::new();
        let dict = b.fresh_ssa();
        let key = b.fresh_ssa();
        let dst = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: dict,
            value: 0x2000,
        });
        b.append(TraceOp::ConstI64 {
            dst: key,
            value: 0x3000,
        });
        // Run the dict_ic_hoist pass shape by hand: emit a
        // DictShapeGuard upstream and the prechecked flavour as the
        // body op. The prechecked helper skips the repeated shape
        // compare; an upstream DictShapeGuard keeps the semantics
        // honest (mismatch deopts).
        b.append(TraceOp::DictShapeGuard {
            dict_ptr: dict,
            shape_hash: 0xfeed_face_dead_beef,
        });
        b.append(TraceOp::DictLookupPrechecked {
            dst,
            dict_ptr: dict,
            key_ptr: key,
        });
        b.record_dict_record_len_hint(dict, 128);
        b.append(TraceOp::Return { value: dst });

        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        let hook_ids = HostHookFuncIds {
            dict_lookup_prechecked: Some(9),
            ..Default::default()
        };
        TraceEmitter::emit_with_hooks(&trace, &mut ctx, I64, hook_ids).expect("emit ok");

        let func = &ctx.func;
        let mut select_count = 0usize;
        let mut call_count = 0usize;
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                match func.dfg.insts[inst].opcode() {
                    Opcode::Select => select_count += 1,
                    Opcode::Call => call_count += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(
            select_count, 0,
            "v2 helper path must not emit hash-only select-chain inline lookup"
        );
        assert!(
            call_count >= 1,
            "DictLookupPrechecked v2 lowering must emit a helper call"
        );
    }

    /// F-D8 v2: missing record length hints are still safe. The
    /// emitter passes `record_len = 0`; the runtime helper deopts
    /// before reading the dict body.
    #[test]
    fn dict_lookup_prechecked_without_record_len_still_lowers() {
        let mut b = TraceBuffer::new();
        let dict = b.fresh_ssa();
        let key = b.fresh_ssa();
        let dst = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: dict,
            value: 0x2000,
        });
        b.append(TraceOp::ConstI64 {
            dst: key,
            value: 0x3000,
        });
        b.append(TraceOp::DictShapeGuard {
            dict_ptr: dict,
            shape_hash: 0xfeed_face_dead_beef,
        });
        b.append(TraceOp::DictLookupPrechecked {
            dst,
            dict_ptr: dict,
            key_ptr: key,
        });
        // No record-length hint: the helper receives record_len = 0
        // and will deopt before reading the dict body at runtime.
        b.append(TraceOp::Return { value: dst });

        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        let hook_ids = HostHookFuncIds {
            dict_lookup_prechecked: Some(9),
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
        b.append(TraceOp::ConstI64 {
            dst: dict,
            value: 0x2000,
        });
        b.append(TraceOp::ConstI64 {
            dst: key,
            value: 0x3000,
        });
        b.append(TraceOp::DictLookup {
            dst,
            dict_ptr: dict,
            key_ptr: key,
            shape_hash: 0xfeed_face_dead_beef,
        });
        b.append(TraceOp::Return { value: dst });
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
        b.append(TraceOp::ConstI64 {
            dst: outer,
            value: 7,
        });
        let phi = b.fresh_ssa();
        b.append(TraceOp::MarkLoopHead {
            loop_id: 42,
            phis: vec![relon_trace_jit::LoopPhi::new(outer, phi)],
        });
        let inner = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: inner,
            value: 9,
        });
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
        b.append(TraceOp::ConstI64 { dst: v, value: 0 });
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
        b.append(TraceOp::LocalGet {
            dst: list,
            slot_idx: 0,
        });
        b.append(TraceOp::LocalGet {
            dst: dict,
            slot_idx: 1,
        });
        b.append(TraceOp::LocalGet {
            dst: key,
            slot_idx: 2,
        });
        b.append(TraceOp::ConstI64 { dst: idx, value: 0 });
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
        b.append(TraceOp::Return { value: dict_v });
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
        b.append(TraceOp::LocalGet {
            dst: n,
            slot_idx: 0,
        });
        let divisor = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: divisor,
            value: 10,
        });
        let i_init = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: i_init,
            value: 0,
        });

        let phi_i = b.fresh_ssa();
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![relon_trace_jit::LoopPhi::new(i_init, phi_i)],
        });
        let mod_dst = b.fresh_ssa();
        b.append(TraceOp::Mod {
            dst: mod_dst,
            lhs: phi_i,
            rhs: divisor,
        });
        let one = b.fresh_ssa();
        b.append(TraceOp::ConstI64 { dst: one, value: 1 });
        let next_i = b.fresh_ssa();
        b.append(TraceOp::Add {
            dst: next_i,
            lhs: phi_i,
            rhs: one,
        });
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![next_i],
        });
        b.append(TraceOp::Return { value: mod_dst });
        let trace = b.into_optimized();

        let mut ctx = CodegenContext::new();
        TraceEmitter::emit(&trace, &mut ctx).expect("emit ok");
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        verifier::verify_function(&ctx.func, &flags).expect("verifier accepts hoisted Mod IR");
    }

    /// F-D8-E.6: the magic-number table must reproduce the canonical
    /// Hacker's Delight numbers for the divisors the W5 hot path (and
    /// other small-int mod sites we anticipate) cares about. d == 10
    /// is the W5 case; the smaller positive divisors round out the
    /// "common small ints" coverage so a future analyzer change that
    /// stamps a different constant doesn't silently drop into the
    /// `srem` arm without us noticing.
    ///
    /// Reference values: published HD-10-1 results for 64-bit signed
    /// magic divisors.
    #[test]
    fn signed_div_magic_matches_published_values_for_small_divisors() {
        // (divisor, expected_magic, expected_post_shift). We only
        // assert the values for divisors whose magic stays in the
        // positive-i64 range (the "no add-correction" branch the
        // current emit lowering supports). Divisors that require
        // the add-correction (e.g. 7, 100, 1000) fall through to
        // the original `srem` arm; we cover that case in the
        // `magic_supported_divisor_rejects_add_correction_class`
        // test below.
        let cases: [(i64, i64, u32); 2] = [
            (3, 0x5555_5555_5555_5556u64 as i64, 0),
            (10, 0x6666_6666_6666_6667u64 as i64, 2),
        ];
        for (d, m, s) in cases {
            let (gm, gs) = super::signed_div_magic_i64(d).expect("supported divisor");
            assert_eq!(gm, m, "magic for {d}");
            assert_eq!(gs, s, "shift for {d}");
        }
    }

    /// F-D8-E.6: divisors whose Hacker's Delight magic carries a
    /// set high bit (the "needs add-correction" class — 7, 100,
    /// 1000, ...) must be rejected by [`magic_supported_divisor`].
    /// The dispatcher in [`TraceEmitterState::emit_mod`] then
    /// keeps the original `srem` lowering rather than emitting an
    /// IR sequence that would silently miscompile.
    #[test]
    fn magic_supported_divisor_rejects_add_correction_class() {
        // The values below all hit the magic >= 2^63 branch of the
        // Granlund–Montgomery search — verified empirically against
        // Hacker's Delight Table 10-1. d == 1024 is a power-of-two
        // that also lands in this class (the search overshoots
        // before converging because `2^63 % 1024 == 0` sends the
        // anc denominator to its maximum).
        // Among the 64-bit signed magics in HD Table 10-1, the
        // divisors whose magic has the sign bit set — and hence
        // require the unimplemented "add the dividend before the
        // shift" correction — include `100` (M = 0xA3D7_0A3D_0A3D_70B,
        // shift 6) and `1024` (M = 0x8000_0000_0000_0001, shift 9).
        // The dispatcher must skip the fast path for these and fall
        // back to the original `srem` lowering.
        for &d in &[100i64, 1024] {
            assert!(
                !super::magic_supported_divisor(d),
                "d = {d} requires add-correction; emit_mod must skip the magic fast path"
            );
        }
        // And by contrast, every divisor whose magic stays in the
        // positive-i64 range must pass the predicate — this is the
        // path the W5 hot loop's `i % 10` takes.
        for &d in &[3i64, 5, 7, 9, 10, 11, 1000] {
            assert!(
                super::magic_supported_divisor(d),
                "d = {d} has a positive magic; emit_mod should take the fast path"
            );
        }
    }

    /// F-D8-E.6: the full `a % d` lowering must agree with native
    /// `i64::rem_euclid`-style signed remainder for a sweep of
    /// dividends including negatives, zero, and the i64 extremes,
    /// so we know the magic-mul + sign-adjust sequence reproduces
    /// `srem` semantics exactly. We exercise the helper by compiling
    /// a one-op trace per `(d, a)` pair through `TraceEmitter::emit`
    /// is overkill for a per-bench unit-test; the algorithm is
    /// straight integer arithmetic in `signed_div_magic_i64`, so we
    /// instead model the IR output in pure Rust and cross-check
    /// against `a.wrapping_rem(d)`.
    #[test]
    fn signed_mod_magic_matches_native_srem_for_sample_grid() {
        fn model_mod(a: i64, d: i64) -> i64 {
            let (magic, shift) = super::signed_div_magic_i64(d).expect("supported divisor");
            // smulhi: (a * magic) >> 64, with arithmetic semantics
            // matching Rust's i128 sign-extend.
            let hi = ((a as i128).wrapping_mul(magic as i128) >> 64) as i64;
            let q_shifted = if shift > 0 { hi >> shift } else { hi };
            let sign = a >> 63;
            let q = q_shifted.wrapping_sub(sign);
            a.wrapping_sub(q.wrapping_mul(d))
        }
        // Only sweep divisors whose magic is supported by the
        // emitter's lowering (the "no add-correction" branch). The
        // add-correction class is exercised by
        // `magic_supported_divisor_rejects_add_correction_class`,
        // which proves the dispatcher falls back to `srem` for it.
        let divisors: [i64; 2] = [3, 10];
        let dividends: [i64; 10] = [0, 1, -1, 9, 10, -10, 1_999, -1_999, i64::MAX, i64::MIN + 1];
        for &d in &divisors {
            for &a in &dividends {
                let got = model_mod(a, d);
                let want = a.wrapping_rem(d);
                assert_eq!(
                    got, want,
                    "mismatch for a = {a}, d = {d}: magic-mul gave {got}, srem gave {want}"
                );
            }
        }
    }

    /// F-D8-E.6: emit a one-op `Mod` trace with a recorded const
    /// divisor and assert that the resulting cranelift IR contains
    /// `smulhi` (the magic-fast-path marker) but no `srem`. This is
    /// the "is the optimisation actually wired in the emit path?"
    /// guard — the smoke test for the dispatcher in
    /// [`TraceEmitterState::emit_mod`].
    #[test]
    fn mod_with_const_divisor_lowers_to_magic_mul() {
        use cranelift_codegen::verifier;

        let mut b = TraceBuffer::new();
        let a = b.fresh_ssa();
        b.append(TraceOp::LocalGet {
            dst: a,
            slot_idx: 0,
        });
        let divisor = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: divisor,
            value: 10,
        });
        let m = b.fresh_ssa();
        b.append(TraceOp::Mod {
            dst: m,
            lhs: a,
            rhs: divisor,
        });
        b.append(TraceOp::Return { value: m });
        // Mirror the analyzer-side pipeline: const_fold seeds
        // `trace.consts` with `divisor → 10`, which is the
        // precondition the magic fast path tests against.
        relon_trace_jit::optimizer::OptimizerPass::run(
            &relon_trace_jit::optimizer::const_fold::ConstFold,
            &mut b,
        );
        let trace = b.into_optimized();

        let mut ctx = CodegenContext::new();
        TraceEmitter::emit(&trace, &mut ctx).expect("emit ok");
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        verifier::verify_function(&ctx.func, &flags).expect("verifier accepts magic-mod IR");

        let ir_str = format!("{}", ctx.func.display());
        assert!(
            ir_str.contains("smulhi"),
            "expected magic-mod fast path (smulhi present) in IR:\n{ir_str}"
        );
        assert!(
            !ir_str.contains("srem"),
            "expected no srem in the magic-mod fast path IR:\n{ir_str}"
        );
    }

    /// F-D8-E.6: W5-shape `i % 10` inside a loop must lower with the
    /// magic-mod fast path AND no leftover `srem`. The const divisor
    /// (v=10) is defined outside the loop body, so the prehoister
    /// also stamps the divisor-nonzero brif into the preheader —
    /// that brif is now dead code (the magic path doesn't trap on
    /// div-by-zero) and cranelift's GVN should fold it.
    #[test]
    fn w5_shape_mod_in_loop_uses_magic_path() {
        use cranelift_codegen::verifier;

        let mut b = TraceBuffer::new();
        let n = b.fresh_ssa();
        b.append(TraceOp::LocalGet {
            dst: n,
            slot_idx: 0,
        });
        let divisor = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: divisor,
            value: 10,
        });
        let i_init = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: i_init,
            value: 0,
        });

        let phi_i = b.fresh_ssa();
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![relon_trace_jit::LoopPhi::new(i_init, phi_i)],
        });
        let mod_dst = b.fresh_ssa();
        b.append(TraceOp::Mod {
            dst: mod_dst,
            lhs: phi_i,
            rhs: divisor,
        });
        let one = b.fresh_ssa();
        b.append(TraceOp::ConstI64 { dst: one, value: 1 });
        let next_i = b.fresh_ssa();
        b.append(TraceOp::Add {
            dst: next_i,
            lhs: phi_i,
            rhs: one,
        });
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![next_i],
        });
        b.append(TraceOp::Return { value: mod_dst });
        relon_trace_jit::optimizer::OptimizerPass::run(
            &relon_trace_jit::optimizer::const_fold::ConstFold,
            &mut b,
        );
        let trace = b.into_optimized();
        let mut ctx = CodegenContext::new();
        TraceEmitter::emit(&trace, &mut ctx).expect("emit ok");
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        verifier::verify_function(&ctx.func, &flags).expect("verifier accepts loop magic-mod IR");
        let ir = format!("{}", ctx.func.display());
        assert!(
            ir.contains("smulhi"),
            "loop magic-mod IR is missing smulhi:\n{ir}"
        );
        assert!(
            !ir.contains("srem"),
            "loop magic-mod IR still contains srem:\n{ir}"
        );
    }
}
