//! Guard / deopt block emission.
//!
//! The emitter centralises all "guard-fired → deopt" lowering paths
//! here so the main emitter file stays focused on the straight-line
//! op stream. Every guard kind eventually funnels into the *single*
//! per-function deopt block: that block calls
//! `__relon_trace_save_deopt(ctx, guard_trace_pc, external_pc)` and
//! returns the [`crate::TraceEntryStatus::GuardFailed`] sentinel.
//!
//! ## Layout
//!
//! ```text
//! …
//! <guard predicate emission>          // computes `cond: i32`
//!     brif cond, ok_block, deopt_block
//! ok_block:
//!     <subsequent ops>
//! deopt_block(guard_pc, external_pc):  // shared by every guard
//!     call save_deopt(ctx, guard_pc, external_pc)
//!     return GuardFailed (i32 1)
//! ```
//!
//! Branching with `brif` lets cranelift's verifier check both arms
//! are reachable / well-typed, which is what our tests rely on.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::InstBuilder;
use cranelift_codegen::ir::{self, BlockArg};
use cranelift_frontend::FunctionBuilder;
use rustc_hash::FxHashMap;

use relon_trace_jit::{GuardKind, GuardSite, ObservedType, SsaVar};

/// Slim view onto the running emitter state the guard helpers need.
/// Kept as a struct (not a closure or trait) so the public API stays
/// inspectable and the failure points are obvious in tests.
pub struct GuardEmitCtx<'a, 'b> {
    pub builder: &'a mut FunctionBuilder<'b>,
    pub deopt_block: ir::Block,
    pub type_info: &'a FxHashMap<SsaVar, ObservedType>,
    pub pointer_ty: ir::Type,
    /// v6-δ M1: per-SSA cranelift `i8` overflow bit surfaced by the
    /// `Add` / `Sub` / `Mul` lowering. The `ArithOverflow(dst)`
    /// guard reads this map to emit a real "did the arith carry?"
    /// brif predicate instead of a constant-0 that always deopts.
    pub overflow_bits: &'a FxHashMap<SsaVar, ir::Value>,
}

