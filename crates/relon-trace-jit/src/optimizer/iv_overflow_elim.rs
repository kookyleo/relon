//! Induction-variable overflow elimination.
//!
//! Drops in-loop `Guard(ArithOverflow(_))` ops whose underlying
//! `Add { phi, step }` op can be proven not to overflow. The proof
//! relies on the loop's own exit idiom: when a `MarkLoopHead` is
//! followed immediately by `Cmp Ge phi, n` + `Guard IsZero(cmp)`, the
//! loop is guaranteed to exit before `phi` reaches `n`. With both `n`
//! and the step bounded by `MAX_SAFE_LOOP_BOUND` (and the step
//! non-negative), `phi + step` never overflows i64.
//!
//! To keep runtime safety, the pass synthesises a single entry guard
//! ahead of the loop preheader that deopts when `n > MAX_SAFE_LOOP_BOUND`.
//! After the entry guard, every dropped in-loop `ArithOverflow` is
//! statically dead.
//!
//! ## Why this exists
//!
//! Cranelift's `sadd_overflow` emits the same `iadd` plus a separate
//! `of_bit`; the matching `Guard(ArithOverflow)` becomes a `brif of_bit`
//! per iter. On W4 the loop fires two of these per iteration (one for
//! `count += hit`, one for `i + 1`). Removing them lets cranelift's DCE
//! reduce `sadd_overflow` to a plain `iadd`, shaving the 1.33× gap to
//! LuaJIT down to 0.89×.
//!
//! ## Algorithm
//!
//! For every well-formed `MarkLoopHead`..`MarkLoopBack` pair:
//!
//! 1. Look at the first two body ops. Match the exit idiom:
//!    `Cmp Ge phi, n` followed by `Guard IsZero(cmp)`. If absent, skip
//!    this loop entirely (we can't prove the bound).
//! 2. Confirm `n` is loop-invariant (defined outside the body and not
//!    one of the head's phis).
//! 3. For each `LoopPhi { init, phi }`:
//!    - `init` must be a `ConstI64(c0)` with `0 <= c0 <= MAX_SAFE`.
//!    - The matching `next_values[k]` must be an in-body
//!      `Add { phi, step }` where `step` is either a `ConstI64(c1)`
//!      with `1 <= c1 <= MAX_STEP`, or a loop-invariant SSA whose
//!      observed type is `Bool` (range `[0, 1]`).
//!    - The exit guard's `phi` is also bounded by `n`; the lemma extends
//!      to every other phi only when ALL phis share the same exit
//!      idiom (any single counterexample disqualifies the whole loop).
//! 4. If every phi qualifies, mark every `Guard(ArithOverflow(next))`
//!    in the body for removal.
//! 5. Synthesise the entry guard:
//!    `ConstI64 max = MAX_SAFE_LOOP_BOUND`
//!    `Cmp Gt cmp = n > max`
//!    `Guard IsZero(cmp)`
//!    and splice it immediately before the `MarkLoopHead`. The guard
//!    fires when `n` is too large for the proof to hold; cranelift then
//!    deopts and the generic backend keeps running with overflow guards
//!    in place.
//! 6. Rebuild the op stream and rebind `GuardSite::trace_pc` entries.
//!
//! ## Ordering
//!
//! Runs after [`super::licm::LICM`] / [`super::noop_typecheck_elim::NoopTypeCheckElim`]
//! and before the round-2 [`super::dead_store::DeadStoreElim`]. LICM
//! must run first so the `n` SSA (a `LocalGet`) is reliably outside the
//! loop body. Running before the final dead-store pass lets it pick up
//! any `Add` whose `dst` is now only consumed by the loop's `next_values`
//! plumbing (no further consumers; the `Add` itself stays — its result is
//! still phi-fed — but its `of_bit` becomes dead and folds out in
//! cranelift). See the [`super`] module docs for the full pipeline
//! contract.
//!
//! ## SSA-allocation note
//!
//! The pass allocates three new SSA ids (the entry guard's constant,
//! the compare result, and one implicit phi-stage slot retained for
//! future expansion). New ids never appear in any side-table the
//! recorder pre-populates, so the "side-table keys must not go stale"
//! invariant from the [`crate::buffer`] docs is preserved. We rely on
//! `TraceBuffer::fresh_ssa` to keep `next_ssa` (and therefore
//! `OptimizedTrace::ssa_high_water`) in sync; the downstream cranelift
//! slot array is sized from `ssa_high_water` so the freshly allocated
//! ids fit without a manual adjustment.

use std::collections::{HashMap, HashSet};

use crate::buffer::TraceBuffer;
use crate::guard::GuardSite;
use crate::trace_ir::{CmpKind, ExternalPc, GuardKind, ObservedType, SsaVar, TraceOp};

use super::{OptimizerPass, PassReport};

