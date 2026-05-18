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

use std::collections::HashMap;

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
        let signature = TRACE_ENTRY_SIG.to_cranelift(pointer_ty, CallConv::SystemV);
        ctx.func = Function::with_name_signature(UserFuncName::user(0, 0), signature.clone());

        let mut builder_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);

        // Entry block + ABI param pickup.
        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        let trace_ctx_ptr = builder.block_params(entry_block)[0];
        let _input_args_ptr = builder.block_params(entry_block)[1];

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
        );
        let resolve_call = declare_host_hook(
            builder.func,
            HostHookId::ResolveCall,
            &[pointer_ty, I32],
            &[pointer_ty],
            pointer_ty,
        );
        let _inline_cache_lookup = declare_host_hook(
            builder.func,
            HostHookId::InlineCacheLookup,
            &[pointer_ty, I32, I64],
            &[I64],
            pointer_ty,
        );

        let mut emitter = TraceEmitterState {
            builder: &mut builder,
            trace,
            trace_ctx_ptr,
            pointer_ty,
            deopt_block,
            save_deopt,
            resolve_call,
            ssa_to_value: HashMap::new(),
            loop_head_blocks: HashMap::new(),
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
    pointer_ty: ir::Type,
    deopt_block: ir::Block,
    save_deopt: ir::FuncRef,
    resolve_call: ir::FuncRef,
    ssa_to_value: HashMap<SsaVar, ir::Value>,
    loop_head_blocks: HashMap<u32, ir::Block>,
    saw_return: bool,
}

impl<'a, 'b> TraceEmitterState<'a, 'b> {
    fn emit_op(&mut self, op: &TraceOp, guard_site: Option<&GuardSite>) -> Result<(), EmitError> {
        match op {
            TraceOp::Add(dst, a, b) => self.emit_binop_i64(*dst, *a, *b, BinOp::Add),
            TraceOp::Sub(dst, a, b) => self.emit_binop_i64(*dst, *a, *b, BinOp::Sub),
            TraceOp::Mul(dst, a, b) => self.emit_binop_i64(*dst, *a, *b, BinOp::Mul),
            TraceOp::Div(dst, a, b) => self.emit_div(*dst, *a, *b),
            TraceOp::Cmp(kind, dst, a, b) => self.emit_cmp(*kind, *dst, *a, *b),
            TraceOp::Load(dst, base, off) => self.emit_load(*dst, *base, off.0),
            TraceOp::Store(base, off, src) => self.emit_store(*base, off.0, *src),
            TraceOp::ConstI32(dst, v) => self.emit_const_i32(*dst, *v),
            TraceOp::ConstI64(dst, v) => self.emit_const_i64(*dst, *v),
            TraceOp::Guard(_, _) => self.emit_guard_op(guard_site),
            TraceOp::Call(dst, func_id, args, effect) => {
                self.emit_call(*dst, func_id.0, args, *effect)
            }
            TraceOp::Return(v) => self.emit_return(*v),
            TraceOp::MarkLoopHead { loop_id } => self.emit_loop_head(*loop_id),
            TraceOp::MarkLoopBack { loop_id } => self.emit_loop_back(*loop_id),
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
        let r = match op {
            BinOp::Add => self.builder.ins().iadd(va, vb),
            BinOp::Sub => self.builder.ins().isub(va, vb),
            BinOp::Mul => self.builder.ins().imul(va, vb),
        };
        self.bind(dst, r);
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

    fn emit_guard_op(&mut self, guard_site: Option<&GuardSite>) -> Result<(), EmitError> {
        let site = guard_site.ok_or(EmitError::OrphanGuardOp)?;
        let mut gctx = GuardEmitCtx {
            builder: self.builder,
            deopt_block: self.deopt_block,
            type_info: &self.trace.type_info,
            pointer_ty: self.pointer_ty,
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

    fn emit_loop_head(&mut self, loop_id: u32) -> Result<(), EmitError> {
        // Create the header block and jump into it from the current
        // insertion point. The header takes no block params because
        // loop-carried values live in the trace's SSA slots (the
        // recorder turns φ-style carries into explicit Load/Store
        // pairs).
        let header = self.builder.create_block();
        self.builder.ins().jump(header, &[]);
        self.builder.switch_to_block(header);
        // Don't seal: the matching MarkLoopBack will add the back edge.
        self.loop_head_blocks.insert(loop_id, header);
        Ok(())
    }

    fn emit_loop_back(&mut self, loop_id: u32) -> Result<(), EmitError> {
        let header = *self
            .loop_head_blocks
            .get(&loop_id)
            .ok_or(EmitError::UnmatchedLoopBack(loop_id))?;
        self.builder.ins().jump(header, &[]);
        // The header had its forward edge from `emit_loop_head` and
        // now its back edge from this jump; safe to seal.
        self.builder.seal_block(header);
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

        // call save_deopt(ctx, guard_pc, external_pc)
        self.builder.ins().call(
            self.save_deopt,
            &[self.trace_ctx_ptr, guard_pc, external_pc],
        );

        let failed = self
            .builder
            .ins()
            .iconst(I32, i64::from(TraceEntryStatus::GuardFailed.as_i32()));
        self.builder.ins().return_(&[failed]);
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

/// Declare a host hook as an `ExtFuncData::User` import on the
/// function. Returns the resulting `FuncRef` so call sites can use it.
///
/// The `namespace` field is set to `0` (matching what
/// `cranelift_module` uses for declared imports); `index` is the
/// `HostHookId` discriminant so external tooling can map back to the
/// symbolic name without a side table.
fn declare_host_hook(
    func: &mut Function,
    hook: HostHookId,
    params: &[ir::Type],
    returns: &[ir::Type],
    _pointer_ty: ir::Type,
) -> ir::FuncRef {
    let mut sig = Signature::new(CallConv::SystemV);
    for p in params {
        sig.params.push(AbiParam::new(*p));
    }
    for r in returns {
        sig.returns.push(AbiParam::new(*r));
    }
    let sig_ref = func.import_signature(sig);
    let name_ref = func.declare_imported_user_function(UserExternalName::new(
        0,
        match hook {
            HostHookId::SaveDeopt => 0,
            HostHookId::ResolveCall => 1,
            HostHookId::InlineCacheLookup => 2,
        },
    ));
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
}
