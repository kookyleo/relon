//! Stateful trace recorder.
//!
//! Holds the [`relon_trace_jit::TraceBuffer`] under construction plus
//! the sidetables the recorder needs across `record_op` calls:
//!
//! * an SSA allocator producing dense, monotone [`SsaVar`] ids,
//! * an `ir_to_ssa` map binding `LocalGet` / `LetGet` keys to the
//!   most-recently-stored SSA value,
//! * an observed-type map driving TypeCheck guard emission per the
//!   policy in `type_obs.rs`,
//! * an `aborted` slot the state machine flips on the first failed
//!   record_op so subsequent calls short-circuit without touching the
//!   buffer.
//!
//! The recorder is intentionally synchronous — it expects to be
//! driven by whatever loop the host uses to execute the Relon IR's
//! op stream (today the cranelift-generic backend, tomorrow a tiny
//! interpreter). Each call corresponds to one observed op execution.

use std::collections::HashMap;

use relon_ir::Op;
use relon_trace_jit::{
    EffectClass as TraceEffect, GuardKind, ObservedType, SsaVar, TraceBuffer, TraceOp,
};

use crate::abort::AbortReason;
use crate::lowering::{lower_op, LookupKind, LowerOutcome, OpLoweringContext};
use crate::type_obs::{classify_observation, TypeObsDecision};

/// Maximum number of ops a single trace may accumulate before the
/// recorder gives up. Hard-coded so the buffer's growth stays
/// predictable; the caller bumps this via [`RecorderState::with_capacity`]
/// when it has more headroom.
pub const DEFAULT_MAX_OPS: usize = 1024;

/// Result of a single [`RecorderState::record_op`] call. Carried as a
/// dedicated enum so callers can pattern-match without inspecting the
/// recorder's internal flags.
#[derive(Debug, Clone)]
pub enum RecordResult {
    /// The op was recorded; if it produced an SSA value, it's
    /// available as `value`.
    Ok { value: Option<SsaVar> },
    /// The op was recorded *and* the recorder appended a guard whose
    /// kind the caller should mirror in its own deopt-state
    /// bookkeeping. Returned in addition to `Ok` when a TypeCheck or
    /// arithmetic overflow guard fires.
    NeedsGuard {
        value: Option<SsaVar>,
        guard: GuardKind,
    },
    /// The op terminated the trace (e.g. `Op::Return`). The recorder
    /// is now in a finalisable state; calling `finalize()` returns
    /// the buffer.
    Terminated,
    /// The op caused the trace to abort. The recorder will return
    /// this on every subsequent call until reset.
    Abort(AbortReason),
}

/// Monotone SSA id allocator. Kept as its own type so unit tests can
/// drive it directly without spinning up a recorder; the
/// [`RecorderState`] embeds one and bumps it via [`SsaAllocator::alloc`].
#[derive(Debug, Default)]
pub struct SsaAllocator {
    next: u32,
}

impl SsaAllocator {
    /// Allocate the next SSA id. Panics if the id space is exhausted —
    /// the recorder's `TraceTooLong` budget hits long before u32::MAX.
    pub fn alloc(&mut self) -> SsaVar {
        let v = SsaVar(self.next);
        self.next = self
            .next
            .checked_add(1)
            .expect("trace SSA id space exhausted (>u32::MAX vars)");
        v
    }

    /// High-water mark — number of distinct SSA ids allocated so far.
    pub fn count(&self) -> u32 {
        self.next
    }

    /// Reset for a fresh trace. Used by long-running hosts that
    /// recycle the allocator across recordings.
    pub fn reset(&mut self) {
        self.next = 0;
    }
}

/// Recorder state machine. Holds the buffer being filled plus all the
/// sidetables the recording needs across op boundaries.
#[derive(Debug)]
pub struct RecorderState {
    buffer: TraceBuffer,
    ssa: SsaAllocator,
    ir_to_ssa: HashMap<LookupKind, SsaVar>,
    type_obs: HashMap<SsaVar, ObservedType>,
    /// SSA values that have already had a TypeCheck guard emitted —
    /// used by the de-dupe logic in `maybe_emit_type_guard` so the
    /// optimiser pipeline can rely on at most one TypeCheck per var
    /// before its own LICM pass runs.
    guarded_vars: HashMap<SsaVar, ObservedType>,
    /// Hot-loop nesting depth so the recorder can stamp matching
    /// `MarkLoopHead` / `MarkLoopBack` ids when it sees an
    /// `Op::Loop` / its closing boundary.
    loop_depth: u32,
    next_loop_id: u32,
    /// Maximum number of ops the buffer may collect before the
    /// recorder aborts with `TraceTooLong`.
    capacity: usize,
    aborted: Option<AbortReason>,
    terminated: bool,
}