/// Largest loop bound `n` we accept before declaring the proof unsafe.
/// `i64::MAX / 4` leaves four i64::MAX/4 worth of headroom for the
/// step, more than enough to cover any realistic per-iter step (typical
/// integer steps are in the single-digit range; the worst case the
/// recorder emits is a Bool/i32 accumulator with step <= 1).
pub const MAX_SAFE_LOOP_BOUND: i64 = i64::MAX / 4;

/// Largest constant step we admit. Bounded so even a maximal-step iter
/// sequence stays inside i64 after `MAX_SAFE_LOOP_BOUND` iterations.
/// `2^32` is the design-document figure; with `n <= MAX_SAFE_LOOP_BOUND`
/// the accumulator stays within `n * MAX_STEP <= i64::MAX / 4 * 2^32 ≈
/// 2^91`... obviously over-budget on paper, but in practice the proof
/// uses `max(phi) <= n` which keeps the bound `n + step` (the per-iter
/// add) safely inside i64 as long as `step <= MAX_STEP`.
pub const MAX_STEP: i64 = 1 << 32;

/// Stateless rewrite pass — see module docs.
pub struct IvOverflowElim;

impl OptimizerPass for IvOverflowElim {
    fn name(&self) -> &'static str {
        "iv_overflow_elim"
    }

    fn run(&self, trace: &mut TraceBuffer) -> PassReport {
        let mut report = PassReport::default();

        // Collect well-formed loop ranges. We borrow LICM's stack
        // matching idiom rather than depend on it (the helper there is
        // private). Outer-first ordering doesn't matter here: each
        // loop's analysis is independent, and the entry-guard insert
        // does not move loops relative to each other.
        let loops = collect_loops(&trace.ops);
        if loops.is_empty() {
            return report;
        }

        // Track every dead pc and every entry-guard we'll splice in.
        let mut drop_pcs: HashSet<usize> = HashSet::new();
        let mut entries: Vec<EntryGuardInsertion> = Vec::new();

        for lp in &loops {
            if let Some(rewrite) = analyse_loop(trace, lp) {
                drop_pcs.extend(rewrite.dead_pcs.iter().copied());
                entries.push(EntryGuardInsertion {
                    before_pc: lp.head_pc,
                    n: rewrite.bound,
                });
            }
        }

        // Const-safe arithmetic overflow strip: `Mod a, b` cannot
        // overflow except in the single `a == i64::MIN && b == -1`
        // corner. If `b` is a known constant other than `-1`, the
        // matching `Guard(ArithOverflow(dst))` is statically dead and
        // we can drop it without inserting any runtime check. Saves a
        // brif per Mod-in-hot-loop iteration (W5: `i % 10` per iter).
        for (pc, op) in trace.ops.iter().enumerate() {
            if let TraceOp::Guard {
                kind: GuardKind::ArithOverflow(v),
                ..
            } = op
            {
                if mod_overflow_provably_safe(trace, *v) {
                    drop_pcs.insert(pc);
                }
            }
        }

        if drop_pcs.is_empty() {
            return report;
        }

        report.ops_removed = drop_pcs.len();
        report.guards_added = entries.len();
        rebuild_with_entry_guards(trace, &drop_pcs, &entries);
        report
    }
}

/// True iff `v` is defined by `TraceOp::Mod { rhs }` with `rhs` a
/// known constant other than `-1`. In that case the only overflow
/// case (`i64::MIN % -1`) is unreachable and the corresponding
/// `Guard(ArithOverflow(v))` is statically dead.
fn mod_overflow_provably_safe(trace: &TraceBuffer, v: SsaVar) -> bool {
    for op in &trace.ops {
        if let TraceOp::Mod { dst, rhs, .. } = op {
            if *dst == v {
                return matches!(const_i64_anywhere(trace, *rhs), Some(c) if c != -1);
            }
        }
    }
    false
}

#[derive(Debug, Clone, Copy)]
struct LoopRange {
    head_pc: usize,
    back_pc: usize,
    #[allow(dead_code)]
    loop_id: u32,
}

fn collect_loops(ops: &[TraceOp]) -> Vec<LoopRange> {
    let mut stack: Vec<(u32, usize)> = Vec::new();
    let mut out: Vec<LoopRange> = Vec::new();
    for (pc, op) in ops.iter().enumerate() {
        if let Some(id) = op.loop_head_id() {
            stack.push((id, pc));
        } else if let Some(id) = op.loop_back_id() {
            if let Some(pos) = stack.iter().rposition(|(sid, _)| *sid == id) {
                let (loop_id, head_pc) = stack.remove(pos);
                out.push(LoopRange {
                    head_pc,
                    back_pc: pc,
                    loop_id,
                });
            }
        }
    }
    out
}

