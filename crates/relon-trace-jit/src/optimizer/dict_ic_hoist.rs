//! F-D8-E.2 — Dict inline-cache shape-check hoisting.
//!
//! `TraceOp::DictLookup` in the recorded trace stream calls into
//! [`crate::runtime::dict_list::__relon_trace_dict_lookup`] every
//! iteration. Inside that helper the first observable cost is a
//! `read_unaligned::<u64>(dict_ptr)` followed by an `icmp` against
//! the trace's per-op `shape_hash` immediate, and a sentinel-return
//! when they disagree.
//!
//! For W5-shaped traces (`for i in 0..n { acc += d[keys[i % 10]] }`)
//! the dict pointer SSA is loop-invariant — the recorder pulls it
//! from `LocalGet(slot=1)` once and reads it on every iteration. The
//! shape immediate is per-op constant by construction. So the
//! shape compare's outcome is invariant over the entire trace, but
//! the helper pays for it on every iter regardless.
//!
//! This pass rewrites in-loop `TraceOp::DictLookup` ops whose
//! `dict_ptr` SSA is invariant under the enclosing loop into:
//!
//! 1. [`TraceOp::DictShapeGuard { dict_ptr, shape_hash }`] inserted
//!    **immediately before** the original `DictLookup` site. This op
//!    has `EffectClass::Pure` so the downstream
//!    [`crate::optimizer::licm::LICM`] pass lifts it ahead of the
//!    enclosing `MarkLoopHead`.
//! 2. [`TraceOp::DictLookupPrechecked { dst, dict_ptr, key_ptr }`]
//!    replacing the original `DictLookup`. Lowers to a call into
//!    [`crate::runtime::dict_list::__relon_trace_dict_lookup_prechecked`],
//!    which skips the shape compare.
//!
//! ## Ordering invariant
//!
//! This pass MUST run BEFORE `LICM`. The optimizer pipeline already
//! orders LICM after `type_spec`, so we insert `dict_ic_hoist` just
//! before LICM (and after `type_spec`, which is unrelated).
//!
//! ## Why not also hoist the prechecked op when `key_ptr` is invariant
//!
//! For the fully invariant case (single fixed key reused every iter)
//! the LICM pass already does the right thing once we rewrite to
//! `DictLookupPrechecked`: that op's `EffectClass` is `ReadOnly`,
//! which LICM does NOT currently treat as hoistable (loads can in
//! principle be invalidated by intervening writes the pass doesn't
//! track). Extending LICM to whitelist `DictLookupPrechecked` when
//! both inputs are invariant is a follow-up — see the F-D8-E.2 stage
//! report. The shape-check hoist alone is sufficient to move the
//! W5 trace_jit ratio.
//!
//! ## Why not rewrite EVERY `DictLookup`, even outside loops
//!
//! When the DictLookup is not in a loop, splitting it into two ops
//! is pure overhead — the prechecked helper still scans the entry
//! table, and we've added one extra inline shape compare. The pass
//! only rewrites lookups that live inside a well-formed
//! `MarkLoopHead`/`MarkLoopBack` pair, and only when `dict_ptr`'s
//! definition lives outside that body.

use std::collections::{HashMap, HashSet};

use crate::buffer::TraceBuffer;
use crate::trace_ir::{SsaVar, TraceOp};

use super::{OptimizerPass, PassReport};

/// Stateless rewrite pass — see module docs.
pub struct DictIcHoist;

impl OptimizerPass for DictIcHoist {
    fn name(&self) -> &'static str {
        "dict_ic_hoist"
    }

    fn run(&self, trace: &mut TraceBuffer) -> PassReport {
        let mut report = PassReport::default();

        // Walk loops outermost-first so a `DictLookup` inside an
        // inner loop with `dict_ptr` defined between the outer and
        // inner heads still gets the hoist (LICM will then lift the
        // resulting `DictShapeGuard` out of the inner loop on its
        // first round, and out of the outer one on its second).
        //
        // Re-scan loops from a clean state every iteration — the op
        // vector may have shifted under us once we inserted ops.
        // Performance is fine: trace bodies are small (tens of ops,
        // single-digit loops).
        while let Some(target) = next_rewrite_target(&trace.ops) {
            apply_split(trace, target);
            report.ops_replaced += 1;
        }

        report
    }
}