impl RecorderState {
    /// Build a recorder with the default op budget.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_OPS)
    }

    /// Build a recorder that aborts after `capacity` ops.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: TraceBuffer::new(),
            ssa: SsaAllocator::default(),
            ir_to_ssa: HashMap::new(),
            type_obs: HashMap::new(),
            guarded_vars: HashMap::new(),
            loop_depth: 0,
            next_loop_id: 0,
            capacity,
            aborted: None,
            terminated: false,
        }
    }

    /// True when the recorder has accepted an abort decision and is
    /// no longer touching the buffer.
    pub fn is_aborted(&self) -> bool {
        self.aborted.is_some()
    }

    /// True when the recorder has observed a terminator
    /// (`Op::Return`) and the trace can be finalised.
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }

    /// Current op count. Mirrors `TraceBuffer::op_count` for
    /// convenience.
    pub fn op_count(&self) -> usize {
        self.buffer.op_count()
    }

    /// Read-only access to the buffer — useful in unit tests that
    /// want to introspect emitted ops without consuming the recorder.
    pub fn buffer(&self) -> &TraceBuffer {
        &self.buffer
    }

    /// Force an abort. Idempotent; the first reason wins so a
    /// downstream `UnsupportedOp` cannot mask an earlier
    /// `UnrecoverableEffect`.
    pub fn abort(&mut self, reason: AbortReason) {
        if self.aborted.is_none() {
            self.aborted = Some(reason);
        }
    }

    /// Drop the recorder, returning the underlying [`TraceBuffer`] iff
    /// no abort was recorded. The returned buffer may then be fed
    /// through the optimiser pipeline.
    pub fn finalize(self) -> Result<TraceBuffer, AbortReason> {
        if let Some(reason) = self.aborted {
            Err(reason)
        } else {
            Ok(self.buffer)
        }
    }

    /// Record one Relon IR op.
    ///
    /// `inputs` are the SSA ids of the values currently sitting on
    /// the host's operand stack in push order (last pushed first), as
    /// observed by the cranelift-generic backend's runtime. `observed`
    /// is the runtime [`ObservedType`] of the value the op produces;
    /// the recorder uses it to drive its TypeCheck guard policy.
    ///
    /// Returns the SSA id of the emitted value (if any) wrapped in
    /// the appropriate [`RecordResult`] variant.
    pub fn record_op(
        &mut self,
        op: &Op,
        inputs: &[SsaVar],
        observed: Option<ObservedType>,
    ) -> RecordResult {
        // Short-circuit terminated / aborted state without touching
        // the buffer. The recorder is a sticky state machine.
        if let Some(reason) = self.aborted {
            return RecordResult::Abort(reason);
        }
        if self.terminated {
            return RecordResult::Abort(AbortReason::UnsupportedOp("PostTerminator"));
        }
        if self.buffer.op_count() >= self.capacity {
            self.aborted = Some(AbortReason::TraceTooLong);
            return RecordResult::Abort(AbortReason::TraceTooLong);
        }

        let fresh_dst = self.ssa.alloc();
        let cx = self.build_lowering_cx(op, inputs, fresh_dst);
        let outcome = lower_op(op, cx);
        self.apply_outcome(op, outcome, inputs, fresh_dst, observed)
    }

    /// Override the effect class the lowering rule applies to the
    /// next `Op::Call`. Used by hosts that have classified the
    /// callee out-of-band (e.g. via a per-stdlib effect table).
    pub fn record_op_with_call_effect(
        &mut self,
        op: &Op,
        inputs: &[SsaVar],
        observed: Option<ObservedType>,
        call_effect: TraceEffect,
    ) -> RecordResult {
        if let Some(reason) = self.aborted {
            return RecordResult::Abort(reason);
        }
        if self.terminated {
            return RecordResult::Abort(AbortReason::UnsupportedOp("PostTerminator"));
        }
        if self.buffer.op_count() >= self.capacity {
            self.aborted = Some(AbortReason::TraceTooLong);
            return RecordResult::Abort(AbortReason::TraceTooLong);
        }

        let fresh_dst = self.ssa.alloc();
        let cx = self
            .build_lowering_cx(op, inputs, fresh_dst)
            .with_call_effect_override(call_effect);
        let outcome = lower_op(op, cx);
        self.apply_outcome(op, outcome, inputs, fresh_dst, observed)
    }

    fn build_lowering_cx<'a>(
        &self,
        _op: &Op,
        inputs: &'a [SsaVar],
        fresh_dst: SsaVar,
    ) -> OpLoweringContext<'a> {
        OpLoweringContext::new(inputs, fresh_dst)
    }

    fn apply_outcome(
        &mut self,
        op: &Op,
        outcome: LowerOutcome,
        inputs: &[SsaVar],
        fresh_dst: SsaVar,
        observed: Option<ObservedType>,
    ) -> RecordResult {
        match outcome {
            LowerOutcome::Emit {
                op: trace_op,
                dst,
                guards_before,
                guards_after,
                effect: _,
            } => {
                for g in guards_before {
                    self.emit_guard(g);
                }
                self.buffer.append(trace_op);
                let mut surfaced_guard = None;
                for g in guards_after {
                    let kind = self.emit_guard(g);
                    surfaced_guard = surfaced_guard.or(kind);
                }
                if let Some(d) = dst {
                    if let Some(ty) = observed {
                        if let Some(g) = self.maybe_emit_type_guard(d, ty) {
                            surfaced_guard = surfaced_guard.or(Some(g));
                        }
                    }
                }
                if let Some(g) = surfaced_guard {
                    RecordResult::NeedsGuard {
                        value: dst,
                        guard: g,
                    }
                } else {
                    RecordResult::Ok { value: dst }
                }
            }
            LowerOutcome::SideEffectOnly { rebind } => {
                // For LocalSet / LetSet we re-bind the slot to the
                // SSA the caller indicated.
                if let Some(ssa) = rebind {
                    if let Some(key) = local_or_let_key(op) {
                        self.ir_to_ssa.insert(key, ssa);
                    }
                }
                RecordResult::Ok { value: None }
            }
            LowerOutcome::Lookup { kind, ty_hint } => {
                let var = if let Some(existing) = self.ir_to_ssa.get(&kind).copied() {
                    existing
                } else {
                    // First time this slot is read — seed the map
                    // with `fresh_dst` so subsequent reads alias the
                    // same SSA id.
                    self.ir_to_ssa.insert(kind, fresh_dst);
                    self.type_obs.insert(fresh_dst, ty_hint);
                    fresh_dst
                };
                if let Some(ty) = observed.or(Some(ty_hint)) {
                    if let Some(g) = self.maybe_emit_type_guard(var, ty) {
                        return RecordResult::NeedsGuard {
                            value: Some(var),
                            guard: g,
                        };
                    }
                }
                RecordResult::Ok { value: Some(var) }
            }
            LowerOutcome::Terminate { op: trace_op } => {
                self.buffer.append(trace_op);
                self.terminated = true;
                RecordResult::Terminated
            }
            LowerOutcome::LoopMarker { op: marker_op } => {
                let marker = match marker_op {
                    TraceOp::MarkLoopHead { .. } => {
                        let id = self.next_loop_id;
                        self.next_loop_id += 1;
                        self.loop_depth = self.loop_depth.saturating_add(1);
                        TraceOp::MarkLoopHead { loop_id: id }
                    }
                    other => other,
                };
                self.buffer.append(marker);
                let _ = inputs;
                RecordResult::Ok { value: None }
            }
            LowerOutcome::Abort(reason) => {
                self.aborted = Some(reason);
                RecordResult::Abort(reason)
            }
        }
    }

    /// Apply the TypeCheck-guard policy from `type_obs`. Returns the
    /// emitted `GuardKind` so the caller can surface it via
    /// [`RecordResult::NeedsGuard`]; returns `None` when no guard was
    /// emitted (first-seen observation).
    fn maybe_emit_type_guard(&mut self, var: SsaVar, ty: ObservedType) -> Option<GuardKind> {
        let prev = self.type_obs.insert(var, ty);
        match classify_observation(prev, ty) {
            TypeObsDecision::FirstSeen => None,
            TypeObsDecision::EmitGuard => {
                // De-dupe: only emit one TypeCheck per (var, ty).
                if self.guarded_vars.get(&var) == Some(&ty) {
                    return None;
                }
                self.guarded_vars.insert(var, ty);
                let kind = GuardKind::TypeCheck(var, ty);
                self.buffer.append(TraceOp::Guard(kind, var));
                Some(kind)
            }
            TypeObsDecision::Mismatch { .. } => {
                self.aborted = Some(AbortReason::GuardFailureInRecording);
                None
            }
        }
    }

    fn emit_guard(&mut self, kind: GuardKind) -> Option<GuardKind> {
        // BoundsCheck whose base equals SsaVar::NONE is a recorder
        // sentinel — we never emit guards over invalid SSA ids.
        if let GuardKind::BoundsCheck(v, _) = kind {
            if v == SsaVar::NONE {
                return None;
            }
        }
        let payload = match kind {
            GuardKind::TypeCheck(v, _)
            | GuardKind::NotNull(v)
            | GuardKind::BoundsCheck(v, _)
            | GuardKind::ArithOverflow(v) => v,
        };
        self.buffer.append(TraceOp::Guard(kind, payload));
        Some(kind)
    }
}

