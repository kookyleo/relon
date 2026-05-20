//! Load-forwarding pass.
//!
//! Scans the trace and rewrites pattern
//!
//! ```text
//! Store(base, off, v1)
//! ... ops that neither read nor write (base, off) ...
//! Load(dst, base, off)
//! ```
//!
//! by aliasing every later read of `dst` to `v1`. The redundant
//! `Load` op is left in the buffer (its `dst` is now dead) and is
//! cleaned up by a subsequent run of [`super::dead_store::DeadStoreElim`].
//! That separation keeps each pass single-purpose; the pipeline in
//! [`super::OptimizerPipeline::default_pipeline`] explicitly re-runs
//! `DeadStoreElim` after load forwarding for that reason.
//!
//! Alias table invariants:
//!
//! - Keyed by `(base_ssa, offset)` -- same conservative aliasing
//!   model as the dead-store pass. Different base SSAs are treated
//!   as non-aliasing.
//! - A `Store(base, off, v)` op updates the entry for `(base, off)`
//!   to `v`. Any prior entry for that exact slot is overwritten.
//! - A `Load(dst, base, off)` hits the table when an entry for
//!   `(base, off)` exists. The SSA id `dst` is then registered in
//!   the alias map `dst -> entry_value`, and every later occurrence
//!   of `dst` as an input is rewritten.
//! - Any op with [`EffectClass::RecoverableWrite`] (other than the
//!   `Store` we just modelled) conservatively flushes the entire
//!   slot table. We cannot reason about which slots it may have
//!   touched. The alias *map* (`dst -> value`) is unaffected:
//!   already-issued aliases remain valid because they refer to
//!   SSA ids whose value-binding cannot change.
//! - A guard op does not invalidate aliases -- it may deopt out,
//!   but if the trace continues the SSA bindings still hold.
//! - `EffectClass::Unrecoverable` is a hard panic: the recorder
//!   must never have emitted such an op into a trace.
//!
//! Limitations (intentional, mirrors `dead_store`):
//!
//! - No alias reasoning between distinct base SSAs.
//! - `MarkLoopHead`/`MarkLoopBack` are pure markers and pass
//!   through unchanged.
//! - We rewrite inputs in place on every op we visit, but we never
//!   remove a `Load` -- removal is delegated to dead-store elim so
//!   guard `trace_pc` fixups stay in one place.

use std::collections::HashMap;

use crate::buffer::TraceBuffer;
use crate::effect::EffectClass;
use crate::trace_ir::{GuardKind, Offset, SsaVar, TraceOp};

use super::{OptimizerPass, PassReport};

/// Load-forwarding pass. Stateless.
pub struct LoadForwarding;

impl OptimizerPass for LoadForwarding {
    fn name(&self) -> &'static str {
        "load_forwarding"
    }

    fn run(&self, trace: &mut TraceBuffer) -> PassReport {
        let mut report = PassReport::default();
        // Most recent value stored at (base, offset).
        let mut slot_value: HashMap<(SsaVar, i32), SsaVar> = HashMap::new();
        // SSA alias chain: lookup is iterated to a fixed point so
        // `A -> B`, `B -> C` collapses to `A -> C` lazily.
        let mut alias: HashMap<SsaVar, SsaVar> = HashMap::new();

        for idx in 0..trace.ops.len() {
            // First, rewrite the op's inputs through the alias table
            // (in place). Each rewrite that actually changes an id
            // counts as a replace.
            let replaced = rewrite_inputs(&mut trace.ops[idx], &alias);
            report.ops_replaced += replaced;

            match &trace.ops[idx] {
                TraceOp::Store(base, Offset(off), src) => {
                    slot_value.insert((*base, *off), *src);
                }
                TraceOp::Load(dst, base, Offset(off)) => {
                    if let Some(forwarded) = slot_value.get(&(*base, *off)).copied() {
                        // Resolve forwarded through the alias chain
                        // in case the stored value was itself a
                        // previously-forwarded SSA id.
                        let root = resolve(&alias, forwarded);
                        alias.insert(*dst, root);
                        // Note: we do NOT remove the Load op here.
                        // dead_store elim will drop it on the next
                        // pipeline iteration.
                    }
                }
                TraceOp::Call(_, _, _, eff) => match eff {
                    EffectClass::Pure | EffectClass::ReadOnly => {
                        // Pure/ReadOnly calls cannot mutate memory
                        // visible to our slot table.
                    }
                    EffectClass::RecoverableWrite => {
                        // Conservative flush: we don't know which
                        // slots the call may have touched.
                        slot_value.clear();
                    }
                    EffectClass::Unrecoverable => {
                        panic!("trace must not contain Unrecoverable ops");
                    }
                },
                TraceOp::Div(_, _, _) | TraceOp::Mod(_, _, _) => {
                    // Div / Mod are classed RecoverableWrite (see
                    // trace_ir docs). They do not actually touch
                    // memory but the conservative rule applies:
                    // flush.
                    slot_value.clear();
                }
                _ => {
                    // Guard / Const* / arithmetic / Cmp / Return /
                    // loop markers / forwarded Load -- none affect
                    // the slot table.
                }
            }
        }

        report
    }
}