/// What `next_rewrite_target` returns: enough info to perform one
/// in-place split.
#[derive(Debug, Clone, Copy)]
struct RewriteTarget {
    /// PC of the `TraceOp::DictLookup` op to be split.
    lookup_pc: usize,
    /// Captured op fields — we re-emit a pair from them.
    dst: SsaVar,
    dict_ptr: SsaVar,
    key_ptr: SsaVar,
    shape_hash: u64,
}

/// Scan once for the first hoistable `DictLookup`. Returns `None`
/// when no more candidates remain (every in-loop `DictLookup` has
/// already been split, or none qualified to begin with).
fn next_rewrite_target(ops: &[TraceOp]) -> Option<RewriteTarget> {
    // Pair up `MarkLoopHead` / `MarkLoopBack` ranges. Reuses the
    // same well-formedness rules as LICM: stack-based matching, drop
    // unmatched markers silently.
    let mut stack: Vec<(u32, usize)> = Vec::new(); // (loop_id, head_pc)
    let mut active_loops: Vec<(usize, usize)> = Vec::new(); // (head_pc, back_pc)
    for (pc, op) in ops.iter().enumerate() {
        if let Some(id) = op.loop_head_id() {
            stack.push((id, pc));
        } else if let Some(id) = op.loop_back_id() {
            if let Some(pos) = stack.iter().rposition(|(sid, _)| *sid == id) {
                let (_, head_pc) = stack.remove(pos);
                active_loops.push((head_pc, pc));
            }
        }
    }
    if active_loops.is_empty() {
        return None;
    }

    // For each loop body in document order, look for the first
    // `DictLookup` whose `dict_ptr` is invariant under that loop. We
    // prefer the innermost enclosing loop so the resulting
    // `DictShapeGuard` lifts out of the tightest scope first (LICM
    // multi-level hoisting cleans up the rest).
    active_loops.sort_by_key(|(head, back)| back - head); // narrower (innermost) loops first
    for (head_pc, back_pc) in &active_loops {
        let body_start = *head_pc + 1;
        let body_end = *back_pc; // exclusive

        // Map every SSA definition inside the loop body to the op
        // that defines it. We use this to recognise "invariant"
        // intermediate SSAs (e.g. `LocalGet(arg_slot)` that LICM has
        // not yet hoisted) without re-running LICM.
        let mut inside_defs: HashMap<SsaVar, &TraceOp> = HashMap::new();
        for op in ops.iter().take(body_end).skip(body_start) {
            for def in op.defs() {
                inside_defs.insert(def, op);
            }
        }
        // The loop head's φ SSAs are loop-CARRIED, never loop-
        // invariant: their value changes every iter.
        let mut phi_defs: HashSet<SsaVar> = HashSet::new();
        phi_defs.extend(ops[*head_pc].defs());

        for (pc, op) in ops.iter().enumerate().take(body_end).skip(body_start) {
            if let TraceOp::DictLookup {
                dst,
                dict_ptr,
                key_ptr,
                shape_hash,
            } = op
            {
                if is_loop_invariant(*dict_ptr, &inside_defs, &phi_defs) {
                    return Some(RewriteTarget {
                        lookup_pc: pc,
                        dst: *dst,
                        dict_ptr: *dict_ptr,
                        key_ptr: *key_ptr,
                        shape_hash: *shape_hash,
                    });
                }
            }
        }
    }
    None
}

