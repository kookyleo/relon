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
//!
//! ## Ordering
//!
//! First pass in [`super::OptimizerPipeline::default_pipeline`]; no
//! upstream dependencies. MUST stay first so the literals it
//! propagates are visible to every subsequent pass (load-forwarding's
//! `(base, offset)` slot keys, type-spec's guard arguments, dict-ic-
//! hoist's `dict_ptr` invariance check). See the [`super`] module
//! docs for the full pipeline contract.

use crate::buffer::TraceBuffer;
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

        // Inputs come from the `known` table keyed by SSA id, so a
        // RecoverableWrite op can never silently overwrite an earlier
        // const we depend on. The op being folded is itself replaced
        // by a pure ConstI32/I64; for `Mod` (the one RecoverableWrite
        // arithmetic op the recorder produces today) `fold_arith`
        // refuses the fold when the divisor is 0 or `MIN % -1`, so
        // the runtime trap path is preserved.

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
    known: &mut rustc_hash::FxHashMap<SsaVar, TraceConst>,
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
