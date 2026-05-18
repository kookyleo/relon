//! Trace optimiser passes.
//!
//! Each pass implements [`OptimizerPass`] -- a stateless visitor that
//! mutates a [`crate::TraceBuffer`] in place and returns a
//! [`PassReport`] for diagnostics.
//!
//! Pass ordering matters. The pipeline runs:
//!
//! 1. [`const_fold::ConstFold`] -- collapse arithmetic on captured
//!    constants. Must run first so later passes see propagated
//!    literals.
//! 2. [`type_spec::TypeSpec`] -- replace generic ops with
//!    type-specialised variants and insert
//!    `Guard(TypeCheck(...))` ops.
//! 3. [`dead_store::DeadStoreElim`] -- remove writes whose target is
//!    overwritten later in the same trace and never read in between.
//!
//! This order keeps dead-store elimination conservative: it runs
//! after type specialisation so that the inserted type-check guards
//! are already in place (a `RecoverableWrite` op cannot move across
//! them).

pub mod const_fold;
pub mod dead_store;
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
                Box::new(type_spec::TypeSpec),
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
    fn default_pipeline_has_three_passes() {
        let p = OptimizerPipeline::default_pipeline();
        assert_eq!(p.passes.len(), 3);
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
