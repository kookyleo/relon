//! Constant-folding pass.
//!
//! Walks the trace top-to-bottom; whenever both operands of an
//! arithmetic / comparison op are known constants (either via a
//! preceding `ConstI32` / `ConstI64` op or via
//! [`crate::TraceBuffer::consts`]), the op is rewritten in place as
//! the corresponding `Const*` op and the buffer's `consts` table is
//! updated for the now-constant SSA destination.
//!
//! The pass refuses to fold across any op with
//! [`crate::EffectClass::is_reorder_barrier`] set when looking at a
//! predecessor input -- this keeps cursor-bumping ops from being
//! silently elided. Since folding rewrites in place rather than
//! moving ops, the only barrier check needed is on operand lookup.
//!
//! `Cmp` results are encoded as i32 (1 / 0) so the trace IR stays
//! integer-typed end-to-end.

use crate::buffer::TraceBuffer;
use crate::effect::EffectClass;
use crate::trace_ir::{CmpKind, SsaVar, TraceConst, TraceOp};

use super::{OptimizerPass, PassReport};

/// Constant-folding pass type. Stateless.
pub struct ConstFold;

impl OptimizerPass for ConstFold {
    fn name(&self) -> &'static str {
        "const_fold"
    }

    fn run(&self, trace: &mut TraceBuffer) -> PassReport {
        let mut report = PassReport::default();
        // Snapshot of known constants. We seed from the buffer's
        // existing const table and from the linear scan below, so
        // chains like `ConstI32 a, ConstI32 b, Add c a b` collapse in
        // a single pass.
        let mut known = trace.consts.clone();

        // Cache effect class per pc to enforce reorder-barrier rule
        // when checking whether an upstream const is still valid.
        // Because we only fold ops whose *inputs* are already in
        // `known` and we walk top-to-bottom, the barrier rule is
        // simpler: ConstI32/ConstI64 ops never cross a barrier
        // because they have no inputs. We only need to make sure we
        // do not let a RecoverableWrite *erase* an earlier const --
        // it cannot, since `known` is keyed by SSA id, not by
        // address. So we just verify the input ops aren't themselves
        // RecoverableWrite outputs (they can't be -- those produce no
        // SSA output for `Store` and `Div` produces non-foldable
        // semantics).
        //
        // The explicit barrier check below guards future variants:
        // if a `RecoverableWrite` op is ever added that *does*
        // produce an SSA value, we must NOT fold it.
        let is_safe_const_source = |op: &TraceOp| match op {
            TraceOp::ConstI32(_, _) | TraceOp::ConstI64(_, _) => true,
            _ => !op.effect_class().is_reorder_barrier(),
        };

        for idx in 0..trace.ops.len() {
            let op = trace.ops[idx].clone();
            match op {
                TraceOp::ConstI32(dst, v) => {
                    known.insert(dst, TraceConst::I32(v));
                }
                TraceOp::ConstI64(dst, v) => {
                    known.insert(dst, TraceConst::I64(v));
                }
                TraceOp::Add(dst, a, b)
                | TraceOp::Sub(dst, a, b)
                | TraceOp::Mul(dst, a, b)
                | TraceOp::Mod(dst, a, b) => {
                    if let (Some(ka), Some(kb)) = (known.get(&a), known.get(&b)) {
                        if !is_safe_const_source(&trace.ops[idx]) {
                            continue;
                        }
                        if let Some(folded) = fold_arith(&trace.ops[idx], *ka, *kb) {
                            apply_fold(trace, &mut known, idx, dst, folded);
                            report.ops_replaced += 1;
                        }
                    }
                }
                TraceOp::Cmp(kind, dst, a, b) => {
                    if let (Some(ka), Some(kb)) = (known.get(&a), known.get(&b)) {
                        if let Some(result) = fold_cmp(kind, *ka, *kb) {
                            let folded = TraceConst::I32(if result { 1 } else { 0 });
                            apply_fold(trace, &mut known, idx, dst, folded);
                            report.ops_replaced += 1;
                        }
                    }
                }
                _ => {}
            }
        }

        // Reflect updated knowledge back into the buffer.
        trace.consts = known;
        report
    }
}