/// Resolve `v` through the alias map to its canonical SSA id.
/// Iterates -- no path compression needed because the chains are
/// short and we walk the trace exactly once.
fn resolve(alias: &HashMap<SsaVar, SsaVar>, mut v: SsaVar) -> SsaVar {
    let mut hops = 0;
    while let Some(next) = alias.get(&v).copied() {
        if next == v {
            break;
        }
        v = next;
        hops += 1;
        debug_assert!(hops < 1024, "alias chain looks cyclic");
        if hops >= 1024 {
            break;
        }
    }
    v
}

/// Rewrite every SSA input of `op` through the alias map. Returns
/// the number of slots that actually changed -- used purely for
/// `PassReport` accounting.
fn rewrite_inputs(op: &mut TraceOp, alias: &HashMap<SsaVar, SsaVar>) -> usize {
    let mut changed = 0;
    macro_rules! swap {
        ($v:expr) => {{
            let new = resolve(alias, *$v);
            if new != *$v {
                *$v = new;
                changed += 1;
            }
        }};
    }
    match op {
        TraceOp::Add(_dst, a, b)
        | TraceOp::Sub(_dst, a, b)
        | TraceOp::Mul(_dst, a, b)
        | TraceOp::Div(_dst, a, b)
        | TraceOp::Mod(_dst, a, b) => {
            swap!(a);
            swap!(b);
        }
        TraceOp::Cmp(_, _dst, a, b) => {
            swap!(a);
            swap!(b);
        }
        TraceOp::Load(_dst, base, _off) => {
            swap!(base);
        }
        TraceOp::Store(base, _off, src) => {
            swap!(base);
            swap!(src);
        }
        TraceOp::ConstI32(_, _) | TraceOp::ConstI64(_, _) | TraceOp::LocalGet(_, _) => {}
        TraceOp::Guard(kind, check) => {
            swap!(check);
            match kind {
                GuardKind::TypeCheck(v, _) => swap!(v),
                GuardKind::NotNull(v) => swap!(v),
                GuardKind::BoundsCheck(v, limit) => {
                    swap!(v);
                    swap!(limit);
                }
                GuardKind::ArithOverflow(v) => swap!(v),
                GuardKind::IsZero(v) => swap!(v),
            }
        }
        TraceOp::Call(_dst, _, args, _) => {
            for a in args {
                swap!(a);
            }
        }
        // F-D7 string ops carry SSA inputs that may shadow a load
        // the forwarding pass replaced earlier. Swap each input slot
        // so a `StrContains(haystack=load_dst, needle=...)` re-uses
        // the forwarded source if the underlying load was DCE-d.
        TraceOp::StrConcat(_dst, a, b)
        | TraceOp::StrContains(_dst, a, b)
        | TraceOp::StrFind(_dst, a, b) => {
            swap!(a);
            swap!(b);
        }
        TraceOp::StrSubstring(_dst, s, start, len) => {
            swap!(s);
            swap!(start);
            swap!(len);
        }
        TraceOp::ListGet { list_ptr, idx, .. } => {
            swap!(list_ptr);
            swap!(idx);
        }
        TraceOp::DictLookup {
            dict_ptr, key_ptr, ..
        } => {
            swap!(dict_ptr);
            swap!(key_ptr);
        }
        // F-D8-E.2: the optimizer's `dict_ic_hoist` pass replaces a
        // single `DictLookup` with this pair; both reference SSA
        // inputs that an earlier load-forward round may have already
        // alias-resolved, so participate in the swap loop just like
        // their full-form sibling above.
        TraceOp::DictShapeGuard { dict_ptr, .. } => {
            swap!(dict_ptr);
        }
        TraceOp::DictLookupPrechecked {
            dict_ptr, key_ptr, ..
        } => {
            swap!(dict_ptr);
            swap!(key_ptr);
        }
        TraceOp::Return(v) => {
            swap!(v);
        }
        TraceOp::MarkLoopHead { .. } | TraceOp::MarkLoopBack { .. } => {}
    }
    changed
}