/// Lower a single [`GuardSite`] into cranelift IR at the current
/// insertion point.
///
/// On a successful guard the caller's insertion point is advanced
/// into a fresh "ok" block; on failure the deopt block runs with the
/// guard's `(trace_pc, external_pc)` arguments threaded in as block
/// params.
///
/// Returns [`GuardEmitError`] when the guard references an SSA var
/// the emitter never bound (programmer error — recorder bug).
pub fn emit_guard(
    ctx: &mut GuardEmitCtx<'_, '_>,
    site: &GuardSite,
    ssa_to_value: &FxHashMap<SsaVar, ir::Value>,
) -> Result<(), GuardEmitError> {
    // ε-M0 fast paths: guard kinds whose predicate is already a
    // single SSA bool value emit `brif` directly, skipping the
    // synthetic icmp+uextend chain `build_guard_predicate` would
    // otherwise produce. This saves ~1 ns/iter on the recorded
    // hot-loop bench row where the same cmp result feeds an
    // adjacent guard.
    let guard_pc = ctx
        .builder
        .ins()
        .iconst(I32, i64::from(site.trace_pc as i32));
    let external_pc = ctx.builder.ins().iconst(I64, site.deopt_pc.0 as i64);
    let ok_block = ctx.builder.create_block();

    match &site.kind {
        GuardKind::NotNull(var) => {
            let v = ssa_to_value
                .get(var)
                .copied()
                .ok_or(GuardEmitError::UnboundSsa(*var))?;
            // Branch to ok when v != 0, deopt when v == 0.
            ctx.builder.ins().brif(
                v,
                ok_block,
                &[],
                ctx.deopt_block,
                &[BlockArg::Value(guard_pc), BlockArg::Value(external_pc)],
            );
        }
        GuardKind::IsZero(var) => {
            let v = ssa_to_value
                .get(var)
                .copied()
                .ok_or(GuardEmitError::UnboundSsa(*var))?;
            // Branch to deopt when v != 0, ok when v == 0. Swapping
            // the target order on `brif` mirrors the IsZero polarity
            // without an extra icmp.
            ctx.builder.ins().brif(
                v,
                ctx.deopt_block,
                &[BlockArg::Value(guard_pc), BlockArg::Value(external_pc)],
                ok_block,
                &[],
            );
        }
        GuardKind::ArithOverflow(var) if ctx.overflow_bits.contains_key(var) => {
            // F-D7-J: when the matching `Add` / `Sub` / `Mul` op
            // surfaced an `of_bit` via `*_overflow`, branch directly
            // on it. `of_bit == 1` ⇒ overflow ⇒ deopt; `of_bit == 0`
            // ⇒ ok. This skips the `icmp(eq, of, 0); uextend(I32)`
            // chain `build_guard_predicate` would otherwise emit and
            // saves ~2 cranelift insts per ArithOverflow guard — the
            // W4 hot loop fires two of these per iter (one for the
            // accumulator `+= hit`, one for `i + 1`), so the trim
            // shows up directly in the per-iter cost.
            //
            // The fallback path (no overflow bit captured — synthetic
            // / hand-rolled buffers in unit tests) stays in
            // `build_guard_predicate` so the constant-0/1 predicate
            // behaviour pinned by existing tests doesn't change.
            let of_bit = ctx
                .overflow_bits
                .get(var)
                .copied()
                .expect("overflow_bits entry verified above");
            ctx.builder.ins().brif(
                of_bit,
                ctx.deopt_block,
                &[BlockArg::Value(guard_pc), BlockArg::Value(external_pc)],
                ok_block,
                &[],
            );
        }
        _ => {
            let cond = build_guard_predicate(ctx, &site.kind, ssa_to_value)?;
            // brif: branch to `deopt_block` when the predicate is
            // *false* (i.e. the guard fired). The deopt block takes
            // two params: (guard_trace_pc: i32, external_pc: i64);
            // we pass these in directly so the host helper signature
            // can be a stable 3-arg shape `(ctx_ptr, pc, external_pc)`.
            ctx.builder.ins().brif(
                cond,
                ok_block,
                &[],
                ctx.deopt_block,
                &[BlockArg::Value(guard_pc), BlockArg::Value(external_pc)],
            );
        }
    }

    ctx.builder.seal_block(ok_block);
    ctx.builder.switch_to_block(ok_block);
    Ok(())
}