fn fold_arith(op: &TraceOp, a: TraceConst, b: TraceConst) -> Option<TraceConst> {
    // We only fold when both consts share a width. Mixed I32/I64
    // folding is intentionally skipped -- the recorder is responsible
    // for emitting explicit widening ops, which we don't model yet.
    match (op, a, b) {
        (TraceOp::Add(_, _, _), TraceConst::I32(x), TraceConst::I32(y)) => {
            Some(TraceConst::I32(x.wrapping_add(y)))
        }
        (TraceOp::Sub(_, _, _), TraceConst::I32(x), TraceConst::I32(y)) => {
            Some(TraceConst::I32(x.wrapping_sub(y)))
        }
        (TraceOp::Mul(_, _, _), TraceConst::I32(x), TraceConst::I32(y)) => {
            Some(TraceConst::I32(x.wrapping_mul(y)))
        }
        (TraceOp::Add(_, _, _), TraceConst::I64(x), TraceConst::I64(y)) => {
            Some(TraceConst::I64(x.wrapping_add(y)))
        }
        (TraceOp::Sub(_, _, _), TraceConst::I64(x), TraceConst::I64(y)) => {
            Some(TraceConst::I64(x.wrapping_sub(y)))
        }
        (TraceOp::Mul(_, _, _), TraceConst::I64(x), TraceConst::I64(y)) => {
            Some(TraceConst::I64(x.wrapping_mul(y)))
        }
        // F-D8-E.1: fold `Mod` only when the divisor is safe. We
        // refuse to fold `_ % 0` (runtime trap surface) and
        // `MIN % -1` (the only overflow case for `srem`) so the
        // emitter's divisor-zero guard / overflow guard stay the
        // single source of truth for the trap behaviour.
        (TraceOp::Mod(_, _, _), TraceConst::I32(x), TraceConst::I32(y)) => {
            if y == 0 || (x == i32::MIN && y == -1) {
                None
            } else {
                Some(TraceConst::I32(x.wrapping_rem(y)))
            }
        }
        (TraceOp::Mod(_, _, _), TraceConst::I64(x), TraceConst::I64(y)) => {
            if y == 0 || (x == i64::MIN && y == -1) {
                None
            } else {
                Some(TraceConst::I64(x.wrapping_rem(y)))
            }
        }
        _ => None,
    }
}

fn fold_cmp(kind: CmpKind, a: TraceConst, b: TraceConst) -> Option<bool> {
    match (a, b) {
        (TraceConst::I32(x), TraceConst::I32(y)) => Some(kind.apply_i64(x as i64, y as i64)),
        (TraceConst::I64(x), TraceConst::I64(y)) => Some(kind.apply_i64(x, y)),
        (TraceConst::Bool(x), TraceConst::Bool(y)) => Some(kind.apply_i64(x as i64, y as i64)),
        _ => None,
    }
}

fn apply_fold(
    trace: &mut TraceBuffer,
    known: &mut std::collections::HashMap<SsaVar, TraceConst>,
    idx: usize,
    dst: SsaVar,
    folded: TraceConst,
) {
    trace.ops[idx] = match folded {
        TraceConst::I32(v) => TraceOp::ConstI32(dst, v),
        TraceConst::I64(v) => TraceOp::ConstI64(dst, v),
        TraceConst::Bool(b) => TraceOp::ConstI32(dst, if b { 1 } else { 0 }),
    };
    known.insert(dst, folded);
}

// Helper: tighten the barrier-source check (currently always true
// for the variants we touch, but keep a function so future readers
// see the intent).
#[allow(dead_code)]
fn safe_to_fold_pure(op: &TraceOp) -> bool {
    matches!(op.effect_class(), EffectClass::Pure)
}