impl Default for RecorderState {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the `ir_to_ssa` key for ops that drive the slot map.
/// Returns `None` for ops that do not touch the local / let table.
fn local_or_let_key(op: &Op) -> Option<LookupKind> {
    match op {
        Op::LetSet { idx, .. } => Some(LookupKind::Let(*idx)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_ir::IrType;

    #[test]
    fn ssa_allocator_monotonic() {
        let mut a = SsaAllocator::default();
        let v0 = a.alloc();
        let v1 = a.alloc();
        assert!(v1.raw() > v0.raw());
        assert_eq!(a.count(), 2);
        a.reset();
        assert_eq!(a.count(), 0);
    }

    #[test]
    fn new_recorder_is_empty() {
        let r = RecorderState::new();
        assert!(!r.is_aborted());
        assert!(!r.is_terminated());
        assert_eq!(r.op_count(), 0);
    }

    #[test]
    fn record_const_then_return() {
        let mut r = RecorderState::new();
        let res = r.record_op(&Op::ConstI64(7), &[], Some(ObservedType::I64));
        let val = match res {
            RecordResult::Ok { value: Some(v) } => v,
            other => panic!("unexpected {:?}", other),
        };
        let term = r.record_op(&Op::Return, &[val], None);
        assert!(matches!(term, RecordResult::Terminated));
        let buf = r.finalize().expect("no abort");
        assert_eq!(buf.op_count(), 2);
    }

    #[test]
    fn unsupported_op_aborts() {
        let mut r = RecorderState::new();
        let res = r.record_op(
            &Op::CallNative {
                import_idx: 0,
                param_tys: vec![],
                ret_ty: IrType::I64,
                cap_bit: 0,
            },
            &[],
            None,
        );
        assert!(matches!(
            res,
            RecordResult::Abort(AbortReason::UnrecoverableEffect)
        ));
        assert!(r.is_aborted());
        // Subsequent op short-circuits without touching the buffer.
        let res2 = r.record_op(&Op::ConstI64(1), &[], None);
        assert!(matches!(res2, RecordResult::Abort(_)));
        assert_eq!(r.op_count(), 0);
    }

    #[test]
    fn finalize_after_abort_returns_err() {
        let mut r = RecorderState::new();
        r.abort(AbortReason::TraceTooLong);
        assert_eq!(r.finalize().err(), Some(AbortReason::TraceTooLong));
    }

    #[test]
    fn capacity_overflow_aborts() {
        let mut r = RecorderState::with_capacity(1);
        let _ = r.record_op(&Op::ConstI64(1), &[], None);
        let res = r.record_op(&Op::ConstI64(2), &[], None);
        assert!(matches!(
            res,
            RecordResult::Abort(AbortReason::TraceTooLong)
        ));
    }
}