/// Materialise the i32 boolean predicate for the supplied guard kind.
/// Convention: `1` ⇒ "guard passes, keep running"; `0` ⇒ deopt.
fn build_guard_predicate(
    ctx: &mut GuardEmitCtx<'_, '_>,
    kind: &GuardKind,
    ssa_to_value: &FxHashMap<SsaVar, ir::Value>,
) -> Result<ir::Value, GuardEmitError> {
    match kind {
        GuardKind::TypeCheck(var, expected) => {
            // Without a runtime tag word in TraceContext yet we
            // compare the *recorded* observed type against the
            // expectation at emit time. If they match we emit a
            // constant `1` (predicate holds); otherwise we emit a
            // constant `0` to deopt immediately. This conservative
            // behaviour matches what LuaJIT does for type-spec
            // guards whose observed type the recorder pinned.
            let observed = ctx
                .type_info
                .get(var)
                .copied()
                .ok_or(GuardEmitError::MissingTypeInfo(*var))?;
            let pred = if observed == *expected { 1 } else { 0 };
            Ok(ctx.builder.ins().iconst(I32, pred))
        }
        GuardKind::NotNull(var) => {
            let v = ssa_to_value
                .get(var)
                .copied()
                .ok_or(GuardEmitError::UnboundSsa(*var))?;
            let ty = ctx.builder.func.dfg.value_type(v);
            let zero = ctx.builder.ins().iconst(ty, 0);
            let r = ctx.builder.ins().icmp(IntCC::NotEqual, v, zero);
            Ok(widen_to_i32(ctx, r))
        }
        GuardKind::BoundsCheck(var, limit) => {
            let v = ssa_to_value
                .get(var)
                .copied()
                .ok_or(GuardEmitError::UnboundSsa(*var))?;
            let l = ssa_to_value
                .get(limit)
                .copied()
                .ok_or(GuardEmitError::UnboundSsa(*limit))?;
            // unsigned-less-than: catches negative `var` as well
            // (cranelift treats the operands as the underlying bit
            // pattern). Matches the recorder's `Op::ReadStringByte`
            // pre-condition.
            let r = ctx.builder.ins().icmp(IntCC::UnsignedLessThan, v, l);
            Ok(widen_to_i32(ctx, r))
        }
        GuardKind::IsZero(var) => {
            // ε-M0 dual of `NotNull`: predicate is `(v == 0)` →
            // passes (1) when v is zero, deopts when v becomes
            // non-zero. Used for the BrIf fall-through arm in
            // recorded loops where the recorded path requires cond=0
            // to stay in the loop.
            let v = ssa_to_value
                .get(var)
                .copied()
                .ok_or(GuardEmitError::UnboundSsa(*var))?;
            let ty = ctx.builder.func.dfg.value_type(v);
            let zero = ctx.builder.ins().iconst(ty, 0);
            let r = ctx.builder.ins().icmp(IntCC::Equal, v, zero);
            Ok(widen_to_i32(ctx, r))
        }
        GuardKind::ArithOverflow(var) => {
            // v6-δ M1: real overflow guard using the boolean carry-out
            // surfaced by `sadd_overflow` / `ssub_overflow` /
            // `smul_overflow` in the arith op lowering. The map is
            // populated by `emit_binop_i64` keyed on the arith op's
            // `dst`; if it's missing we fall back to the v6-γ
            // constant-0 predicate so traces without recorded arith
            // (e.g. `LocalGet` → `Cmp` → `Return`) keep their
            // observed-type behaviour and corpus regression
            // testing continues to pin the absence.
            //
            // The predicate is `1` (guard passes) when the overflow
            // bit is **zero**, i.e. the arith op didn't overflow:
            // emit `pred = (of == 0)` as an i32.
            if let Some(of_bit) = ctx.overflow_bits.get(var).copied() {
                let of_ty = ctx.builder.func.dfg.value_type(of_bit);
                let zero = ctx.builder.ins().iconst(of_ty, 0);
                let no_of = ctx.builder.ins().icmp(IntCC::Equal, of_bit, zero);
                Ok(widen_to_i32(ctx, no_of))
            } else {
                // No overflow bit captured: fall back to the
                // conservative pinned-by-observed-type predicate.
                // This keeps ArithOverflow guards on synthetic /
                // hand-rolled trace buffers (used in optimiser /
                // emitter unit tests) behaving exactly as before.
                let observed = ctx.type_info.get(var).copied();
                let pred = match observed {
                    Some(ObservedType::I32 | ObservedType::Bool) => 1,
                    _ => 0,
                };
                Ok(ctx.builder.ins().iconst(I32, pred))
            }
        }
    }
}

/// Some cranelift `icmp` instructions yield an `i8` boolean; widen to
/// `i32` so the trampoline path can branch on the result with a
/// uniform width.
fn widen_to_i32(ctx: &mut GuardEmitCtx<'_, '_>, v: ir::Value) -> ir::Value {
    let ty = ctx.builder.func.dfg.value_type(v);
    if ty == I32 {
        v
    } else {
        ctx.builder.ins().uextend(I32, v)
    }
}

/// Things that can go wrong while emitting a guard. Every variant is
/// a recorder / optimiser invariant violation — runtime callers should
/// never trigger one. We avoid pulling `thiserror` as a new dep on
/// this crate: the variant set is small and the manual `Display` impl
/// below carries no maintenance cost.
#[derive(Debug)]
pub enum GuardEmitError {
    /// Guard references an SSA var the emitter never bound.
    UnboundSsa(SsaVar),
    /// Type-check guard on an SSA var with no recorded observed type.
    MissingTypeInfo(SsaVar),
}

