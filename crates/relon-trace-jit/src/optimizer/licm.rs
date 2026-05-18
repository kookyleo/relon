//! Loop-invariant code motion.
//!
//! Trace IR is linear, but the recorder marks loop bodies with
//! [`TraceOp::MarkLoopHead`] / [`TraceOp::MarkLoopBack`] pairs (added
//! for v6-gamma). LICM identifies ops whose every SSA input is
//! defined outside the enclosing loop body and hoists them to just
//! before the loop's `MarkLoopHead`. Outputs are not renamed -- the
//! op simply executes earlier and its SSA result is now visible at
//! the loop's entry, so existing reads inside the loop are still
//! valid.
//!
//! ## Hoist eligibility
//!
//! An op is hoistable iff **all** of:
//!
//! 1. Its [`EffectClass`] is `Pure`. `ReadOnly` is intentionally
//!    excluded: lifting a load above the loop changes its observed
//!    state if any iteration writes the same slot, and we do not
//!    do dependency analysis here. `RecoverableWrite` and worse are
//!    never hoisted.
//! 2. It is not a `Guard`. Guard placement is position-sensitive
//!    (deopt expects the trace to have reached that pc before
//!    failing).
//! 3. It is not itself a loop marker.
//! 4. Every SSA input is defined *outside* the loop body. "Defined
//!    outside" means: produced by an op at a pc strictly less than
//!    the loop's `MarkLoopHead`, or never produced at all (an
//!    externally captured value).
//!
//! Nested loops are supported via `loop_id`. An op may be hoisted
//! out of the innermost loop containing it, and the next pass run
//! can further hoist it out of an outer loop. We do the multi-level
//! lifting in a single pass by walking loops outside-in and
//! re-checking after each rewrite.
//!
//! ## Implementation outline
//!
//! 1. Pair up `MarkLoopHead`/`MarkLoopBack` ops by `loop_id`. We
//!    only consider well-formed pairs; an unmatched marker is a
//!    recorder bug -- LICM logs nothing and leaves the trace alone
//!    for that loop.
//! 2. For each pair (innermost first), scan the loop body collecting
//!    hoistable indices.
//! 3. Splice the hoistable ops out of the loop and re-insert them
//!    immediately before the `MarkLoopHead`. Order among the hoisted
//!    ops is preserved.
//! 4. After moving ops, rebuild the guard `trace_pc` table because
//!    indices shifted.

use std::collections::{HashMap, HashSet};

use crate::buffer::TraceBuffer;
use crate::effect::EffectClass;
use crate::trace_ir::{SsaVar, TraceOp};

use super::{OptimizerPass, PassReport};

/// Loop-invariant code motion pass. Stateless.
pub struct LICM;

impl OptimizerPass for LICM {
    fn name(&self) -> &'static str {
        "licm"
    }

    fn run(&self, trace: &mut TraceBuffer) -> PassReport {
        let mut report = PassReport::default();

        loop {
            let loops = collect_loops(&trace.ops);
            if loops.is_empty() {
                break;
            }
            // Process innermost loops first so an op can subsequently
            // bubble out further when the enclosing loop is visited
            // in the next iteration.
            let mut progressed = false;
            for lp in &loops {
                if hoist_one_loop(trace, lp, &mut report) {
                    progressed = true;
                    // Restart from a fresh scan -- indices changed.
                    break;
                }
            }
            if !progressed {
                break;
            }
        }

        rebind_guard_pcs(trace);
        report
    }
}

/// A located `MarkLoopHead`/`MarkLoopBack` pair.
#[derive(Debug, Clone, Copy)]
struct LoopRange {
    head_pc: usize,
    back_pc: usize,
    #[allow(dead_code)]
    loop_id: u32,
    /// Nesting depth (0 = outermost). Used so we can prefer innermost
    /// loops first.
    depth: usize,
}

