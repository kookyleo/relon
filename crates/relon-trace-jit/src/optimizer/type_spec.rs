//! Type specialisation pass.
//!
//! Uses the recorder's observed-type sidetable
//! ([`crate::TraceBuffer::type_info`]) to insert
//! `Guard(TypeCheck(...))` ops in front of generic-typed `Call`
//! sites. A real cranelift emitter will later specialise the
//! arithmetic path; for the scaffolding the focus is on the *guard
//! insertion* so the deopt machinery has the metadata it needs.
//!
//! Algorithm:
//!
//! - For each `Call` op, look up the *first* argument's observed
//!   type. If known, insert a guard before the call that checks
//!   `arg0 : observed_type`. The corresponding [`GuardSite`] is
//!   appended to `trace.guards`, anchored at the freshly inserted
//!   guard op.
//! - Calls whose `EffectClass` is `Unrecoverable` are left alone:
//!   the trace recorder should have aborted before they got here, so
//!   inserting a guard would only mask a recorder bug.
//!
//! The pass is intentionally narrow: it does **not** rewrite the
//! call to a specialised callee (that requires the v6-gamma host FFI
//! to expose a `specialised_for(callee, type)` lookup). What it
//! does provide is the guard-site bookkeeping that downstream
//! cranelift emission relies on.
//!
//! ## Ordering
//!
//! Runs after [`super::dead_store::DeadStoreElim`] (round 1) and
//! before [`super::dict_ic_hoist::DictIcHoist`]. The order relative
//! to dict-ic-hoist / LICM is incidental — the guards this pass
//! inserts don't affect `dict_ptr` invariance — but it MUST precede
//! [`super::noop_typecheck_elim::NoopTypeCheckElim`], which is the
//! consumer that folds away `TypeCheck` guards once LICM has had a
//! chance to hoist them. See the [`super`] module docs for the full
//! pipeline contract.

use crate::buffer::TraceBuffer;
use crate::effect::EffectClass;
use crate::guard::GuardSite;
use crate::trace_ir::{ExternalPc, GuardKind, ObservedType, SsaVar, TraceOp};

use super::{OptimizerPass, PassReport};

pub struct TypeSpec;

impl OptimizerPass for TypeSpec {
    fn name(&self) -> &'static str {
        "type_spec"
    }

    fn run(&self, trace: &mut TraceBuffer) -> PassReport {
        let mut report = PassReport::default();
        // Snapshot the observed types so we can mutate the op vec
        // without fighting the borrow checker.
        let type_info = trace.type_info.clone();
        // Walk in reverse so insertions don't shift indices we haven't
        // processed yet.
        let mut idx = trace.ops.len();
        while idx > 0 {
            idx -= 1;
            let needs_guard = match &trace.ops[idx] {
                TraceOp::Call { args, effect, .. }
                    if !matches!(effect, EffectClass::Unrecoverable) =>
                {
                    args.first()
                        .copied()
                        .and_then(|arg| type_info.get(&arg).copied().map(|t| (arg, t)))
                }
                _ => None,
            };
            if let Some((arg, ty)) = needs_guard {
                insert_type_guard(trace, idx, arg, ty);
                report.guards_added += 1;
            }
        }

        // Fix up guard pcs for previously existing guards: every
        // insertion shifts the trailing pcs by +1. The simplest
        // correct approach is a single pass that recomputes guard pcs
        // from the op vector: a guard's pc is the position of its
        // corresponding `TraceOp::Guard`. We match by `kind` + arg
        // since `trace_pc` was the only previous link.
        trace.rebind_guard_pcs();
        report
    }
}

fn insert_type_guard(trace: &mut TraceBuffer, call_idx: usize, var: SsaVar, ty: ObservedType) {
    let kind = GuardKind::TypeCheck(var, ty);
    let guard_op = TraceOp::Guard { kind, check: var };
    trace.ops.insert(call_idx, guard_op);
    // Record the GuardSite. The placeholder `deopt_pc` is a sentinel
    // that the v6-gamma lowering must replace with a real PC into the
    // generic code.
    let site = GuardSite::new(call_idx as u32, ExternalPc(0), kind);
    trace.guards.push(site);
}