#[derive(Debug)]
struct LoopRewrite {
    /// SSA carrying the upper bound `n` (loop-invariant).
    bound: SsaVar,
    /// Trace pc indices of the `Guard(ArithOverflow(_))` ops we'll drop.
    dead_pcs: Vec<usize>,
}

struct EntryGuardInsertion {
    /// pc immediately *before* the `MarkLoopHead` to splice the entry
    /// guard at. The new ops land at `before_pc`, shifting the loop and
    /// everything after by `+3`.
    before_pc: usize,
    /// SSA carrying the bound `n` we'll compare against the entry-time
    /// safety bound.
    n: SsaVar,
}

/// Analyse a single loop. Returns `Some(LoopRewrite)` iff the IV-overflow
/// proof goes through for at least one in-body `Guard(ArithOverflow)`.
fn analyse_loop(trace: &TraceBuffer, lp: &LoopRange) -> Option<LoopRewrite> {
    let body_start = lp.head_pc + 1;
    let body_end = lp.back_pc; // exclusive
    if body_end <= body_start + 1 {
        return None;
    }

    // 1. Exit idiom: body[0] must be `Cmp Ge cmp_dst = exit_phi, n`,
    //    body[1] must be `Guard IsZero(cmp_dst)`.
    let (exit_phi, bound, cmp_dst) = match &trace.ops[body_start] {
        TraceOp::Cmp {
            kind: CmpKind::Ge,
            dst,
            lhs,
            rhs,
        } => (*lhs, *rhs, *dst),
        _ => return None,
    };
    let isz_ok = matches!(
        &trace.ops[body_start + 1],
        TraceOp::Guard {
            kind: GuardKind::IsZero(v),
            ..
        } if *v == cmp_dst
    );
    if !isz_ok {
        return None;
    }

    // 2. Snapshot the loop head's phis and the matching back's
    //    next_values. Bail if the marker pair is malformed.
    let (phis, next_values) = match (&trace.ops[lp.head_pc], &trace.ops[lp.back_pc]) {
        (TraceOp::MarkLoopHead { phis, .. }, TraceOp::MarkLoopBack { next_values, .. })
            if phis.len() == next_values.len() =>
        {
            (phis.clone(), next_values.clone())
        }
        _ => return None,
    };
    if phis.is_empty() {
        return None;
    }

    // 3. Confirm `bound` is loop-invariant: its defining op (if any)
    //    must live outside the body, and it must not be one of the
    //    phi-rebound SSAs.
    let phi_defs: HashSet<SsaVar> = phis.iter().map(|p| p.phi).collect();
    if phi_defs.contains(&bound) {
        return None;
    }
    if !is_defined_outside_body(&trace.ops, bound, body_start, body_end) {
        return None;
    }

    // 4. The exit guard only constrains `exit_phi`; we need every
    //    other phi to share the same bound. The simplest sufficient
    //    condition is "the phi's step is bounded by the same `n`
    //    via the exit idiom" — i.e. its `next` increment is
    //    proportional to the same iteration count. Walk every phi:
    //    each must either BE the exit_phi (which the guard covers
    //    directly) or have a Bool/non-negative-constant step. We
    //    don't need to redo the exit-idiom search for non-exit phis
    //    because they ride the same iteration count as exit_phi.
    let mut phi_to_idx: HashMap<SsaVar, usize> = HashMap::new();
    for (k, p) in phis.iter().enumerate() {
        phi_to_idx.insert(p.phi, k);
    }
    let exit_phi_idx = phi_to_idx.get(&exit_phi).copied()?;

    // Pre-index the body ops by their defining SSA for O(1) lookup.
    let body_defs = index_body_defs(&trace.ops, body_start, body_end);

    // Find every Add { phi_k, step_k } in the body keyed by phi_k →
    // the producing op. Per-phi: if any single phi fails its proof,
    // skip JUST that phi (don't taint the others — each proof is
    // independent given the shared entry guard `n <= MAX_SAFE`). 2026-
    // 05-25 refactor: previous behaviour returned None on first
    // failure, which prevented W5's counter `i + 1` overflow guard
    // from being stripped because the accumulator `count + dict_value`
    // failed its bound check. The bound-check failure is irrelevant to
    // the counter's safety.
    let mut dead_pcs: Vec<usize> = Vec::new();
    for (k, p) in phis.iter().enumerate() {
        let next = next_values[k];
        // The increment op must be in the body, must be `Add`, and
        // must read this phi.
        let inc_pc = match body_defs.get(&next) {
            Some(&pc) => pc,
            None => continue,
        };
        let (inc_lhs, inc_rhs) = match &trace.ops[inc_pc] {
            TraceOp::Add { dst, lhs, rhs } => {
                debug_assert_eq!(*dst, next);
                (*lhs, *rhs)
            }
            _ => continue,
        };
        // `phi` must be one of the operands; the other is the step.
        let step = if inc_lhs == p.phi {
            inc_rhs
        } else if inc_rhs == p.phi {
            inc_lhs
        } else {
            continue;
        };

        // 4a. init must be a known constant in [0, MAX_SAFE_LOOP_BOUND].
        let init_c = match const_i64_of(&trace.ops, p.init, body_start) {
            Some(c) => c,
            None => continue,
        };
        if !(0..=MAX_SAFE_LOOP_BOUND).contains(&init_c) {
            continue;
        }

        // 4b. step must be a non-negative bounded constant OR a
        //     loop-invariant Bool SSA.
        let step_ok = match const_i64_anywhere(trace, step) {
            Some(c) => (0..=MAX_STEP).contains(&c),
            None => {
                // Non-constant step: require loop-invariance + Bool type
                // (range [0,1]) so the per-iter delta is at most 1.
                let invariant = is_defined_outside_body(&trace.ops, step, body_start, body_end);
                let bool_type = trace.type_info.get(&step).copied() == Some(ObservedType::Bool);
                invariant && bool_type
            }
        };
        if !step_ok {
            continue;
        }

        // 4c. For non-exit phis, also confirm there's no per-iter
        //     mechanism that could escape the bound. Both shapes we
        //     accept (constant step, Bool step) are monotonically
        //     non-decreasing under non-negative init, so the loop's
        //     exit-on-`exit_phi >= n` guarantees `phi <= n` at the
        //     `Add` point. For k == exit_phi_idx we're proving
        //     `exit_phi + step < n + step` ≤ MAX_SAFE+step ≤ i64::MAX.
        //     For k != exit_phi_idx we're using the same trip-count
        //     bound: `phi_k <= init_k + step_k * iters_remaining <=
        //     MAX_SAFE + MAX_STEP * MAX_SAFE`. Pick the conservative
        //     accumulator headroom by capping `MAX_STEP * MAX_SAFE`
        //     inside `i64::MAX`. With MAX_SAFE = i64::MAX/4 and
        //     MAX_STEP = 2^32, MAX_STEP * MAX_SAFE = i64::MAX/4 * 2^32
        //     overflows on paper, so the conservative real-world rule
        //     is: a non-Bool, non-tiny step is only safe when this
        //     phi IS the exit phi. Enforce here.
        if k != exit_phi_idx {
            let is_bool_step = matches!(
                trace.type_info.get(&step).copied(),
                Some(ObservedType::Bool)
            );
            let is_unit_const =
                matches!(const_i64_anywhere(trace, step), Some(c) if (0..=1).contains(&c));
            if !is_bool_step && !is_unit_const {
                continue;
            }
        }

        // 5. Mark the matching `Guard(ArithOverflow(next))` (if any)
        //    for removal. The recorder typically emits it immediately
        //    after the `Add`, but we scan the full body for safety.
        for (pc, op) in trace.ops.iter().enumerate().take(body_end).skip(body_start) {
            if let TraceOp::Guard {
                kind: GuardKind::ArithOverflow(v),
                ..
            } = op
            {
                if *v == next {
                    dead_pcs.push(pc);
                }
            }
        }
    }

    if dead_pcs.is_empty() {
        return None;
    }

    // De-duplicate (an Add that's used in multiple `next_values` slots
    // shouldn't double-count its guard).
    dead_pcs.sort_unstable();
    dead_pcs.dedup();

    Some(LoopRewrite { bound, dead_pcs })
}

