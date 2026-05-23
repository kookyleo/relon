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
//!
//! ## Ordering
//!
//! Runs **after** [`super::const_fold::ConstFold`] so constant
//! offsets are already collapsed into the `(base, offset)` slot keys
//! the alias table uses, and **before**
//! [`super::dead_store::DeadStoreElim`] (round 1), which cleans up
//! the `Load` ops this pass leaves dead. See the [`super`] module
//! docs for the full pipeline contract.

use rustc_hash::FxHashMap;

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
        let mut slot_value: FxHashMap<(SsaVar, i32), SsaVar> = FxHashMap::default();
        // SSA alias chain: lookup is iterated to a fixed point so
        // `A -> B`, `B -> C` collapses to `A -> C` lazily.
        let mut alias: FxHashMap<SsaVar, SsaVar> = FxHashMap::default();

        for idx in 0..trace.ops.len() {
            // First, rewrite the op's inputs through the alias table
            // (in place). Each rewrite that actually changes an id
            // counts as a replace.
            let replaced = rewrite_inputs(&mut trace.ops[idx], &alias);
            report.ops_replaced += replaced;

            match &trace.ops[idx] {
                TraceOp::Store {
                    base,
                    offset: Offset(off),
                    src,
                } => {
                    slot_value.insert((*base, *off), *src);
                }
                TraceOp::Load {
                    dst,
                    base,
                    offset: Offset(off),
                } => {
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
                TraceOp::Call { effect, .. } => match effect {
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
                TraceOp::Div { .. } | TraceOp::Mod { .. } => {
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
fn resolve(alias: &FxHashMap<SsaVar, SsaVar>, mut v: SsaVar) -> SsaVar {
    // Cap the walk at 1024 hops as a defensive break against cycles —
    // none should be reachable but a soft cap beats an infinite loop
    // on a future bug; debug builds also assert the chain stays short.
    for hops in 0..1024 {
        let Some(next) = alias.get(&v).copied() else {
            break;
        };
        if next == v {
            break;
        }
        v = next;
        debug_assert!(hops + 1 < 1024, "alias chain looks cyclic");
    }
    v
}

/// Rewrite every SSA input of `op` through the alias map. Returns
/// the number of slots that actually changed -- used purely for
/// `PassReport` accounting.
fn rewrite_inputs(op: &mut TraceOp, alias: &FxHashMap<SsaVar, SsaVar>) -> usize {
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
        TraceOp::Add { lhs, rhs, .. }
        | TraceOp::Sub { lhs, rhs, .. }
        | TraceOp::Mul { lhs, rhs, .. }
        | TraceOp::Div { lhs, rhs, .. }
        | TraceOp::Mod { lhs, rhs, .. } => {
            swap!(lhs);
            swap!(rhs);
        }
        TraceOp::Cmp { lhs, rhs, .. } => {
            swap!(lhs);
            swap!(rhs);
        }
        TraceOp::Load { base, .. } => {
            swap!(base);
        }
        TraceOp::Store { base, src, .. } => {
            swap!(base);
            swap!(src);
        }
        TraceOp::ConstI32 { .. } | TraceOp::ConstI64 { .. } | TraceOp::LocalGet { .. } => {}
        TraceOp::Guard { kind, check } => {
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
        TraceOp::Call { args, .. } => {
            for a in args {
                swap!(a);
            }
        }
        // F-D7 string ops carry SSA inputs that may shadow a load
        // the forwarding pass replaced earlier. Swap each input slot
        // so a `StrContains(haystack=load_dst, needle=...)` re-uses
        // the forwarded source if the underlying load was DCE-d.
        TraceOp::StrConcat { lhs, rhs, .. } => {
            swap!(lhs);
            swap!(rhs);
        }
        TraceOp::StrContains {
            haystack, needle, ..
        }
        | TraceOp::StrFind {
            haystack, needle, ..
        } => {
            swap!(haystack);
            swap!(needle);
        }
        TraceOp::StrGlobMatch { s, pattern, .. } => {
            swap!(s);
            swap!(pattern);
        }
        // #168: variable-arity sibling of `StrConcat`. Every operand
        // SSA participates in the load-forward swap loop just like the
        // two-operand variant — the inline emitter lowers each operand
        // to a separate `(ptr, len)` load, so an alias resolution on
        // operand[k] still flows through to the per-operand memcpy.
        TraceOp::StrConcatN { operands, .. } => {
            for o in operands {
                swap!(o);
            }
        }
        TraceOp::StrSubstring {
            s, start, length, ..
        } => {
            swap!(s);
            swap!(start);
            swap!(length);
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
        TraceOp::Return { value } => {
            swap!(value);
        }
        TraceOp::MarkLoopHead { .. } | TraceOp::MarkLoopBack { .. } => {}
    }
    changed
}
