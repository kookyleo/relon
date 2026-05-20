//! v6-ε-0-A — at-call-site trace IR inline emission.
//!
//! Where [`crate::emitter::TraceEmitter`] builds a self-contained
//! trace entry function (its own `Function` value, ABI param pickup,
//! `return` for status), `inline_emit` splices the same trace ops
//! into an **existing** [`cranelift_frontend::FunctionBuilder`]
//! belonging to a host function. The two paths share the per-op
//! lowering rules; only the prologue / epilogue / deopt termination
//! differ.
//!
//! ## Mental model
//!
//! Today (v6-δ M2-C / v6-ε-0-C): the host fn emits `call trace_fn_ptr`
//! → cranelift JIT-emitted trace entry → trace body → `ret` to host.
//!
//! After this module ships: the host fn's builder calls
//! [`emit_trace_inline`] at the call-site block. The function:
//!
//! 1. Walks the trace's [`OptimizedTrace`] ops in order, lowering each
//!    one into the supplied builder using the same rules
//!    [`crate::emitter::TraceEmitter`] uses for a stand-alone entry fn.
//! 2. Replaces `TraceOp::Return(v)` (the trace's terminator) with a
//!    `jump` to a caller-supplied **post-block** carrying the resulting
//!    `i64` value as a single block-param. Control flow then continues
//!    inside the host fn — no `call/ret` pair, no `extern "C"` arg
//!    marshall.
//! 3. Routes guard fires to a caller-supplied **deopt-block** with
//!    `(guard_pc: i32, external_pc: i64)` block args. The host fn
//!    decides what to do (call `__relon_trace_save_deopt`, dispatch
//!    to the bytecode VM, return a sentinel, …).
//!
//! ## Trace size cap
//!
//! Inlining a 4 KB trace into every host call site bloats the host
//! fn's machine code and stretches the regalloc footprint. We refuse
//! to inline traces with more than [`MAX_INLINE_OPS`] ops; the host
//! fn falls back to the regular `call trace_fn_ptr` path
//! ([`crate::emitter::TraceEmitter::emit_with_hooks_and_call_conv`])
//! when [`should_inline_trace`] returns `false`.
//!
//! 256 is the figure documented in the v6-ε plan §3 ε-0-A "size cap";
//! revisit when corpus traces grow large enough to bump it.
//!
//! ## Why we don't reuse the standalone emitter directly
//!
//! `TraceEmitter::emit_with_hooks_and_call_conv` writes into a freshly
//! reset `ctx.func`, overwriting its name + signature, and terminates
//! every path with a `return` instruction. Reusing it would require
//! the host fn to live in a *separate* `Function` and then patch the
//! two together via `cranelift_module` — there's no stable API for
//! that in cranelift 0.131. Instead `inline_emit` is a focused
//! re-implementation of the per-op lowering against an arbitrary
//! [`FunctionBuilder`], no entry block / ABI param creation, no
//! `return`. The lowering rules themselves are line-for-line copies
//! of `crate::emitter::TraceEmitterState`'s; keeping them in sync is
//! a sync-check on a stage commit (see the smoke test
//! `inline_matches_standalone_result` in
//! `crates/relon-codegen-native/tests/trace_jit_inline_smoke.rs`).

use std::collections::HashMap;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{self, BlockArg, InstBuilder, MemFlags};
use cranelift_frontend::FunctionBuilder;

use relon_trace_jit::{EffectClass, GuardSite, OptimizedTrace, SsaVar, TraceOp};

use crate::guard_emit::{emit_guard, GuardEmitCtx, GuardEmitError};

/// Maximum number of [`TraceOp`]s an inlined trace may contain. Above
/// this threshold the host fn falls back to the regular trampoline
/// call (`emit_with_hooks_and_call_conv` path) to keep host-fn code
/// bloat bounded.
///
/// The figure (256) matches v6-ε-guard-hoist-plan.md §3 ε-0-A.
pub const MAX_INLINE_OPS: usize = 256;