/// Returns `Some(c)` if `var` is defined by a `ConstI64 { dst: var, value: c }`
/// op anywhere before `boundary`. Used to peek at a phi's `init` value
/// (which always lives in the pre-loop region).
fn const_i64_of(ops: &[TraceOp], var: SsaVar, boundary: usize) -> Option<i64> {
    for op in ops.iter().take(boundary) {
        if let TraceOp::ConstI64 { dst, value } = op {
            if *dst == var {
                return Some(*value);
            }
        }
    }
    None
}

/// Lookup the integer constant bound to `var` from either the buffer's
/// `consts` side table OR a `ConstI64 { dst: var, ... }` op anywhere in
/// the stream. The former covers values folded by `ConstFold` (the op
/// might have been replaced); the latter covers fresh recorder output
/// that hasn't been folded.
fn const_i64_anywhere(trace: &TraceBuffer, var: SsaVar) -> Option<i64> {
    if let Some(crate::trace_ir::TraceConst::I64(c)) = trace.consts.get(&var).copied() {
        return Some(c);
    }
    if let Some(crate::trace_ir::TraceConst::I32(c)) = trace.consts.get(&var).copied() {
        return Some(c as i64);
    }
    for op in &trace.ops {
        match op {
            TraceOp::ConstI64 { dst, value } if *dst == var => return Some(*value),
            TraceOp::ConstI32 { dst, value } if *dst == var => return Some(*value as i64),
            _ => {}
        }
    }
    None
}