/// Is `var` provably loop-invariant from the body-local SSA def map?
///
/// "Loop-invariant" means: every iteration of the loop sees the same
/// value bound to `var`. Three cases qualify:
///
/// 1. `var` is defined OUTSIDE the loop (not in `inside_defs`) and
///    is not one of the head's φ SSAs. This is the canonical LICM
///    invariant.
/// 2. `var` is defined INSIDE the loop by a [`TraceOp::LocalGet`] —
///    `LocalGet` reads from the trace entry's args pointer, which
///    never changes across iterations of the same trace invocation.
///    LICM will hoist the op on its own pass; we just need to
///    recognise the dependency here so the dict-ic-hoist pass can
///    fire in the same pipeline round.
/// 3. `var` is defined INSIDE the loop by a [`TraceOp::ConstI32`] or
///    [`TraceOp::ConstI64`] — constants are trivially loop-invariant.
///
/// Returns `false` for anything else (arithmetic results, phi values,
/// loads, calls), so a `dict_ptr` derived from per-iter state is
/// correctly rejected and never gets a hoist that would deopt every
/// other iteration.
fn is_loop_invariant(
    var: SsaVar,
    inside_defs: &HashMap<SsaVar, &TraceOp>,
    phi_defs: &HashSet<SsaVar>,
) -> bool {
    if phi_defs.contains(&var) {
        return false; // loop-carried
    }
    match inside_defs.get(&var) {
        None => true,
        Some(op) => matches!(
            op,
            TraceOp::LocalGet(_, _) | TraceOp::ConstI32(_, _) | TraceOp::ConstI64(_, _)
        ),
    }
}