/// Cheap pre-check: should the trace be inlined or trampoline-called?
///
/// Today only the op-count cap is consulted. Future revisions may
/// add guard-density or call-density limits.
pub fn should_inline_trace(trace: &OptimizedTrace) -> bool {
    trace.op_count() <= MAX_INLINE_OPS
}

/// Things that can go wrong while inline-emitting a trace.
#[derive(Debug)]
pub enum InlineEmitError {
    /// Op references an SSA var the inline emitter never bound.
    UnboundSsa(SsaVar),
    /// `Guard(...)` op in the stream but no matching [`GuardSite`].
    OrphanGuardOp,
    /// `Call(...)` op marked [`EffectClass::Unrecoverable`]. Same
    /// invariant as the standalone emitter — recorder must abort
    /// rather than commit such a trace.
    UnrecoverableEffectInTrace,
    /// `MarkLoopBack` op with no preceding matching `MarkLoopHead`.
    UnmatchedLoopBack(u32),
    /// Trace exceeds [`MAX_INLINE_OPS`]; caller must fall back to the
    /// regular trampoline-call path.
    TraceTooLarge { op_count: usize, cap: usize },
    /// Inline emit reached the end of the op stream without seeing
    /// `TraceOp::Return`. Well-formed traces always end in `Return`;
    /// the inline path can't synthesise a tail jump without knowing
    /// the post-block's i64 block-arg shape, so we surface the
    /// recorder bug instead of silently jumping with an undef value.
    MissingReturn,
    /// Forwarded from [`crate::guard_emit::GuardEmitError`].
    Guard(GuardEmitError),
    /// `TraceOp::Call(..)` op in the stream. The inline path doesn't
    /// support recursive `Call` lowering yet (the call helper lookup
    /// would need a per-host-fn FuncRef on the resolve_call extern,
    /// which the inline path can't synthesise without coupling to the
    /// cranelift module pipeline). Caller should fall back to the
    /// trampoline-call path.
    CallNotSupportedInInline,
}

impl std::fmt::Display for InlineEmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InlineEmitError::UnboundSsa(v) => write!(f, "inline op references unbound SSA {v:?}"),
            InlineEmitError::OrphanGuardOp => {
                write!(f, "inline Guard op has no matching GuardSite")
            }
            InlineEmitError::UnrecoverableEffectInTrace => write!(
                f,
                "inline Call op marked Unrecoverable cannot live in a recorded trace"
            ),
            InlineEmitError::UnmatchedLoopBack(id) => {
                write!(f, "inline MarkLoopBack {{ loop_id: {id} }} has no head")
            }
            InlineEmitError::TraceTooLarge { op_count, cap } => write!(
                f,
                "inline rejected: trace has {op_count} ops, cap is {cap}; fall back to trampoline"
            ),
            InlineEmitError::MissingReturn => {
                write!(f, "inline op stream ended without a TraceOp::Return")
            }
            InlineEmitError::Guard(e) => write!(f, "inline guard emit failure: {e}"),
            InlineEmitError::CallNotSupportedInInline => write!(
                f,
                "inline emit rejects TraceOp::Call; fall back to trampoline-call path"
            ),
        }
    }
}

impl std::error::Error for InlineEmitError {}