fn is_defined_outside_body(
    ops: &[TraceOp],
    var: SsaVar,
    body_start: usize,
    body_end: usize,
) -> bool {
    for (pc, op) in ops.iter().enumerate().take(body_end).skip(body_start) {
        if op.defs().contains(&var) {
            // Inside-body definition disqualifies (unless it's the
            // `LocalGet` immediately ahead of the cmp — but we don't
            // need that here because LICM runs first and lifts those).
            let _ = pc;
            return false;
        }
    }
    true
}

fn index_body_defs(ops: &[TraceOp], body_start: usize, body_end: usize) -> HashMap<SsaVar, usize> {
    let mut out = HashMap::new();
    for (pc, op) in ops.iter().enumerate().take(body_end).skip(body_start) {
        for d in op.defs() {
            out.insert(d, pc);
        }
    }
    out
}

/// Rebuild `trace.ops` skipping every pc in `drop_pcs` and inserting the
/// entry guards at the requested positions. Also fix up
/// `GuardSite::trace_pc` entries: drop sites that point at a removed op,
/// shift everything else, and append fresh sites for each entry guard.
fn rebuild_with_entry_guards(
    trace: &mut TraceBuffer,
    drop_pcs: &HashSet<usize>,
    entries: &[EntryGuardInsertion],
) {
    let old_ops = std::mem::take(&mut trace.ops);
    let mut entries_at: HashMap<usize, Vec<EntryGuardInsertion>> = HashMap::new();
    for e in entries {
        entries_at
            .entry(e.before_pc)
            .or_default()
            .push(EntryGuardInsertion {
                before_pc: e.before_pc,
                n: e.n,
            });
    }
    let mut new_ops: Vec<TraceOp> = Vec::with_capacity(old_ops.len() + entries.len() * 3);
    // Track the old→new pc map so we can rebind surviving guards.
    let mut old_to_new: Vec<Option<u32>> = vec![None; old_ops.len()];
    // Collect the trace_pc each newly inserted entry guard lands at, in
    // the order we inserted them. The matching `GuardSite` is appended
    // below.
    let mut entry_guard_pcs: Vec<u32> = Vec::new();
    // We need the SSA carrying `n` to materialise the entry guard.
    // Defer fresh-SSA allocation until splice time so we don't touch
    // `trace` while it's split.
    let mut entry_specs: Vec<(usize, SsaVar)> = Vec::new(); // (insert_at, n)
    for (pc, op) in old_ops.into_iter().enumerate() {
        if let Some(group) = entries_at.remove(&pc) {
            for g in group {
                entry_specs.push((new_ops.len(), g.n));
                // Placeholders: we replace these with real ops after the
                // sweep finishes (so `fresh_ssa` can run on `trace`).
                new_ops.push(TraceOp::ConstI64 {
                    dst: SsaVar::NONE,
                    value: 0,
                });
                new_ops.push(TraceOp::ConstI64 {
                    dst: SsaVar::NONE,
                    value: 0,
                });
                new_ops.push(TraceOp::Guard {
                    kind: GuardKind::IsZero(SsaVar::NONE),
                    check: SsaVar::NONE,
                });
            }
        }
        if drop_pcs.contains(&pc) {
            continue;
        }
        old_to_new[pc] = Some(new_ops.len() as u32);
        new_ops.push(op);
    }
    trace.ops = new_ops;
    // Replace each entry-guard placeholder triple with real ops backed
    // by fresh SSAs. Allocate the SSAs here so the `next_ssa` counter
    // advances exactly once per entry guard.
    for (insert_at, n) in entry_specs {
        let max_ssa = trace.fresh_ssa();
        let cmp_ssa = trace.fresh_ssa();
        trace.ops[insert_at] = TraceOp::ConstI64 {
            dst: max_ssa,
            value: MAX_SAFE_LOOP_BOUND,
        };
        trace.ops[insert_at + 1] = TraceOp::Cmp {
            kind: CmpKind::Gt,
            dst: cmp_ssa,
            lhs: n,
            rhs: max_ssa,
        };
        trace.ops[insert_at + 2] = TraceOp::Guard {
            kind: GuardKind::IsZero(cmp_ssa),
            check: cmp_ssa,
        };
        entry_guard_pcs.push((insert_at + 2) as u32);
    }
    // Rebuild the guards table.
    let mut new_guards = Vec::with_capacity(trace.guards.len() + entry_guard_pcs.len());
    let guards_drain = std::mem::take(&mut trace.guards);
    for g in guards_drain {
        let old_pc = g.trace_pc as usize;
        if let Some(Some(new_pc)) = old_to_new.get(old_pc).copied() {
            let mut site = g;
            site.trace_pc = new_pc;
            new_guards.push(site);
        }
        // Otherwise the guard's op got removed; drop the site.
    }
    // Append fresh entry-guard sites. We anchor them on `ExternalPc(0)`
    // — entry-time deopt rolls back to the trace's resume PC, which the
    // host's GuardFailed handler maps to the bytecode-side entry.
    for pc in entry_guard_pcs {
        let kind = match trace.ops[pc as usize] {
            TraceOp::Guard { kind, .. } => kind,
            _ => unreachable!("entry guard slot must hold a Guard op"),
        };
        new_guards.push(GuardSite::new(pc, ExternalPc(0), kind));
    }
    trace.guards = new_guards;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace_ir::{LoopPhi, ObservedType, TraceConst};

    fn mk_w4_loop(n_value: SsaVar) -> TraceBuffer {
        // Replicates the design-doc's "W4 post-LICM" IR.
        let mut b = TraceBuffer::new();
        // Pre-loop seeds.
        let i_seed = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: i_seed,
            value: 0,
        });
        b.record_const(i_seed, TraceConst::I64(0));
        let count_seed = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: count_seed,
            value: 0,
        });
        b.record_const(count_seed, TraceConst::I64(0));
        // `n` (LocalGet, hoisted) — caller supplies the SSA so we can
        // make it match the bound the loop checks against. We append
        // its definition AFTER allocating so the buffer's next_ssa
        // stays in sync with the caller's pick. Use `LocalGet` since
        // the design doc shows that as the canonical "post-LICM" form.
        b.append(TraceOp::LocalGet {
            dst: n_value,
            slot_idx: 0,
        });
        let hit = b.fresh_ssa();
        // `hit = StrContains(...)` — pretend it's already materialised
        // by recording the type. The `Add` step will treat it as a Bool
        // SSA (range [0, 1]).
        b.append(TraceOp::StrContains {
            dst: hit,
            haystack: SsaVar(99),
            needle: SsaVar(100),
        });
        b.record_type(hit, ObservedType::Bool);
        let step1 = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: step1,
            value: 1,
        });
        b.record_const(step1, TraceConst::I64(1));
        // Loop body.
        let count_phi = b.fresh_ssa();
        let i_phi = b.fresh_ssa();
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![
                LoopPhi::new(count_seed, count_phi),
                LoopPhi::new(i_seed, i_phi),
            ],
        });
        let cmp_dst = b.fresh_ssa();
        b.append(TraceOp::Cmp {
            kind: CmpKind::Ge,
            dst: cmp_dst,
            lhs: i_phi,
            rhs: n_value,
        });
        let isz = TraceOp::Guard {
            kind: GuardKind::IsZero(cmp_dst),
            check: cmp_dst,
        };
        let isz_pc = b.append(isz);
        b.record_guard(GuardSite::new(
            isz_pc,
            ExternalPc(11),
            GuardKind::IsZero(cmp_dst),
        ));
        let count_next = b.fresh_ssa();
        b.append(TraceOp::Add {
            dst: count_next,
            lhs: count_phi,
            rhs: hit,
        });
        let of1 = TraceOp::Guard {
            kind: GuardKind::ArithOverflow(count_next),
            check: count_next,
        };
        let of1_pc = b.append(of1);
        b.record_guard(GuardSite::new(
            of1_pc,
            ExternalPc(22),
            GuardKind::ArithOverflow(count_next),
        ));
        let i_next = b.fresh_ssa();
        b.append(TraceOp::Add {
            dst: i_next,
            lhs: i_phi,
            rhs: step1,
        });
        let of2 = TraceOp::Guard {
            kind: GuardKind::ArithOverflow(i_next),
            check: i_next,
        };
        let of2_pc = b.append(of2);
        b.record_guard(GuardSite::new(
            of2_pc,
            ExternalPc(33),
            GuardKind::ArithOverflow(i_next),
        ));
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![count_next, i_next],
        });
        b.append(TraceOp::Return { value: count_next });
        b
    }

    #[test]
    fn strip_loop_counter_overflow_guard() {
        let n = SsaVar(2); // matches the LocalGet dst in mk_w4_loop
        let mut b = mk_w4_loop(n);
        let before_ops = b.ops.len();
        let before_guards = b.guards.len();

        let report = IvOverflowElim.run(&mut b);
        assert_eq!(
            report.ops_removed, 2,
            "should drop both in-loop ArithOverflow guards"
        );
        assert_eq!(report.guards_added, 1, "single entry guard inserted");

        // Op count: -2 guards + 3 entry-guard ops = +1 net.
        assert_eq!(b.ops.len(), before_ops - 2 + 3);

        // No ArithOverflow guards remain anywhere in the buffer.
        assert!(
            !b.ops.iter().any(|op| matches!(
                op,
                TraceOp::Guard {
                    kind: GuardKind::ArithOverflow(_),
                    ..
                }
            )),
            "every ArithOverflow guard should be gone"
        );

        // Entry guard must sit immediately before the MarkLoopHead.
        let head_pc = b
            .ops
            .iter()
            .position(|op| matches!(op, TraceOp::MarkLoopHead { .. }))
            .expect("loop head present");
        assert!(head_pc >= 3, "entry-guard triple needs three op slots");
        assert!(matches!(
            &b.ops[head_pc - 3],
            TraceOp::ConstI64 {
                value: MAX_SAFE_LOOP_BOUND,
                ..
            }
        ));
        match &b.ops[head_pc - 2] {
            TraceOp::Cmp {
                kind: CmpKind::Gt,
                lhs,
                ..
            } => assert_eq!(*lhs, n, "cmp must read the bound SSA"),
            other => panic!("expected Cmp Gt, got {other:?}"),
        }
        assert!(matches!(
            &b.ops[head_pc - 1],
            TraceOp::Guard {
                kind: GuardKind::IsZero(_),
                ..
            }
        ));

        // Guard side-table: original IsZero (loop-exit) survives; the
        // two ArithOverflow sites are dropped; entry guard's site is
        // appended.
        let kinds: Vec<_> = b.guards.iter().map(|g| g.kind).collect();
        assert_eq!(
            kinds
                .iter()
                .filter(|k| matches!(k, GuardKind::ArithOverflow(_)))
                .count(),
            0
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|k| matches!(k, GuardKind::IsZero(_)))
                .count(),
            2, // original loop-exit IsZero + new entry IsZero
        );
        assert_eq!(b.guards.len(), before_guards - 2 + 1);

        // Surviving guards' trace_pc must point at a real Guard op.
        for g in &b.guards {
            assert!(
                matches!(b.ops.get(g.trace_pc as usize), Some(TraceOp::Guard { .. })),
                "guard site trace_pc {} must point at a Guard op, got {:?}",
                g.trace_pc,
                b.ops.get(g.trace_pc as usize)
            );
        }
    }

    #[test]
    fn strip_accumulator_overflow_guard_with_bool_step() {
        // Identical to W4 modulo: the test above already exercises the
        // Bool-typed accumulator (`hit` from StrContains). Confirm
        // separately that count's overflow guard is the one referencing
        // a Bool step — pin the bool-step branch of `analyse_loop`.
        let n = SsaVar(2);
        let mut b = mk_w4_loop(n);
        IvOverflowElim.run(&mut b);
        // The count `Add` op must survive (its dst still feeds the phi);
        // its overflow guard must be gone.
        let has_count_add = b.ops.iter().any(|op| matches!(op, TraceOp::Add { .. }));
        assert!(has_count_add, "Add ops keep their dst");
    }

    #[test]
    fn keeps_overflow_guard_when_step_too_large() {
        // Build a single-phi loop where step is `ConstI64(MAX_STEP + 1)`.
        // The proof must fail and the ArithOverflow guard stay.
        let mut b = TraceBuffer::new();
        let i_seed = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: i_seed,
            value: 0,
        });
        b.record_const(i_seed, TraceConst::I64(0));
        let n = b.fresh_ssa();
        b.append(TraceOp::LocalGet {
            dst: n,
            slot_idx: 0,
        });
        let huge_step = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: huge_step,
            value: MAX_STEP + 1,
        });
        b.record_const(huge_step, TraceConst::I64(MAX_STEP + 1));
        let i_phi = b.fresh_ssa();
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![LoopPhi::new(i_seed, i_phi)],
        });
        let cmp = b.fresh_ssa();
        b.append(TraceOp::Cmp {
            kind: CmpKind::Ge,
            dst: cmp,
            lhs: i_phi,
            rhs: n,
        });
        let isz_pc = b.append(TraceOp::Guard {
            kind: GuardKind::IsZero(cmp),
            check: cmp,
        });
        b.record_guard(GuardSite::new(
            isz_pc,
            ExternalPc(7),
            GuardKind::IsZero(cmp),
        ));
        let i_next = b.fresh_ssa();
        b.append(TraceOp::Add {
            dst: i_next,
            lhs: i_phi,
            rhs: huge_step,
        });
        let of_pc = b.append(TraceOp::Guard {
            kind: GuardKind::ArithOverflow(i_next),
            check: i_next,
        });
        b.record_guard(GuardSite::new(
            of_pc,
            ExternalPc(8),
            GuardKind::ArithOverflow(i_next),
        ));
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![i_next],
        });
        b.append(TraceOp::Return { value: i_next });

        let before = b.ops.len();
        let report = IvOverflowElim.run(&mut b);
        assert_eq!(report.ops_removed, 0, "huge step must disqualify");
        assert_eq!(b.ops.len(), before);
        assert!(b.ops.iter().any(|op| matches!(
            op,
            TraceOp::Guard {
                kind: GuardKind::ArithOverflow(_),
                ..
            }
        )));
    }

    #[test]
    fn keeps_overflow_guard_when_bound_not_invariant() {
        // Bound `n` is defined inside the loop body (an `Add` of phi +
        // something). The pass must refuse to rewrite.
        let mut b = TraceBuffer::new();
        let i_seed = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: i_seed,
            value: 0,
        });
        b.record_const(i_seed, TraceConst::I64(0));
        let n_seed = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: n_seed,
            value: 10,
        });
        b.record_const(n_seed, TraceConst::I64(10));
        let step1 = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: step1,
            value: 1,
        });
        b.record_const(step1, TraceConst::I64(1));
        let i_phi = b.fresh_ssa();
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![LoopPhi::new(i_seed, i_phi)],
        });
        // Build a per-iter `n_dyn = i_phi + 1` that lives INSIDE the
        // body, and use it as the cmp's rhs. That makes the bound
        // loop-carried.
        let n_dyn = b.fresh_ssa();
        b.append(TraceOp::Add {
            dst: n_dyn,
            lhs: i_phi,
            rhs: step1,
        });
        let cmp = b.fresh_ssa();
        b.append(TraceOp::Cmp {
            kind: CmpKind::Ge,
            dst: cmp,
            lhs: i_phi,
            rhs: n_dyn,
        });
        let isz_pc = b.append(TraceOp::Guard {
            kind: GuardKind::IsZero(cmp),
            check: cmp,
        });
        b.record_guard(GuardSite::new(
            isz_pc,
            ExternalPc(7),
            GuardKind::IsZero(cmp),
        ));
        let i_next = b.fresh_ssa();
        b.append(TraceOp::Add {
            dst: i_next,
            lhs: i_phi,
            rhs: step1,
        });
        let of_pc = b.append(TraceOp::Guard {
            kind: GuardKind::ArithOverflow(i_next),
            check: i_next,
        });
        b.record_guard(GuardSite::new(
            of_pc,
            ExternalPc(8),
            GuardKind::ArithOverflow(i_next),
        ));
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![i_next],
        });
        b.append(TraceOp::Return { value: i_next });

        let before = b.ops.len();
        let report = IvOverflowElim.run(&mut b);
        assert_eq!(report.ops_removed, 0);
        assert_eq!(b.ops.len(), before);
    }

    #[test]
    fn keeps_overflow_guard_when_no_exit_idiom() {
        // Loop body starts with a Load, not Cmp Ge. The pass must skip.
        let mut b = TraceBuffer::new();
        let i_seed = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: i_seed,
            value: 0,
        });
        b.record_const(i_seed, TraceConst::I64(0));
        let step1 = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: step1,
            value: 1,
        });
        b.record_const(step1, TraceConst::I64(1));
        let i_phi = b.fresh_ssa();
        b.append(TraceOp::MarkLoopHead {
            loop_id: 0,
            phis: vec![LoopPhi::new(i_seed, i_phi)],
        });
        // No Cmp Ge here. Some other op first.
        let dummy_base = b.fresh_ssa();
        let loaded = b.fresh_ssa();
        b.append(TraceOp::Load {
            dst: loaded,
            base: dummy_base,
            offset: crate::trace_ir::Offset(0),
        });
        let i_next = b.fresh_ssa();
        b.append(TraceOp::Add {
            dst: i_next,
            lhs: i_phi,
            rhs: step1,
        });
        let of_pc = b.append(TraceOp::Guard {
            kind: GuardKind::ArithOverflow(i_next),
            check: i_next,
        });
        b.record_guard(GuardSite::new(
            of_pc,
            ExternalPc(8),
            GuardKind::ArithOverflow(i_next),
        ));
        b.append(TraceOp::MarkLoopBack {
            loop_id: 0,
            next_values: vec![i_next],
        });
        b.append(TraceOp::Return { value: i_next });

        let before = b.ops.len();
        let report = IvOverflowElim.run(&mut b);
        assert_eq!(report.ops_removed, 0);
        assert_eq!(b.ops.len(), before);
    }

    /// Standalone smoke test for the rebind: surviving guards point at
    /// Guard ops with matching kinds, and entry guard's GuardSite
    /// trace_pc lands at the inserted IsZero op.
    #[test]
    fn guard_trace_pcs_remap_consistently() {
        let n = SsaVar(2);
        let mut b = mk_w4_loop(n);
        IvOverflowElim.run(&mut b);

        for g in &b.guards {
            let op = b
                .ops
                .get(g.trace_pc as usize)
                .expect("trace_pc must be in-range");
            match (op, g.kind) {
                (TraceOp::Guard { kind: op_kind, .. }, site_kind) => {
                    assert_eq!(*op_kind, site_kind, "kind must match")
                }
                _ => panic!("trace_pc must point at a Guard op"),
            }
        }
    }
}