/// Walk the op stream once and collect every well-formed loop pair.
/// Returns them sorted with the **innermost / deepest** first.
fn collect_loops(ops: &[TraceOp]) -> Vec<LoopRange> {
    let mut stack: Vec<(u32, usize)> = Vec::new(); // (loop_id, head_pc)
    let mut out: Vec<LoopRange> = Vec::new();
    for (pc, op) in ops.iter().enumerate() {
        if let Some(id) = op.loop_head_id() {
            stack.push((id, pc));
        } else if let Some(id) = op.loop_back_id() {
            // Pop the matching head. If unmatched, skip silently
            // (the recorder is buggy but we don't want to crash a
            // pipeline run).
            if let Some(pos) = stack.iter().rposition(|(sid, _)| *sid == id) {
                let (loop_id, head_pc) = stack.remove(pos);
                let depth = stack.len(); // depth after pop = nesting under any remaining heads
                out.push(LoopRange {
                    head_pc,
                    back_pc: pc,
                    loop_id,
                    depth,
                });
            }
        }
    }
    // Deepest first: larger depth before smaller.
    out.sort_by(|a, b| b.depth.cmp(&a.depth).then(a.head_pc.cmp(&b.head_pc)));
    out
}

/// Try to hoist invariants out of a single loop. Returns true if any
/// op moved.
fn hoist_one_loop(trace: &mut TraceBuffer, lp: &LoopRange, report: &mut PassReport) -> bool {
    // Snapshot the loop body PCs. We hoist ops *strictly between*
    // the head and back markers.
    let body_start = lp.head_pc + 1;
    let body_end = lp.back_pc; // exclusive
    if body_start >= body_end {
        return false;
    }

    // Set of SSA ids defined inside this loop body. An op whose
    // inputs are all *outside* this set is invariant.
    let inside_defs: HashSet<SsaVar> = (body_start..body_end)
        .filter_map(|i| trace.ops[i].output())
        .collect();

    // Collect candidate indices in order.
    let mut hoist_pcs: Vec<usize> = Vec::new();
    for pc in body_start..body_end {
        if !is_hoistable(&trace.ops[pc]) {
            continue;
        }
        let inputs = trace.ops[pc].inputs();
        if inputs.iter().any(|v| inside_defs.contains(v)) {
            continue;
        }
        hoist_pcs.push(pc);
    }
    if hoist_pcs.is_empty() {
        return false;
    }

    // Extract the hoisted ops (cloning so we don't have to worry
    // about index shifting during removal).
    let hoisted: Vec<TraceOp> = hoist_pcs.iter().map(|&p| trace.ops[p].clone()).collect();

    // Remove from highest pc to lowest so earlier indices stay
    // valid.
    let mut hoist_set: HashSet<usize> = hoist_pcs.iter().copied().collect();
    let mut new_ops: Vec<TraceOp> = Vec::with_capacity(trace.ops.len());
    let head_pc = lp.head_pc;
    for (pc, op) in trace.ops.drain(..).enumerate() {
        if pc == head_pc {
            // Insert hoisted ops *before* the head marker.
            for h in &hoisted {
                new_ops.push(h.clone());
            }
            new_ops.push(op);
            continue;
        }
        if hoist_set.remove(&pc) {
            // Skip -- already prepended above.
            continue;
        }
        new_ops.push(op);
    }
    trace.ops = new_ops;
    report.ops_replaced += hoisted.len();
    true
}

fn is_hoistable(op: &TraceOp) -> bool {
    if op.is_guard() || op.is_loop_head() || op.is_loop_back() {
        return false;
    }
    match op.effect_class() {
        EffectClass::Pure => {
            // Even pure: Return is not movable; it ends the trace.
            !matches!(op, TraceOp::Return(_))
        }
        EffectClass::ReadOnly | EffectClass::RecoverableWrite | EffectClass::Unrecoverable => false,
    }
}

/// Rebind every `GuardSite::trace_pc` to the current position of its
/// guard op. Matches by `(GuardKind, occurrence index)` so duplicate
/// kinds still line up positionally.
fn rebind_guard_pcs(trace: &mut TraceBuffer) {
    if trace.guards.is_empty() {
        return;
    }
    // Build a queue of pcs for each kind in document order.
    let mut by_kind: HashMap<crate::trace_ir::GuardKind, Vec<usize>> = HashMap::new();
    for (pc, op) in trace.ops.iter().enumerate() {
        if let TraceOp::Guard(k, _) = op {
            by_kind.entry(*k).or_default().push(pc);
        }
    }
    // Drain front-to-back so guard order is preserved.
    for site in &mut trace.guards {
        if let Some(q) = by_kind.get_mut(&site.kind) {
            if !q.is_empty() {
                site.trace_pc = q.remove(0) as u32;
            }
        }
    }
}
