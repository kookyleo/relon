//! Trace optimiser passes.
//!
//! Each pass implements [`OptimizerPass`] -- a stateless visitor that
//! mutates a [`crate::TraceBuffer`] in place and returns a
//! [`PassReport`] for diagnostics.
//!
//! ## Pass ordering invariants
//!
//! The default pipeline runs nine passes in a fixed order. Each
//! ordering choice has a load-bearing reason; before reordering or
//! inserting a new pass, read the dependency it would cross.
//!
//! 1. [`const_fold::ConstFold`] -- collapse arithmetic on captured
//!    constants. MUST run first so the literals it propagates are
//!    visible to every subsequent pass (load-forwarding's slot
//!    keys, type-spec's guard arguments, dict-ic-hoist's invariance
//!    check).
//! 2. [`load_forward::LoadForwarding`] -- alias `Load(addr)` results
//!    to the value most recently `Store`d at the same slot. Relies
//!    on `ConstFold` having turned constant offsets into stable
//!    `(base, offset)` keys.
//! 3. [`dead_store::DeadStoreElim`] (round 1) -- drop the loads
//!    forwarded above plus any plain redundant stores. MUST follow
//!    `LoadForwarding` to pick up its trail of dead `Load` ops.
//! 4. [`type_spec::TypeSpec`] -- insert `Guard(TypeCheck(...))` ops
//!    in front of generic call sites with observed types. Order
//!    relative to `dict_ic_hoist` / `licm` is incidental (the
//!    guards it inserts don't affect dict-pointer invariance), but
//!    it MUST precede `noop_typecheck_elim`, which folds away the
//!    guards `type_spec` inserts but `licm` later finds redundant.
//! 5. [`dict_ic_hoist::DictIcHoist`] -- split in-loop
//!    `TraceOp::DictLookup` ops with loop-invariant `dict_ptr` into
//!    a hoistable `DictShapeGuard` + an in-loop
//!    `DictLookupPrechecked`. MUST run **before** `LICM` so the
//!    freshly inserted `DictShapeGuard` ops are visible to the
//!    LICM invariant scan in the same pipeline round; otherwise
//!    they would only get hoisted on a follow-up run, which the
//!    pipeline never schedules.
//! 6. [`licm::LICM`] -- hoist `MarkLoopHead`-bracketed pure
//!    invariants (including the `DictShapeGuard`s from the previous
//!    pass) to the loop preheader. MUST run after `dict_ic_hoist`
//!    (see above) and before `noop_typecheck_elim` (see below).
//! 7. [`noop_typecheck_elim::NoopTypeCheckElim`] -- drop
//!    `Guard(TypeCheck(var, ty))` ops whose observed type already
//!    matches `ty`. MUST run **after** `LICM` so any TypeCheck that
//!    LICM hoisted out of the loop body — and that now sits in a
//!    region where the observed type is statically known — also
//!    gets eliminated in the same pass.
//! 8. [`iv_overflow_elim::IvOverflowElim`] -- prove that the in-loop
//!    `Guard(ArithOverflow(_))` guards a bounded induction variable
//!    never fires, and splice a single entry guard `n <= MAX_SAFE`
//!    into the preheader so a runtime that violates the bound deopts
//!    safely. MUST run after `LICM` (so the loop-bound `n` lives
//!    above the head as a `LocalGet`) and before the round-2
//!    `DeadStoreElim` (which folds the `of_bit` chain cranelift's
//!    DCE leaves behind once the guard disappears).
//! 9. [`dead_store::DeadStoreElim`] (round 2) -- pick up any stores
//!    that became dead after type specialisation / LICM moved
//!    guards around. Cheap when nothing changed; preserved so the
//!    pipeline doesn't need a third round for rare interactions.
//!
//! Two rounds of `DeadStoreElim` are explicit: round 1 cleans up
//! forwarded loads (cheap), round 2 cleans up the trailing effects
//! of `type_spec` / `licm` (rarely needed, but cheap to keep).
//!
//! When adding a new pass, declare its ordering dependencies in
//! this list AND in the new pass's own module-level docs (each
//! existing pass file carries an "Ordering" section pointing here).

