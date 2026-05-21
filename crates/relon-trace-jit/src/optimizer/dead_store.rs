//! Dead-store elimination.
//!
//! Walks the trace and removes `Store(base, offset, _)` ops that
//! satisfy *all* of these:
//!
//! 1. Some later `Store(base, offset, _)` writes the same slot
//!    before any intervening `Load(base, offset)` reads it.
//! 2. There is no `Guard` op between the two stores. A guard can
//!    fail and force a deopt, at which point the generic code
//!    expects to see the earlier store's value in memory. Trace-side
//!    elision of that store would silently lose data.
//! 3. There is no `Call` to a callee with effects other than
//!    `Pure` / `ReadOnly` between the two stores. Anything that
//!    might inspect / mutate memory blocks elimination.
//!
//! The pass is intentionally conservative: it operates per
//! `(base_ssa, offset)` key with byte-exact matching. Aliasing
//! between different base SSAs is *not* analysed.
//!
//! ## Ordering
//!
//! Scheduled twice by [`super::OptimizerPipeline::default_pipeline`]:
//!
//! - **Round 1** runs after [`super::load_forward::LoadForwarding`]
//!   to drop the now-dead `Load` ops it leaves behind.
//! - **Round 2** runs last, after
//!   [`super::noop_typecheck_elim::NoopTypeCheckElim`], to mop up
//!   any stores that became dead because LICM moved a guard past
//!   them. Round 2 is cheap when nothing changed; preserved so the
//!   pipeline doesn't need a third pass for rare interactions.
//!
//! See the [`super`] module docs for the full pipeline contract.

use std::collections::HashMap;

use crate::buffer::TraceBuffer;
use crate::effect::EffectClass;
use crate::trace_ir::{Offset, SsaVar, TraceOp};

use super::{OptimizerPass, PassReport};

pub struct DeadStoreElim;

impl OptimizerPass for DeadStoreElim {
    fn name(&self) -> &'static str {
        "dead_store_elim"
    }

    fn run(&self, trace: &mut TraceBuffer) -> PassReport {
        let mut report = PassReport::default();
        let dead = find_dead_stores(&trace.ops);
        if dead.is_empty() {
            return report;
        }

        // Drop dead ops while keeping the remaining order intact.
        // We have to fix up `guards`' trace_pc so they keep pointing
        // at the right ops in the compacted buffer.
        let mut new_ops = Vec::with_capacity(trace.ops.len() - dead.len());
        let mut pc_remap = vec![u32::MAX; trace.ops.len()];
        for (old_pc, op) in trace.ops.drain(..).enumerate() {
            if dead.contains(&old_pc) {
                continue;
            }
            pc_remap[old_pc] = new_ops.len() as u32;
            new_ops.push(op);
        }
        report.ops_removed = dead.len();
        trace.ops = new_ops;

        for guard in &mut trace.guards {
            let new_pc = pc_remap
                .get(guard.trace_pc as usize)
                .copied()
                .unwrap_or(u32::MAX);
            // If a guard's anchor op was deleted it's a bug -- guards
            // are never themselves dead stores, so the remap must be
            // valid. Assert in debug.
            debug_assert!(new_pc != u32::MAX, "guard anchored on dead op");
            guard.trace_pc = new_pc;
        }
        report
    }
}

fn find_dead_stores(ops: &[TraceOp]) -> std::collections::HashSet<usize> {
    let mut dead = std::collections::HashSet::new();
    // Map of (base, offset) -> idx of most recent live store.
    let mut latest: HashMap<(SsaVar, i32), usize> = HashMap::new();

    for (idx, op) in ops.iter().enumerate() {
        match op {
            TraceOp::Store(base, Offset(off), _src) => {
                let key = (*base, *off);
                if let Some(prev) = latest.insert(key, idx) {
                    if is_block_free(&ops[prev + 1..idx]) {
                        dead.insert(prev);
                    }
                }
            }
            TraceOp::Load(_, base, Offset(off)) => {
                latest.remove(&(*base, *off));
            }
            // Calls / unrecoverable ops: clear knowledge globally to
            // be safe. We do not attempt alias reasoning.
            TraceOp::Call(_, _, _, eff)
                if !matches!(eff, EffectClass::Pure | EffectClass::ReadOnly) =>
            {
                latest.clear();
            }
            _ => {}
        }
    }
    dead
}

/// Returns true if the slice contains no op that would block
/// elimination of a store that brackets it: i.e. no guard, no impure
/// call.
fn is_block_free(ops: &[TraceOp]) -> bool {
    for op in ops {
        if op.is_guard() {
            return false;
        }
        if let TraceOp::Call(_, _, _, eff) = op {
            if !matches!(eff, EffectClass::Pure | EffectClass::ReadOnly) {
                return false;
            }
        }
    }
    true
}
