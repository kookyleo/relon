//! ε-M0: drop `Guard(TypeCheck(var, ty))` ops that the emitter would
//! lower to a constant-true predicate (`brif (iconst 1), ok, deopt`
//! → `jump ok` after cranelift's egraph fold).
//!
//! The pass scans the buffer once; for every `Guard(TypeCheck(var,
//! expected))` op it consults the buffer's [`crate::TraceBuffer::type_info`]
//! map. If the recorder pinned `var`'s observed type to the same
//! `expected`, the guard never fires at runtime — dropping the op
//! shrinks the cranelift IR by one brif + iconst pair per
//! eliminated guard, which compounds to ~0.5-1 ns/iter for tight
//! hot-loop traces with multiple per-iter TypeChecks.
//!
//! Side-table fix-up: when an op is removed the matching
//! [`crate::GuardSite::trace_pc`] entries are kept (the
//! emitter only consults them on guards that survive). The pass
//! also rebinds surviving guards' trace_pc to their new indices via
//! a position-tracking sweep — mirrors `rebind_guard_pcs` in
//! [`crate::optimizer::licm`].
//!
//! ## Ordering
//!
//! Runs after [`super::licm::LICM`] and before the round-2
//! [`super::dead_store::DeadStoreElim`]. The post-LICM placement is
//! load-bearing: LICM can hoist a `TypeCheck` guard out of the loop
//! body into the preheader, and only after that move does this pass
//! see it sitting in a region where the recorder's observed type
//! statically matches `expected`. Running before LICM would miss
//! every hoist-eligible guard and leave the per-iter `brif` on the
//! hot path. See the [`super`] module docs for the full pipeline
//! contract.

use std::collections::HashSet;

use crate::buffer::TraceBuffer;
use crate::trace_ir::{GuardKind, TraceOp};

use super::{OptimizerPass, PassReport};

/// `Guard(TypeCheck(var, ty))` eliminator: drops ops the emitter would
/// fold to `jump ok`.
pub struct NoopTypeCheckElim;

impl OptimizerPass for NoopTypeCheckElim {
    fn name(&self) -> &'static str {
        "noop_typecheck_elim"
    }

    fn run(&self, trace: &mut TraceBuffer) -> PassReport {
        let mut report = PassReport::default();
        let mut drop: HashSet<usize> = HashSet::new();
        for (pc, op) in trace.ops.iter().enumerate() {
            if let TraceOp::Guard(GuardKind::TypeCheck(var, expected), _) = op {
                if trace.type_info.get(var).copied() == Some(*expected) {
                    drop.insert(pc);
                }
            }
        }
        if drop.is_empty() {
            return report;
        }
        // Build the new op stream by skipping dropped indices. Track
        // the old→new mapping so we can rebind surviving guards'
        // trace_pc values.
        let mut new_ops: Vec<TraceOp> = Vec::with_capacity(trace.ops.len() - drop.len());
        let mut old_to_new: Vec<Option<u32>> = vec![None; trace.ops.len()];
        for (pc, op) in trace.ops.drain(..).enumerate() {
            if drop.contains(&pc) {
                report.ops_removed += 1;
                continue;
            }
            old_to_new[pc] = Some(new_ops.len() as u32);
            new_ops.push(op);
        }
        trace.ops = new_ops;
        // Rebind guard sites: drop entries for removed pcs, remap the
        // rest. The emitter consults `trace.guards` by `trace_pc` to
        // find the matching site for each surviving Guard op.
        let mut new_guards = Vec::with_capacity(trace.guards.len());
        for g in trace.guards.drain(..) {
            let old_pc = g.trace_pc as usize;
            if let Some(Some(new_pc)) = old_to_new.get(old_pc).copied() {
                let mut site = g;
                site.trace_pc = new_pc;
                new_guards.push(site);
            }
        }
        trace.guards = new_guards;
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace_ir::ObservedType;

    #[test]
    fn drops_const_typecheck() {
        let mut b = TraceBuffer::new();
        let v = b.fresh_ssa();
        b.append(TraceOp::ConstI64(v, 7));
        b.record_type(v, ObservedType::I64);
        b.append(TraceOp::Guard(
            GuardKind::TypeCheck(v, ObservedType::I64),
            v,
        ));
        b.append(TraceOp::Return(v));
        let n_before = b.ops.len();
        let report = NoopTypeCheckElim.run(&mut b);
        assert_eq!(report.ops_removed, 1);
        assert_eq!(b.ops.len(), n_before - 1);
        for op in &b.ops {
            assert!(
                !matches!(op, TraceOp::Guard(GuardKind::TypeCheck(_, _), _)),
                "no surviving TypeCheck guard"
            );
        }
    }

    #[test]
    fn keeps_mismatched_typecheck() {
        let mut b = TraceBuffer::new();
        let v = b.fresh_ssa();
        b.append(TraceOp::ConstI64(v, 7));
        b.record_type(v, ObservedType::I64);
        // Guard expects F64 — would deopt at runtime.
        b.append(TraceOp::Guard(
            GuardKind::TypeCheck(v, ObservedType::F64),
            v,
        ));
        b.append(TraceOp::Return(v));
        let n_before = b.ops.len();
        let report = NoopTypeCheckElim.run(&mut b);
        assert_eq!(report.ops_removed, 0);
        assert_eq!(b.ops.len(), n_before);
    }
}