pub mod const_fold;
pub mod dead_store;
pub mod dict_ic_hoist;
pub mod iv_overflow_elim;
pub mod licm;
pub mod load_forward;
pub mod noop_typecheck_elim;
pub mod type_spec;

use crate::buffer::TraceBuffer;

/// Diagnostic summary returned by every pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassReport {
    pub ops_removed: usize,
    pub ops_replaced: usize,
    pub guards_added: usize,
}

impl PassReport {
    pub fn touched(&self) -> bool {
        self.ops_removed > 0 || self.ops_replaced > 0 || self.guards_added > 0
    }
}

/// One trace-optimiser pass.
pub trait OptimizerPass {
    fn name(&self) -> &'static str;
    fn run(&self, trace: &mut TraceBuffer) -> PassReport;
}

/// Standard pass pipeline. Runs each pass once; reorder by mutating
/// the `passes` vec if needed.
pub struct OptimizerPipeline {
    pub passes: Vec<Box<dyn OptimizerPass>>,
}

impl OptimizerPipeline {
    /// Build the default pipeline described in the module-level docs.
    pub fn default_pipeline() -> Self {
        Self {
            passes: vec![
                Box::new(const_fold::ConstFold),
                Box::new(load_forward::LoadForwarding),
                Box::new(dead_store::DeadStoreElim),
                Box::new(type_spec::TypeSpec),
                // F-D8-E.2: insert `DictShapeGuard`s ahead of LICM
                // so the freshly-emitted guards are visible to its
                // invariant scan in the same pipeline round.
                Box::new(dict_ic_hoist::DictIcHoist),
                Box::new(licm::LICM),
                // ε-M0: drop Guard(TypeCheck(var, ty)) ops whose
                // observed type already matches expected. Runs AFTER
                // licm so any hoisted no-op TypeCheck above the loop
                // also gets removed in the same pass.
                Box::new(noop_typecheck_elim::NoopTypeCheckElim),
                // W4 IV-overflow elim: drop Guard(ArithOverflow(...))
                // ops on bounded induction variables. Runs after LICM
                // (which hoists the bound `n` to the preheader) and
                // before the round-2 dead-store pass.
                Box::new(iv_overflow_elim::IvOverflowElim),
                Box::new(dead_store::DeadStoreElim),
            ],
        }
    }

    /// Empty pipeline (callers build their own ordering).
    pub fn empty() -> Self {
        Self { passes: Vec::new() }
    }

    /// Run every pass in order and aggregate their reports.
    pub fn run(&self, trace: &mut TraceBuffer) -> Vec<(&'static str, PassReport)> {
        let mut out = Vec::with_capacity(self.passes.len());
        for pass in &self.passes {
            let r = pass.run(trace);
            out.push((pass.name(), r));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::TraceBuffer;

    #[test]
    fn empty_pipeline_is_noop() {
        let p = OptimizerPipeline::empty();
        let mut b = TraceBuffer::new();
        let report = p.run(&mut b);
        assert!(report.is_empty());
    }

    #[test]
    fn default_pipeline_has_nine_passes() {
        // ε-M0 added `noop_typecheck_elim` between LICM and the
        // second dead-store round; F-D8-E.2 added `dict_ic_hoist`
        // immediately before LICM. The W4 IV-overflow-elim pass slots
        // in between `noop_typecheck_elim` and the round-2
        // dead-store pass.
        let p = OptimizerPipeline::default_pipeline();
        assert_eq!(p.passes.len(), 9);
    }

    #[test]
    fn pass_report_touched_when_changed() {
        let r = PassReport {
            ops_removed: 1,
            ..Default::default()
        };
        assert!(r.touched());
        let r0 = PassReport::default();
        assert!(!r0.touched());
    }
}