/// External handles the host fn must supply when inlining a trace.
///
/// Unlike the standalone emitter (which owns its own deopt block, its
/// own `return` instruction and its own `result_slot` store), the
/// inline path expects the host fn to hand the following bundle in:
///
/// * `trace_ctx_ptr` — value carrying `*mut TraceContext`. The inline
///   trace stores to `ctx.result_slot` (legacy compat) **and** also
///   jumps to `post_block` with the i64 result as a block arg so the
///   host fn never has to re-load from memory in the hot path.
/// * `input_args_ptr` — value carrying the packed `u64[]` the trace's
///   `LocalGet` ops index off.
/// * `post_block` — block to jump to on the successful `Return` path.
///   Must already have a single `I64` block param appended; the inline
///   emitter passes the trace's return value as that param.
/// * `deopt_block` — block to jump to on guard fire. Must already have
///   two block params appended: `(guard_pc: I32, external_pc: I64)`,
///   matching the standalone emitter's deopt-block shape.
pub struct InlineEmitHandles {
    pub trace_ctx_ptr: ir::Value,
    pub input_args_ptr: ir::Value,
    pub post_block: ir::Block,
    pub deopt_block: ir::Block,
}

/// Inline-emit the supplied [`OptimizedTrace`] into the host
/// function's `builder` at its current insertion point.
///
/// On success, the builder is positioned at a freshly-created dummy
/// block past `post_block`; callers that have more host-fn code after
/// the inline trace should `switch_to_block(post_block)` to continue.
///
/// Returns [`InlineEmitError::TraceTooLarge`] for traces above
/// [`MAX_INLINE_OPS`]; callers must check [`should_inline_trace`]
/// before calling, or be prepared to fall back to the trampoline-call
/// path on that error.
pub fn emit_trace_inline(
    builder: &mut FunctionBuilder<'_>,
    trace: &OptimizedTrace,
    pointer_ty: ir::Type,
    handles: InlineEmitHandles,
) -> Result<(), InlineEmitError> {
    if trace.op_count() > MAX_INLINE_OPS {
        return Err(InlineEmitError::TraceTooLarge {
            op_count: trace.op_count(),
            cap: MAX_INLINE_OPS,
        });
    }

    let mut emitter = InlineEmitterState {
        builder,
        trace,
        trace_ctx_ptr: handles.trace_ctx_ptr,
        input_args_ptr: handles.input_args_ptr,
        pointer_ty,
        post_block: handles.post_block,
        deopt_block: handles.deopt_block,
        ssa_to_value: HashMap::new(),
        overflow_bits: HashMap::new(),
        loop_head_blocks: HashMap::new(),
        saw_return: false,
    };

    // Index guards by `trace_pc` so the per-op walk can pick the
    // matching site without scanning the guard vector each time.
    let guards_by_pc: HashMap<u32, &GuardSite> =
        trace.guards.iter().map(|g| (g.trace_pc, g)).collect();

    for (pc, op) in trace.ops.iter().enumerate() {
        let guard_site = if op.is_guard() {
            guards_by_pc.get(&(pc as u32)).copied()
        } else {
            None
        };
        emitter.emit_op(op, guard_site)?;
    }

    if !emitter.saw_return {
        return Err(InlineEmitError::MissingReturn);
    }

    Ok(())
}

/// Per-function emit state for the inline path. Mirrors
/// [`crate::emitter::TraceEmitterState`] but writes into an external
/// `FunctionBuilder` and terminates the `Return` op with a jump to
/// `post_block` instead of a `return` instruction.
struct InlineEmitterState<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    trace: &'a OptimizedTrace,
    trace_ctx_ptr: ir::Value,
    input_args_ptr: ir::Value,
    pointer_ty: ir::Type,
    post_block: ir::Block,
    deopt_block: ir::Block,
    ssa_to_value: HashMap<SsaVar, ir::Value>,
    overflow_bits: HashMap<SsaVar, ir::Value>,
    loop_head_blocks: HashMap<u32, ir::Block>,
    saw_return: bool,
}

