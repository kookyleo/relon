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
//! 1. Its [`EffectClass`] is `Pure`, or it is one of the
//!    allow-listed `ReadOnly` ops (see [`is_hoistable`]).
//!    `RecoverableWrite` and worse are never hoisted.
//! 2. Guards are normally position-sensitive (deopt expects the
//!    trace to have reached that pc before failing). The exceptions
//!    are [`GuardKind::BoundsCheck`] (F-D8-E.3) and
//!    [`GuardKind::NotNull`] (F-D7-J): when their inputs are
//!    loop-invariant the pass/fail decision is iteration-independent,
//!    so hoisting them merely fires the same deopt earlier — never
//!    later than it would have anyway. F-D8-E.3 admits `BoundsCheck`
//!    so the `ListGet { list_ptr, idx }` it shields can also hoist
//!    when its inputs are invariant; F-D7-J admits `NotNull` so the
//!    `StrContains` / `StrConcat` haystack null-check the recorder
//!    emits ahead of every str op lifts when the haystack is
//!    invariant (saves one brif per iter on W4).
//! 3. It is not itself a loop marker.
//! 4. Every SSA input is defined *outside* the loop body. "Defined
//!    outside" means: produced by an op at a pc strictly less than
//!    the loop's `MarkLoopHead`, or never produced at all (an
//!    externally captured value).
//! 5. For `TraceOp::Load` (F-D7-G): the loop body must contain no
//!    `TraceOp::Store` and no op of effect class `RecoverableWrite`
//!    or `Unrecoverable`. The trace IR's alias model treats every
//!    in-loop write as potentially aliasing the load's slot, so the
//!    conservative gate is "no in-loop writes at all". This is
//!    sufficient for the F-D7-G W4-flavoured pattern (StringRef
//!    `(ptr, len)` loads from a loop-invariant `*const StringRef`
//!    base) — the recorder never emits a `Store` against StringRef
//!    payload in the same trace because the host-side `Arc<str>` is
//!    immutable. When a future phase needs a finer-grained
//!    aliasing check, replace the per-loop boolean with a
//!    `(base, offset)` clobber set.
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
use crate::trace_ir::{GuardKind, Offset, SsaVar, TraceOp};

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

    // Set of SSA ids defined inside this loop body OR by the loop
    // header's φ pairs. An op whose inputs are all *outside* this set
    // is invariant. The φ SSAs are technically defined by the
    // [`TraceOp::MarkLoopHead`] op AT `head_pc`, but for LICM purposes
    // they behave like loop-local definitions: their value changes
    // every iteration (driven by `MarkLoopBack::next_values`), so any
    // op consuming them must stay inside the loop body.
    let mut inside_defs: HashSet<SsaVar> = (body_start..body_end)
        .flat_map(|i| trace.ops[i].defs())
        .collect();
    inside_defs.extend(trace.ops[lp.head_pc].defs());

    // F-D7-G: precompute whether this loop body contains any op that
    // could clobber the slot a hoist candidate `TraceOp::Load` reads.
    // The conservative rule is "any in-loop `Store` or any op of
    // effect class `RecoverableWrite` / `Unrecoverable` blocks all
    // Load hoists". We do not attempt aliasing between distinct
    // `(base, offset)` pairs because the trace IR's alias model is
    // intentionally coarse — every write may alias every load. When
    // the workload's loop body has zero writes (the W3 / W4 string
    // patterns and the W5 / W6 dict/list patterns alike), the gate is
    // open and any loop-invariant Load lifts on the same pipeline
    // round as the existing pure / ReadOnly hoists.
    let body_has_writes = (body_start..body_end).any(|i| {
        let op = &trace.ops[i];
        matches!(op, TraceOp::Store(_, _, _))
            || matches!(
                op.effect_class(),
                EffectClass::RecoverableWrite | EffectClass::Unrecoverable
            )
    });

    // Collect candidate indices in order.
    let mut hoist_pcs: Vec<usize> = Vec::new();
    for pc in body_start..body_end {
        if !is_hoistable(&trace.ops[pc], body_has_writes) {
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

fn is_hoistable(op: &TraceOp, body_has_writes: bool) -> bool {
    if op.is_loop_head() || op.is_loop_back() {
        return false;
    }
    // F-D8-E.3: a `Guard(BoundsCheck(idx, list_ptr))` whose inputs
    // are both loop-invariant is iteration-independent — either it
    // always passes or it always fails. Hoisting it just moves the
    // (would-be) deopt earlier in the trace, which is safe because
    // no side effect sits between the original guard position and
    // the loop head (the in-loop ops up to that point would all be
    // hoistable themselves under the same invariance precondition).
    //
    // F-D7-J extends the same argument to `Guard(NotNull(v))`. The
    // F-D7-B recorder injects a `NotNull(haystack)` ahead of every
    // `StrContains` (and similarly for `StrConcat` / `StrFind` /
    // `StrSubstring`). When `haystack` is loop-invariant (LICM has
    // hoisted the matching `LocalGet`), the null-check answer is
    // iteration-independent too: pass forever or deopt on the very
    // first iter. Lifting the guard above the loop head saves one
    // brif per iter on the W4 hot loop without changing semantics —
    // an early deopt fires the same trace_pc / external_pc the
    // recorder annotated.
    //
    // `TypeCheck` is similarly loop-invariant when its target is
    // outside the body, but the current emitter resolves the
    // predicate from the recorded observed type at install time
    // (constant 0 / 1 brif), so hoisting offers no per-iter win.
    // `ArithOverflow` and `IsZero` are positional by construction —
    // they reference an SSA produced just upstream — and never get
    // a loop-invariant input under the current trace shapes.
    if let TraceOp::Guard(kind, _) = op {
        return matches!(kind, GuardKind::BoundsCheck(_, _) | GuardKind::NotNull(_));
    }
    match op.effect_class() {
        EffectClass::Pure => {
            // Even pure: Return is not movable; it ends the trace.
            !matches!(op, TraceOp::Return(_))
        }
        // F-D7-D: `TraceOp::LocalGet` reads an immutable arg slot.
        // The recorder may emit it inside the loop body when the
        // first observation lands there (e.g. the loop-bound `n`
        // arg, the loop-invariant haystack / needle ptrs in
        // `s.contains(...)` patterns). Treat it as hoistable so the
        // trace doesn't re-read `args_ptr[slot * 8]` every iter — the
        // cranelift backend would constant-propagate anyway but we
        // also want the dependent `StrContains` / `StrConcat` to see
        // its haystack input as a hoistable SSA defined OUTSIDE the
        // loop body, which only happens once LICM moves the
        // `LocalGet` itself.
        //
        // F-D8-E.3: extend the `ReadOnly` allow-list with
        // `ListGet` and `DictLookup`. Both are referentially
        // transparent w.r.t. the trace's own write set — the
        // recorder never emits a `Store` against a dict / list
        // payload header in the same trace, and the optimiser
        // pipeline would refuse to merge such a trace anyway. Their
        // inputs (`list_ptr` / `dict_ptr` / `idx` / `key_ptr`)
        // carry the actual variance, so the input-invariance check
        // upstream of this predicate gates correctness. When *all*
        // inputs are loop-invariant (e.g. the recorder observed a
        // constant index, or a previous LICM round already hoisted
        // `key_ptr`'s producer), the entire op moves to the loop
        // preheader. The matching `Guard(BoundsCheck)` that the
        // recorder prepended to a `ListGet` is hoisted by the
        // dedicated branch above so the deopt-anchored guard stays
        // adjacent to the load.
        //
        // F-D7-G: admit `TraceOp::Load` to the ReadOnly allow-list
        // when the enclosing loop body contains no writes. The
        // recorder emits `Load(dst, base, Offset(0 | 8))` for
        // StringRef `ptr` / `len` payload reads off a `*const
        // StringRef` SSA (see `LoadField { offset, ty }` lowering in
        // the recorder). When the base SSA is loop-invariant and
        // the loop body has no aliasing writes, hoisting the load
        // moves the StringRef header deref into the preheader so
        // the per-iter cost drops to the bare op (the `StrConcat`
        // / `StrContains` extern call). The `body_has_writes` gate
        // keeps the rule conservative: any in-loop `Store` (or any
        // op of effect class `RecoverableWrite` / `Unrecoverable`,
        // such as `Div` / `Mod`) closes the gate for every Load in
        // that body. The input-invariance check upstream of this
        // predicate still gates `base` invariance, so a Load whose
        // base is loop-carried (e.g. an accumulator phi in the W3
        // concat shape) stays inside the loop regardless.
        EffectClass::ReadOnly => match op {
            TraceOp::LocalGet(_, _) | TraceOp::ListGet { .. } | TraceOp::DictLookup { .. } => true,
            TraceOp::Load(_, _, Offset(off)) => !body_has_writes && (*off == 0 || *off == 8),
            _ => false,
        },
        EffectClass::RecoverableWrite | EffectClass::Unrecoverable => false,
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