/// In-place split: replace `ops[lookup_pc]` with the
/// `DictLookupPrechecked` flavour and insert a `DictShapeGuard`
/// immediately ahead of it. After the rewrite, the original SSA
/// `dst` is still produced (by the prechecked op) and the
/// `DictShapeGuard` produces no SSA — so the trace's SSA contract
/// is preserved without renaming any downstream uses.
///
/// Note: guard `trace_pc` shifts by 1 for every op at index >=
/// `lookup_pc`. We don't fix them up here because the subsequent
/// LICM pass calls `rebind_guard_pcs` after its own rewrites, and
/// the trace_install pipeline does one more rebind round before
/// emission. Re-running `LICM` (which the standard pipeline does
/// after this pass) handles the bookkeeping uniformly.
fn apply_split(trace: &mut TraceBuffer, target: RewriteTarget) {
    let RewriteTarget {
        lookup_pc,
        dst,
        dict_ptr,
        key_ptr,
        shape_hash,
    } = target;

    // Replace the original op in place with the prechecked form.
    trace.ops[lookup_pc] = TraceOp::DictLookupPrechecked {
        dst,
        dict_ptr,
        key_ptr,
    };

    // Insert the shape guard immediately before the prechecked op.
    // We use `insert` so trailing ops shift by exactly +1, matching
    // the doc-comment above.
    let guard = TraceOp::DictShapeGuard {
        dict_ptr,
        shape_hash,
    };
    trace.ops.insert(lookup_pc, guard);

    // Shift all existing guard `trace_pc`s that point at or past
    // `lookup_pc` by +1 so they keep pointing at their original op.
    for site in &mut trace.guards {
        if site.trace_pc as usize >= lookup_pc {
            site.trace_pc += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace_ir::{CmpKind, LoopPhi};

    fn mk() -> TraceBuffer {
        TraceBuffer::new()
    }

    /// Build a minimal in-loop trace:
    ///
    /// ```text
    ///   dict = LocalGet 1       // SSA 0 — defined OUTSIDE the loop
    ///   MarkLoopHead loop_id=0  // empty phis
    ///     key  = LocalGet 2     // SSA 1 — also outside-invariant
    ///     val  = DictLookup dict key shape=0xabc  // SSA 2
    ///   MarkLoopBack loop_id=0
    /// ```
    ///
    /// Both inputs to the DictLookup are loop-invariant; the pass
    /// should split it and the resulting `DictShapeGuard` should
    /// land at the pc preceding the original lookup.
    fn build_simple_dict_loop() -> TraceBuffer {
        let mut b = mk();
        let dict = b.fresh_ssa();
        let key = b.fresh_ssa();
        let val = b.fresh_ssa();
        b.append(TraceOp::LocalGet(dict, 1));
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![],
        });
        b.append(TraceOp::LocalGet(key, 2));
        b.append(TraceOp::DictLookup {
            dst: val,
            dict_ptr: dict,
            key_ptr: key,
            shape_hash: 0xabc,
        });
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![],
        });
        b
    }

    #[test]
    fn hoist_splits_loop_invariant_dict_lookup() {
        let mut b = build_simple_dict_loop();
        let before = b.ops.len();
        let report = DictIcHoist.run(&mut b);
        assert!(report.touched(), "pass must report a rewrite");
        assert_eq!(b.ops.len(), before + 1, "exactly one new op inserted");

        // Expect: LocalGet, MarkLoopHead, LocalGet, DictShapeGuard,
        // DictLookupPrechecked, MarkLoopBack.
        match &b.ops[3] {
            TraceOp::DictShapeGuard {
                dict_ptr,
                shape_hash,
            } => {
                assert_eq!(*dict_ptr, SsaVar(0));
                assert_eq!(*shape_hash, 0xabc);
            }
            other => panic!("expected DictShapeGuard at index 3, got {:?}", other),
        }
        match &b.ops[4] {
            TraceOp::DictLookupPrechecked {
                dst,
                dict_ptr,
                key_ptr,
            } => {
                assert_eq!(*dst, SsaVar(2));
                assert_eq!(*dict_ptr, SsaVar(0));
                assert_eq!(*key_ptr, SsaVar(1));
            }
            other => panic!("expected DictLookupPrechecked at index 4, got {:?}", other),
        }
    }

    #[test]
    fn hoist_skips_when_dict_ptr_is_loop_carried() {
        // Build a loop where `dict_ptr` is the phi-rebound SSA — i.e.
        // the dict pointer changes every iteration. The pass MUST
        // NOT rewrite this case, otherwise a hoisted shape guard
        // would check the wrong dict on the second iter onward.
        let mut b = mk();
        let init = b.fresh_ssa();
        let phi = b.fresh_ssa();
        let key = b.fresh_ssa();
        let val = b.fresh_ssa();
        b.append(TraceOp::LocalGet(init, 0));
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![LoopPhi::new(init, phi)],
        });
        b.append(TraceOp::LocalGet(key, 1));
        b.append(TraceOp::DictLookup {
            dst: val,
            dict_ptr: phi, // ← varies every iter (loop-carried)
            key_ptr: key,
            shape_hash: 0xdef,
        });
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![phi],
        });
        let report = DictIcHoist.run(&mut b);
        assert!(
            !report.touched(),
            "loop-carried dict_ptr must not be rewritten"
        );
        // Op count unchanged.
        assert!(matches!(b.ops[3], TraceOp::DictLookup { .. }));
    }

    /// F-D8-E.2: even when `LocalGet(dict_slot)` lives INSIDE the
    /// loop body — which is what the recorder emits for the W5
    /// trace before LICM has had a chance to hoist it — the pass
    /// must still recognise `dict_ptr` as loop-invariant. Otherwise
    /// the rewrite never fires on the actual workload and the W5
    /// trace_jit ratio stays exactly where F-D8-E.1 left it.
    #[test]
    fn hoist_recognises_in_loop_local_get_as_invariant() {
        let mut b = mk();
        let dict = b.fresh_ssa();
        let key = b.fresh_ssa();
        let val = b.fresh_ssa();
        // Note: NO pre-loop LocalGet here. Both LocalGet ops live
        // inside the loop body, mirroring the recorder's emit order
        // for `for i in 0..n { acc += d[keys[i]] }`.
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![],
        });
        b.append(TraceOp::LocalGet(dict, 1));
        b.append(TraceOp::LocalGet(key, 2));
        b.append(TraceOp::DictLookup {
            dst: val,
            dict_ptr: dict,
            key_ptr: key,
            shape_hash: 0xcafe,
        });
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![],
        });

        let report = DictIcHoist.run(&mut b);
        assert!(
            report.touched(),
            "in-loop LocalGet should be treated as invariant"
        );
        assert!(
            b.ops
                .iter()
                .any(|op| matches!(op, TraceOp::DictShapeGuard { .. })),
            "DictShapeGuard must have been inserted"
        );
    }

    #[test]
    fn hoist_skips_dict_lookup_outside_any_loop() {
        // No MarkLoopHead/Back markers — the pass should leave the
        // straight-line DictLookup alone (no perf gain to chase,
        // and the prechecked form would still need its own shape
        // guard).
        let mut b = mk();
        let dict = b.fresh_ssa();
        let key = b.fresh_ssa();
        let val = b.fresh_ssa();
        b.append(TraceOp::LocalGet(dict, 1));
        b.append(TraceOp::LocalGet(key, 2));
        b.append(TraceOp::DictLookup {
            dst: val,
            dict_ptr: dict,
            key_ptr: key,
            shape_hash: 0xfeed,
        });
        let report = DictIcHoist.run(&mut b);
        assert!(
            !report.touched(),
            "straight-line DictLookup must not be rewritten"
        );
    }

    #[test]
    fn hoist_handles_multiple_invariant_lookups_in_one_loop() {
        // Two lookups sharing the same dict_ptr should each get
        // their own DictShapeGuard. (LICM will then deduplicate
        // when it lifts both probes — the redundant-guard cleanup
        // is LICM's job; this pass only emits guards in 1:1 pairs
        // with each `DictLookup`.)
        let mut b = mk();
        let dict = b.fresh_ssa();
        let key1 = b.fresh_ssa();
        let key2 = b.fresh_ssa();
        let val1 = b.fresh_ssa();
        let val2 = b.fresh_ssa();
        b.append(TraceOp::LocalGet(dict, 1));
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![],
        });
        b.append(TraceOp::LocalGet(key1, 2));
        b.append(TraceOp::DictLookup {
            dst: val1,
            dict_ptr: dict,
            key_ptr: key1,
            shape_hash: 0xa,
        });
        b.append(TraceOp::LocalGet(key2, 3));
        b.append(TraceOp::DictLookup {
            dst: val2,
            dict_ptr: dict,
            key_ptr: key2,
            shape_hash: 0xa,
        });
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![],
        });

        let report = DictIcHoist.run(&mut b);
        assert!(report.touched());

        let guards = b
            .ops
            .iter()
            .filter(|op| matches!(op, TraceOp::DictShapeGuard { .. }))
            .count();
        let prechecked = b
            .ops
            .iter()
            .filter(|op| matches!(op, TraceOp::DictLookupPrechecked { .. }))
            .count();
        assert_eq!(guards, 2);
        assert_eq!(prechecked, 2);
        assert!(
            !b.ops
                .iter()
                .any(|op| matches!(op, TraceOp::DictLookup { .. })),
            "no DictLookup ops should remain after the rewrite"
        );
    }

    /// Round-trip with LICM: after `dict_ic_hoist` + `LICM`, the
    /// `DictShapeGuard` should sit BEFORE the `MarkLoopHead`, and
    /// the `DictLookupPrechecked` should remain inside the body.
    #[test]
    fn licm_lifts_dict_shape_guard_out_of_loop() {
        let mut b = build_simple_dict_loop();
        DictIcHoist.run(&mut b);
        crate::optimizer::licm::LICM.run(&mut b);

        // Find the indices of the two ops + the loop head.
        let head_pc = b
            .ops
            .iter()
            .position(|op| matches!(op, TraceOp::MarkLoopHead { .. }))
            .expect("loop head must still be present");
        let guard_pc = b
            .ops
            .iter()
            .position(|op| matches!(op, TraceOp::DictShapeGuard { .. }))
            .expect("shape guard must still be present");
        let prechecked_pc = b
            .ops
            .iter()
            .position(|op| matches!(op, TraceOp::DictLookupPrechecked { .. }))
            .expect("prechecked op must still be present");
        assert!(
            guard_pc < head_pc,
            "LICM must hoist DictShapeGuard (pc {guard_pc}) ahead of MarkLoopHead (pc {head_pc})"
        );
        assert!(
            prechecked_pc > head_pc,
            "DictLookupPrechecked must stay inside the loop body"
        );
    }

    /// W5-shaped round-trip: `LocalGet(dict_slot)` and
    /// `LocalGet(key_slot)` both live INSIDE the loop body the
    /// recorder emits. After `dict_ic_hoist` + `LICM`, the
    /// `DictShapeGuard` AND the LocalGet that feeds it must end up
    /// above the `MarkLoopHead`. This is the bench-critical case —
    /// without the in-loop-LocalGet invariance recognition the
    /// shape guard never lifts and the F-D8-E.2 perf win
    /// evaporates.
    #[test]
    fn licm_lifts_shape_guard_when_local_get_starts_inside_loop() {
        let mut b = mk();
        let dict = b.fresh_ssa();
        let key = b.fresh_ssa();
        let val = b.fresh_ssa();
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![],
        });
        b.append(TraceOp::LocalGet(dict, 1));
        b.append(TraceOp::LocalGet(key, 2));
        b.append(TraceOp::DictLookup {
            dst: val,
            dict_ptr: dict,
            key_ptr: key,
            shape_hash: 0xdead_beef,
        });
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![],
        });

        DictIcHoist.run(&mut b);
        crate::optimizer::licm::LICM.run(&mut b);

        let head_pc = b
            .ops
            .iter()
            .position(|op| matches!(op, TraceOp::MarkLoopHead { .. }))
            .expect("loop head present");
        let guard_pc = b
            .ops
            .iter()
            .position(|op| matches!(op, TraceOp::DictShapeGuard { .. }))
            .expect("shape guard present");
        assert!(
            guard_pc < head_pc,
            "DictShapeGuard (pc {guard_pc}) must be above MarkLoopHead (pc {head_pc}) \
             even when its source `LocalGet` started inside the body"
        );
    }

    /// Defensive smoke: a `Cmp` op sandwich around the rewrite must
    /// stay structurally intact — the inserted guard shouldn't bump
    /// SSA ids or shuffle the operand order of unrelated ops.
    #[test]
    fn unrelated_ops_unaffected() {
        let mut b = mk();
        let a = b.fresh_ssa();
        let bv = b.fresh_ssa();
        let cmp_dst = b.fresh_ssa();
        let dict = b.fresh_ssa();
        let key = b.fresh_ssa();
        let val = b.fresh_ssa();
        b.append(TraceOp::ConstI64(a, 1));
        b.append(TraceOp::ConstI64(bv, 2));
        b.append(TraceOp::Cmp(CmpKind::Eq, cmp_dst, a, bv));
        b.append(TraceOp::LocalGet(dict, 1));
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![],
        });
        b.append(TraceOp::LocalGet(key, 2));
        b.append(TraceOp::DictLookup {
            dst: val,
            dict_ptr: dict,
            key_ptr: key,
            shape_hash: 0x1,
        });
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![],
        });

        DictIcHoist.run(&mut b);

        // The Cmp op's operand SSAs should be unchanged.
        match b.ops[2] {
            TraceOp::Cmp(kind, dst, x, y) => {
                assert_eq!(kind, CmpKind::Eq);
                assert_eq!(dst, cmp_dst);
                assert_eq!(x, a);
                assert_eq!(y, bv);
            }
            ref other => panic!("Cmp op must remain at index 2, got {:?}", other),
        }
    }
}