impl<'a, 'b> InlineEmitterState<'a, 'b> {
    fn emit_op(
        &mut self,
        op: &TraceOp,
        guard_site: Option<&GuardSite>,
    ) -> Result<(), InlineEmitError> {
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
            TraceOp::Call(_, _, _, effect) => {
                if matches!(effect, EffectClass::Unrecoverable) {
                    return Err(InlineEmitError::UnrecoverableEffectInTrace);
                }
                // v6-ε-0-A: inline path does NOT support `TraceOp::Call`
                // yet. Calls require a resolve-host-fn step that goes
                // through the host hook table; threading that through
                // the inline path means re-deriving the FuncRef to the
                // resolve_call helper inside the host fn's module,
                // which is currently a per-trace-module concern. The
                // recorder produces straight-line traces today
                // (Phase-1 envelope: arith / cmp / load / store / If),
                // so traces with `Call` ops aren't in the bench. When
                // they appear, return an error and let the caller fall
                // back to the trampoline-call path.
                Err(InlineEmitError::CallNotSupportedInInline)
            }
            TraceOp::Return(v) => self.emit_return(*v),
            TraceOp::MarkLoopHead { loop_id, phis } => self.emit_loop_head(*loop_id, phis),
            TraceOp::MarkLoopBack {
                loop_id,
                next_values,
            } => self.emit_loop_back(*loop_id, next_values),
            // F-D7: string ops follow the same rationale as Call —
            // they need a host-function FuncRef that the inline path
            // can't derive without going through the per-trace-module
            // import machinery. Surface the same fallback error so the
            // caller routes the trace through the regular emitter.
            TraceOp::StrConcat(_, _, _)
            | TraceOp::StrContains(_, _, _)
            | TraceOp::StrFind(_, _, _)
            | TraceOp::StrSubstring(_, _, _, _) => Err(InlineEmitError::CallNotSupportedInInline),
            // F-D8: inline emit doesn't yet thread the dict/list host
            // helpers through the surrounding host-fn cranelift module
            // (same reason `TraceOp::Call` returns `CallNotSupported`).
            // The inline path is a perf shortcut for tiny straight-line
            // traces; dict/list traces always go through the standalone
            // emitter where the resolve_call FuncRef is in scope.
            //
            // F-D8-E.2: `DictShapeGuard` is technically inline-only
            // (no host helper call) and could be supported here, but
            // it only appears in trace streams that ALSO carry a
            // `DictLookupPrechecked` host call further down. Keeping
            // the conservative bail-out preserves the historical
            // "no dict/list ops in inline path" invariant; the bench
            // and production paths route through the standalone
            // emitter anyway.
            TraceOp::ListGet { .. }
            | TraceOp::DictLookup { .. }
            | TraceOp::DictShapeGuard { .. }
            | TraceOp::DictLookupPrechecked { .. } => {
                Err(InlineEmitError::CallNotSupportedInInline)
            }
        }
    }

    fn emit_binop_i64(
        &mut self,
        dst: SsaVar,
        a: SsaVar,
        b: SsaVar,
        op: BinOp,
    ) -> Result<(), InlineEmitError> {
        let va = self.lookup(a)?;
        let vb = self.lookup(b)?;
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

    fn emit_div(&mut self, dst: SsaVar, a: SsaVar, b: SsaVar) -> Result<(), InlineEmitError> {
        let va = self.lookup(a)?;
        let vb = self.lookup(b)?;
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

    /// F-D8-E.1: `Mod` mirrors `emit_div` — divisor-zero pre-check
    /// + `srem`. Signed remainder, matches Relon `Int` semantics.
    fn emit_mod(&mut self, dst: SsaVar, a: SsaVar, b: SsaVar) -> Result<(), InlineEmitError> {
        let va = self.lookup(a)?;
        let vb = self.lookup(b)?;
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
        let r = self.builder.ins().srem(va, vb);
        self.bind(dst, r);
        Ok(())
    }

    fn emit_cmp(
        &mut self,
        kind: relon_trace_jit::CmpKind,
        dst: SsaVar,
        a: SsaVar,
        b: SsaVar,
    ) -> Result<(), InlineEmitError> {
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

    fn emit_load(&mut self, dst: SsaVar, base: SsaVar, off: i32) -> Result<(), InlineEmitError> {
        let base_v = self.lookup(base)?;
        let r = self
            .builder
            .ins()
            .load(I64, MemFlags::trusted(), base_v, off);
        self.bind(dst, r);
        Ok(())
    }

    fn emit_store(&mut self, base: SsaVar, off: i32, src: SsaVar) -> Result<(), InlineEmitError> {
        let base_v = self.lookup(base)?;
        let src_v = self.lookup(src)?;
        let src_v = self.widen_to_i64(src_v);
        self.builder
            .ins()
            .store(MemFlags::trusted(), src_v, base_v, off);
        Ok(())
    }

    fn emit_const_i32(&mut self, dst: SsaVar, v: i32) -> Result<(), InlineEmitError> {
        let val = self.builder.ins().iconst(I32, i64::from(v));
        self.bind(dst, val);
        Ok(())
    }

    fn emit_const_i64(&mut self, dst: SsaVar, v: i64) -> Result<(), InlineEmitError> {
        let val = self.builder.ins().iconst(I64, v);
        self.bind(dst, val);
        Ok(())
    }

    fn emit_local_get(&mut self, dst: SsaVar, slot_idx: u32) -> Result<(), InlineEmitError> {
        let off = (slot_idx as i64).wrapping_mul(8);
        let off_i32 = i32::try_from(off).unwrap_or(0);
        let v = self
            .builder
            .ins()
            .load(I64, MemFlags::trusted(), self.input_args_ptr, off_i32);
        self.bind(dst, v);
        Ok(())
    }

    fn emit_guard_op(&mut self, guard_site: Option<&GuardSite>) -> Result<(), InlineEmitError> {
        let site = guard_site.ok_or(InlineEmitError::OrphanGuardOp)?;
        let mut gctx = GuardEmitCtx {
            builder: self.builder,
            deopt_block: self.deopt_block,
            type_info: &self.trace.type_info,
            pointer_ty: self.pointer_ty,
            overflow_bits: &self.overflow_bits,
        };
        emit_guard(&mut gctx, site, &self.ssa_to_value).map_err(InlineEmitError::Guard)
    }

    fn emit_return(&mut self, var: SsaVar) -> Result<(), InlineEmitError> {
        let v = self.lookup(var)?;
        let v = self.widen_to_i64(v);
        // Legacy compat: keep writing the value into `ctx.result_slot`
        // so any host code that reads it (telemetry, deopt fallback)
        // still observes the canonical location. Cranelift's later
        // passes can eliminate this store when the host fn never
        // re-reads ctx.result_slot — but for v6-ε-0-A we keep it for
        // parity with the standalone emitter.
        self.builder.ins().store(
            MemFlags::trusted(),
            v,
            self.trace_ctx_ptr,
            crate::abi::result_slot_offset(),
        );
        // Hot path: jump to the host-supplied post-block carrying the
        // i64 result. No `return` — control stays inside the host fn.
        self.builder
            .ins()
            .jump(self.post_block, &[BlockArg::Value(v)]);
        // Switch to a fresh dummy block so any trailing ops have
        // somewhere to land. cranelift's dead-block elim drops it.
        let dummy = self.builder.create_block();
        self.builder.seal_block(dummy);
        self.builder.switch_to_block(dummy);
        self.saw_return = true;
        Ok(())
    }

    fn emit_loop_head(
        &mut self,
        loop_id: u32,
        phis: &[relon_trace_jit::LoopPhi],
    ) -> Result<(), InlineEmitError> {
        let header = self.builder.create_block();

        let mut init_vals: Vec<ir::Value> = Vec::with_capacity(phis.len());
        for phi in phis {
            let v = self.lookup(phi.init)?;
            let widened = self.widen_to_i64(v);
            init_vals.push(widened);
        }
        for phi in phis {
            let bp = self.builder.append_block_param(header, I64);
            self.bind(phi.phi, bp);
        }

        let init_args: Vec<BlockArg> = init_vals.iter().map(|v| BlockArg::Value(*v)).collect();
        self.builder.ins().jump(header, &init_args);
        self.builder.switch_to_block(header);
        self.loop_head_blocks.insert(loop_id, header);
        Ok(())
    }

    fn emit_loop_back(
        &mut self,
        loop_id: u32,
        next_values: &[SsaVar],
    ) -> Result<(), InlineEmitError> {
        let header = *self
            .loop_head_blocks
            .get(&loop_id)
            .ok_or(InlineEmitError::UnmatchedLoopBack(loop_id))?;
        let mut args_vals: Vec<ir::Value> = Vec::with_capacity(next_values.len());
        for v in next_values {
            let val = self.lookup(*v)?;
            let widened = self.widen_to_i64(val);
            args_vals.push(widened);
        }
        let args: Vec<BlockArg> = args_vals.iter().map(|v| BlockArg::Value(*v)).collect();
        self.builder.ins().jump(header, &args);
        self.builder.seal_block(header);
        let after = self.builder.create_block();
        self.builder.seal_block(after);
        self.builder.switch_to_block(after);
        Ok(())
    }

    fn lookup(&self, var: SsaVar) -> Result<ir::Value, InlineEmitError> {
        self.ssa_to_value
            .get(&var)
            .copied()
            .ok_or(InlineEmitError::UnboundSsa(var))
    }

    fn bind(&mut self, var: SsaVar, v: ir::Value) {
        self.ssa_to_value.insert(var, v);
    }

    fn widen_to_i64(&mut self, v: ir::Value) -> ir::Value {
        let ty = self.builder.func.dfg.value_type(v);
        if ty == I64 {
            v
        } else if ty == I32 {
            self.builder.ins().uextend(I64, v)
        } else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::{Function, UserFuncName};
    use cranelift_codegen::isa::CallConv;
    use cranelift_codegen::settings;
    use cranelift_codegen::verifier::verify_function;
    use cranelift_codegen::Context as CodegenContext;
    use cranelift_frontend::FunctionBuilderContext;
    use relon_trace_jit::{Offset, TraceBuffer};

    /// Build a minimal host fn signature
    /// `fn(ctx: *mut TraceContext, args: *const u64) -> i64` and emit a
    /// hand-rolled inline trace into it. Returns the generated cranelift
    /// IR text so the test can assert structural properties.
    fn host_fn_with_inline_trace(trace: OptimizedTrace) -> Result<String, InlineEmitError> {
        let pointer_ty = I64;
        let mut ctx = CodegenContext::new();
        let mut sig = ir::Signature::new(CallConv::SystemV);
        sig.params.push(ir::AbiParam::new(pointer_ty));
        sig.params.push(ir::AbiParam::new(pointer_ty));
        sig.returns.push(ir::AbiParam::new(I64));
        ctx.func = Function::with_name_signature(UserFuncName::user(0, 0), sig);

        let mut builder_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);

        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);
        let trace_ctx_ptr = builder.block_params(entry_block)[0];
        let input_args_ptr = builder.block_params(entry_block)[1];

        let post_block = builder.create_block();
        builder.append_block_param(post_block, I64);
        let deopt_block = builder.create_block();
        builder.append_block_param(deopt_block, I32);
        builder.append_block_param(deopt_block, I64);

        emit_trace_inline(
            &mut builder,
            &trace,
            pointer_ty,
            InlineEmitHandles {
                trace_ctx_ptr,
                input_args_ptr,
                post_block,
                deopt_block,
            },
        )?;

        // Post-block: return the inline trace's result.
        builder.switch_to_block(post_block);
        builder.seal_block(post_block);
        let result = builder.block_params(post_block)[0];
        builder.ins().return_(&[result]);

        // Deopt-block: return sentinel `-1` to the caller.
        builder.switch_to_block(deopt_block);
        builder.seal_block(deopt_block);
        let _guard_pc = builder.block_params(deopt_block)[0];
        let _external_pc = builder.block_params(deopt_block)[1];
        let sentinel = builder.ins().iconst(I64, -1);
        builder.ins().return_(&[sentinel]);

        builder.finalize();
        // Verify the resulting IR before returning so test failures
        // surface as a clear "inline emit produced malformed IR" rather
        // than a cranelift codegen panic later.
        let flags = settings::Flags::new(settings::builder());
        verify_function(&ctx.func, &flags).expect("inline-emitted IR must verify");
        Ok(format!("{}", ctx.func.display()))
    }

    #[test]
    fn inline_emit_const_return() {
        let mut b = TraceBuffer::new();
        let dst = b.fresh_ssa();
        b.append(TraceOp::ConstI64(dst, 42));
        b.append(TraceOp::Return(dst));
        let trace = b.into_optimized();
        let ir = host_fn_with_inline_trace(trace).expect("inline emit");
        // The result-slot store survives the inline path so deopt
        // fallback paths can pick up the value if needed.
        assert!(
            ir.contains("store"),
            "inline trace must store result to ctx.result_slot"
        );
        // The post-block jump must carry the i64 result as a block arg.
        assert!(ir.contains("jump"), "inline must terminate with a jump");
    }

    #[test]
    fn inline_emit_add_local_get() {
        let mut b = TraceBuffer::new();
        let a = b.fresh_ssa();
        let bb = b.fresh_ssa();
        let sum = b.fresh_ssa();
        b.append(TraceOp::LocalGet(a, 0));
        b.append(TraceOp::LocalGet(bb, 1));
        b.append(TraceOp::Add(sum, a, bb));
        b.append(TraceOp::Return(sum));
        let trace = b.into_optimized();
        let ir = host_fn_with_inline_trace(trace).expect("inline emit");
        // Both LocalGet ops lower to loads off args_ptr.
        assert!(
            ir.matches("load").count() >= 2,
            "two LocalGet ops must produce two loads"
        );
        // sadd_overflow + jump to post block.
        assert!(
            ir.contains("sadd_overflow"),
            "Add op must emit sadd_overflow"
        );
    }

    #[test]
    fn inline_emit_load_store_round_trip() {
        let mut b = TraceBuffer::new();
        let base = b.fresh_ssa();
        b.append(TraceOp::ConstI64(base, 0x1000));
        let loaded = b.fresh_ssa();
        b.append(TraceOp::Load(loaded, base, Offset(8)));
        b.append(TraceOp::Store(base, Offset(16), loaded));
        b.append(TraceOp::Return(loaded));
        let trace = b.into_optimized();
        let _ = host_fn_with_inline_trace(trace).expect("inline emit");
    }

    #[test]
    fn inline_rejects_oversized_trace() {
        // Build a trace with MAX_INLINE_OPS + 1 ops (each a no-op ConstI64).
        let mut b = TraceBuffer::new();
        let mut last = b.fresh_ssa();
        b.append(TraceOp::ConstI64(last, 0));
        for _ in 0..MAX_INLINE_OPS {
            last = b.fresh_ssa();
            b.append(TraceOp::ConstI64(last, 0));
        }
        b.append(TraceOp::Return(last));
        let trace = b.into_optimized();
        assert!(!should_inline_trace(&trace));
        let err = host_fn_with_inline_trace(trace).expect_err("must reject");
        assert!(matches!(err, InlineEmitError::TraceTooLarge { .. }));
    }

    #[test]
    fn inline_emit_missing_return_errors() {
        let mut b = TraceBuffer::new();
        let dst = b.fresh_ssa();
        b.append(TraceOp::ConstI64(dst, 42));
        // No Return — should surface MissingReturn.
        let trace = b.into_optimized();
        let err = host_fn_with_inline_trace(trace).expect_err("must error");
        assert!(matches!(err, InlineEmitError::MissingReturn));
    }
}