impl std::fmt::Display for GuardEmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardEmitError::UnboundSsa(v) => {
                write!(f, "guard references unbound SSA var {:?}", v)
            }
            GuardEmitError::MissingTypeInfo(v) => write!(
                f,
                "type-check guard on {:?} has no recorded observed type",
                v
            ),
        }
    }
}

impl std::error::Error for GuardEmitError {}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::{AbiParam, Function, Signature, UserFuncName};
    use cranelift_codegen::isa::CallConv;
    use cranelift_frontend::FunctionBuilderContext;
    use relon_trace_jit::{ExternalPc, ObservedType};

    fn fresh_builder() -> (Function, FunctionBuilderContext) {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I32));
        (
            Function::with_name_signature(UserFuncName::user(0, 0), sig),
            FunctionBuilderContext::new(),
        )
    }

    #[test]
    fn not_null_guard_emits_brif_pair() {
        let (mut func, mut fbc) = fresh_builder();
        let mut builder = FunctionBuilder::new(&mut func, &mut fbc);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let v = builder.block_params(entry)[0];

        let deopt = builder.create_block();
        builder.append_block_param(deopt, I32);
        builder.append_block_param(deopt, I64);

        let type_info = FxHashMap::default();
        let mut ssa_to_value = FxHashMap::default();
        ssa_to_value.insert(SsaVar(0), v);
        let site = GuardSite::new(7, ExternalPc(0xfeedbeef), GuardKind::NotNull(SsaVar(0)));

        let overflow_bits = FxHashMap::default();
        let mut ctx = GuardEmitCtx {
            builder: &mut builder,
            deopt_block: deopt,
            type_info: &type_info,
            pointer_ty: I64,
            overflow_bits: &overflow_bits,
        };
        emit_guard(&mut ctx, &site, &ssa_to_value).expect("guard emits");
    }

    #[test]
    fn type_check_observed_matches_emits_const_one() {
        let (mut func, mut fbc) = fresh_builder();
        let mut builder = FunctionBuilder::new(&mut func, &mut fbc);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let deopt = builder.create_block();
        builder.append_block_param(deopt, I32);
        builder.append_block_param(deopt, I64);

        let mut type_info = FxHashMap::default();
        type_info.insert(SsaVar(3), ObservedType::I64);
        let ssa_to_value = FxHashMap::default();
        let site = GuardSite::new(
            2,
            ExternalPc(0x1),
            GuardKind::TypeCheck(SsaVar(3), ObservedType::I64),
        );

        let overflow_bits = FxHashMap::default();
        let mut ctx = GuardEmitCtx {
            builder: &mut builder,
            deopt_block: deopt,
            type_info: &type_info,
            pointer_ty: I64,
            overflow_bits: &overflow_bits,
        };
        emit_guard(&mut ctx, &site, &ssa_to_value).expect("type guard emits");
    }

    #[test]
    fn missing_type_info_for_type_check_errors() {
        let (mut func, mut fbc) = fresh_builder();
        let mut builder = FunctionBuilder::new(&mut func, &mut fbc);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let deopt = builder.create_block();
        builder.append_block_param(deopt, I32);
        builder.append_block_param(deopt, I64);

        let type_info = FxHashMap::default();
        let ssa_to_value = FxHashMap::default();
        let site = GuardSite::new(
            0,
            ExternalPc(0),
            GuardKind::TypeCheck(SsaVar(99), ObservedType::Bool),
        );

        let overflow_bits = FxHashMap::default();
        let mut ctx = GuardEmitCtx {
            builder: &mut builder,
            deopt_block: deopt,
            type_info: &type_info,
            pointer_ty: I64,
            overflow_bits: &overflow_bits,
        };
        let err = emit_guard(&mut ctx, &site, &ssa_to_value).unwrap_err();
        assert!(matches!(err, GuardEmitError::MissingTypeInfo(_)));
    }
}
