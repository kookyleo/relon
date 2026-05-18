//! v6-γ M4: IR-walking evaluator that records as it executes.
//!
//! The host's HotCounter prologue fires
//! [`crate::trace_install::__relon_jump_to_recorder`] when an entry-fn
//! counter saturates. M2/M3 left that helper as a stub; M4 turns it
//! into a real recording driver:
//!
//! 1. Find the `relon_ir::Func` for `fn_id` in the registered IR
//!    module.
//! 2. Spin up a [`RecorderState`] and a tiny stack-machine
//!    interpreter that **runs the IR for real** while feeding each
//!    op into the recorder. The dual-walk lets the host complete the
//!    current invocation (the recorder doesn't execute on its own)
//!    and at the same time collect a finalisable trace buffer.
//! 3. Finalise the recorder → optimiser → emitter → JIT install via
//!    [`crate::trace_install::TraceJitState::jit_compile_trace_for_fn`].
//!
//! ## Scope
//!
//! The IR walker handles the **Phase-1 hot subset** the recorder's
//! lowering rules accept today:
//!
//! - Const ops: `ConstI32` / `ConstI64` / `ConstBool`.
//! - Arithmetic: `Add` / `Sub` / `Mul` / `Div` (I64 only; the recorder
//!   aborts on float arith).
//! - Comparisons: `Eq` / `Ne` / `Lt` / `Le` / `Gt` / `Ge`.
//! - Locals: `LocalGet` / `LetGet` / `LetSet`.
//! - Terminator: `Return`.
//!
//! Anything outside this set (strings, list ops, calls, control flow)
//! aborts the recording with `AbortReason::UnsupportedOp`. The
//! cranelift-generic backend then keeps handling the cold path; the
//! HotCounter saturates so the helper won't retry until reset.
//!
//! ## Why not reuse `relon-evaluator::TreeWalkEvaluator`?
//!
//! The tree-walker is a Value-based AST interpreter — it operates on
//! AST nodes, not flat IR. The trace recorder consumes flat IR ops, so
//! the two walkers can't share a backbone. Re-implementing the subset
//! here keeps the recording driver under ~300 lines and avoids
//! pulling the whole `relon-evaluator` crate into a layer that is
//! supposed to feed off the cranelift IR.

use std::collections::HashMap;

use relon_ir::{IrType, Op, TaggedOp};
use relon_trace_abi::ObservedType;
use relon_trace_jit::SsaVar;
use relon_trace_recorder::{AbortReason, RecordResult, RecorderState};

/// One value the recording-evaluator pushes on its operand stack while
/// walking the IR.
///
/// The cell holds **both** the concrete runtime value (so the walker
/// can compute results) and the SSA id the recorder has bound to it
/// (so the next op the recorder lowers can reference the right SSA).
#[derive(Debug, Clone, Copy)]
pub struct StackCell {
    /// Concrete value. `u64` is wide enough for every type in the
    /// Phase-1 hot subset: `i32` / `i64` / `bool` all fit, and the
    /// recorder rejects `f64`-typed arithmetic up front so we never
    /// need to carry a float bit-pattern around.
    pub value: u64,
    /// SSA id the recorder allocated for this value. The next op the
    /// recorder lowers receives this id as one of its inputs.
    pub ssa: SsaVar,
    /// Observed type tag. Drives the recorder's TypeCheck guard
    /// policy and lets the walker pick the right integer width on
    /// arithmetic.
    pub ty: ObservedType,
}

impl StackCell {
    /// Construct a new cell.
    pub fn new(value: u64, ssa: SsaVar, ty: ObservedType) -> Self {
        Self { value, ssa, ty }
    }
}

/// Outcome of one `record_and_run` invocation.
///
/// The `RecorderState` carried by the `Recorded` arm is large (HashMaps
/// plus a `TraceBuffer`); we box it so the enum stays cheap to move
/// through the install pipeline.
#[derive(Debug)]
pub enum RecordingOutcome {
    /// The walker completed the function, the trace was finalised,
    /// and the JIT-installable [`RecorderState`] is ready for the
    /// caller. The `result` field holds the concrete u64 value the
    /// IR computed during this invocation.
    Recorded {
        recorder: Box<RecorderState>,
        result: u64,
    },
    /// The walker hit an op outside the trace-safe subset and the
    /// recorder aborted. The carried `result` is whatever value the
    /// walker had produced before the abort (defaults to 0 if no
    /// value was on the stack).
    Aborted {
        reason: AbortReason,
        partial_result: u64,
    },
}

/// IR walker that records into a [`RecorderState`] as it executes.
///
/// The walker is single-shot: it consumes a function body's op stream,
/// produces a `RecordingOutcome`, and is dropped. Callers build a
/// fresh walker per `__relon_jump_to_recorder` invocation.
pub struct TraceRecordingEvaluator<'a> {
    recorder: &'a mut RecorderState,
    operand_stack: Vec<StackCell>,
    /// Arg slots the host passed via the `args_ptr` second argument
    /// to the cranelift entry helper. Indexed by [`Op::LocalGet`].
    /// v6-γ M4 keeps this conservatively typed as `i64`; production
    /// hosts that pass `i32` handshake slots will need to widen
    /// before calling. Stored as raw u64 plus the `IrType` declared
    /// by the function's `params` vector.
    args: &'a [(u64, IrType)],
    /// `let`-bound locals. Filled in by `Op::LetSet`, read by
    /// `Op::LetGet`. Keyed on the let-index.
    let_slots: HashMap<u32, StackCell>,
}

impl<'a> TraceRecordingEvaluator<'a> {
    /// Construct a walker bound to `recorder` and the supplied
    /// argument slots.
    pub fn new(recorder: &'a mut RecorderState, args: &'a [(u64, IrType)]) -> Self {
        Self {
            recorder,
            operand_stack: Vec::with_capacity(32),
            args,
            let_slots: HashMap::new(),
        }
    }

    /// Walk `body` op-by-op, recording each op into the recorder
    /// state. Returns the final return value on success.
    ///
    /// On abort the function returns immediately; subsequent ops are
    /// skipped. The recorder is left in its sticky `aborted` state so
    /// the caller can still inspect it.
    pub fn run(mut self, body: &[TaggedOp]) -> u64 {
        let mut result_value: u64 = 0;
        for tagged in body {
            if self.recorder.is_aborted() || self.recorder.is_terminated() {
                break;
            }
            match self.step_one(&tagged.op) {
                StepOutcome::Continue => {}
                StepOutcome::Return(v) => {
                    result_value = v;
                    break;
                }
                StepOutcome::Abort => {
                    // Recorder has already flipped its sticky abort
                    // flag. Walker exits with whatever happens to be
                    // on top of the stack so callers can fall back
                    // to the generic path with at least one valid
                    // value.
                    if let Some(top) = self.operand_stack.last() {
                        result_value = top.value;
                    }
                    break;
                }
            }
        }
        result_value
    }

    /// Run the walker against `body`, returning a [`RecordingOutcome`]
    /// the caller routes into the install pipeline.
    pub fn record_and_run(
        recorder: &mut RecorderState,
        args: &[(u64, IrType)],
        body: &[TaggedOp],
    ) -> RecordingOutcome {
        let walker = TraceRecordingEvaluator::new(recorder, args);
        let result = walker.run(body);
        if let Some(reason) = recorder.abort_reason() {
            return RecordingOutcome::Aborted {
                reason,
                partial_result: result,
            };
        }
        // Even if the recorder is not terminated (the IR body has no
        // explicit Return — uncommon, but possible for synthetic
        // fragments), we still return Recorded so the caller can
        // decide whether to install or fall back.
        let owned = std::mem::replace(recorder, RecorderState::new());
        RecordingOutcome::Recorded {
            recorder: Box::new(owned),
            result,
        }
    }

    fn step_one(&mut self, op: &Op) -> StepOutcome {
        match op {
            Op::ConstI32(v) => self.step_const(*v as u64, ObservedType::I32, op),
            Op::ConstI64(v) => self.step_const(*v as u64, ObservedType::I64, op),
            Op::ConstBool(v) => self.step_const(u64::from(*v), ObservedType::Bool, op),

            Op::Add(IrType::I64) => {
                self.step_arith(op, |a, b| (a as i64).wrapping_add(b as i64) as u64)
            }
            Op::Sub(IrType::I64) => {
                self.step_arith(op, |a, b| (a as i64).wrapping_sub(b as i64) as u64)
            }
            Op::Mul(IrType::I64) => {
                self.step_arith(op, |a, b| (a as i64).wrapping_mul(b as i64) as u64)
            }
            Op::Div(IrType::I64) => self.step_div(op),

            Op::Eq(IrType::I64) => self.step_cmp(op, |a, b| a == b),
            Op::Ne(IrType::I64) => self.step_cmp(op, |a, b| a != b),
            Op::Lt(IrType::I64) => self.step_cmp(op, |a, b| (a as i64) < (b as i64)),
            Op::Le(IrType::I64) => self.step_cmp(op, |a, b| (a as i64) <= (b as i64)),
            Op::Gt(IrType::I64) => self.step_cmp(op, |a, b| (a as i64) > (b as i64)),
            Op::Ge(IrType::I64) => self.step_cmp(op, |a, b| (a as i64) >= (b as i64)),

            Op::LocalGet(idx) => self.step_local_get(*idx, op),
            Op::LetGet { idx, ty } => self.step_let_get(*idx, *ty, op),
            Op::LetSet { idx, .. } => self.step_let_set(*idx, op),

            Op::Return => {
                let v = self.operand_stack.last().map(|c| c.value).unwrap_or(0);
                let inputs: Vec<SsaVar> = self
                    .operand_stack
                    .last()
                    .map(|c| vec![c.ssa])
                    .unwrap_or_default();
                let res = self.recorder.record_op(op, &inputs, None);
                if matches!(res, RecordResult::Terminated) {
                    StepOutcome::Return(v)
                } else {
                    StepOutcome::Abort
                }
            }

            // Everything outside the Phase-1 subset bounces off the
            // recorder. The recorder's lowering rule will Abort with
            // an UnsupportedOp variant carrying the right diagnostic
            // string, so we just forward the op as-is and check the
            // result.
            other => {
                // We try `record_op` with whatever inputs are on the
                // stack — the recorder is responsible for surfacing
                // UnsupportedOp at the right spot.
                let inputs: Vec<SsaVar> = self.operand_stack.iter().map(|c| c.ssa).collect();
                let _ = self.recorder.record_op(other, &inputs, None);
                StepOutcome::Abort
            }
        }
    }

    fn step_const(&mut self, raw: u64, ty: ObservedType, op: &Op) -> StepOutcome {
        match self.recorder.record_op(op, &[], Some(ty)) {
            RecordResult::Ok { value: Some(ssa) } => {
                self.operand_stack.push(StackCell::new(raw, ssa, ty));
                StepOutcome::Continue
            }
            RecordResult::NeedsGuard {
                value: Some(ssa), ..
            } => {
                self.operand_stack.push(StackCell::new(raw, ssa, ty));
                StepOutcome::Continue
            }
            _ => StepOutcome::Abort,
        }
    }

    fn step_arith(&mut self, op: &Op, compute: impl Fn(u64, u64) -> u64) -> StepOutcome {
        if self.operand_stack.len() < 2 {
            return StepOutcome::Abort;
        }
        let rhs = self.operand_stack.pop().expect("checked above");
        let lhs = self.operand_stack.pop().expect("checked above");
        let inputs = [rhs.ssa, lhs.ssa];
        let result_value = compute(lhs.value, rhs.value);
        match self
            .recorder
            .record_op(op, &inputs, Some(ObservedType::I64))
        {
            RecordResult::Ok { value: Some(ssa) }
            | RecordResult::NeedsGuard {
                value: Some(ssa), ..
            } => {
                self.operand_stack
                    .push(StackCell::new(result_value, ssa, ObservedType::I64));
                StepOutcome::Continue
            }
            _ => StepOutcome::Abort,
        }
    }

    fn step_div(&mut self, op: &Op) -> StepOutcome {
        if self.operand_stack.len() < 2 {
            return StepOutcome::Abort;
        }
        let rhs = self.operand_stack.pop().expect("checked above");
        let lhs = self.operand_stack.pop().expect("checked above");
        // Guard against div-by-zero during the *recording* walk so we
        // never panic the host. The cranelift-generic backend would
        // surface DivisionByZero through its own trap path; in the
        // recording case we just abort the trace and let the cold
        // path replay.
        if rhs.value as i64 == 0 {
            self.recorder.abort(AbortReason::GuardFailureInRecording);
            return StepOutcome::Abort;
        }
        let inputs = [rhs.ssa, lhs.ssa];
        let result_value = ((lhs.value as i64).wrapping_div(rhs.value as i64)) as u64;
        match self
            .recorder
            .record_op(op, &inputs, Some(ObservedType::I64))
        {
            RecordResult::Ok { value: Some(ssa) }
            | RecordResult::NeedsGuard {
                value: Some(ssa), ..
            } => {
                self.operand_stack
                    .push(StackCell::new(result_value, ssa, ObservedType::I64));
                StepOutcome::Continue
            }
            _ => StepOutcome::Abort,
        }
    }

    fn step_cmp(&mut self, op: &Op, predicate: impl Fn(u64, u64) -> bool) -> StepOutcome {
        if self.operand_stack.len() < 2 {
            return StepOutcome::Abort;
        }
        let rhs = self.operand_stack.pop().expect("checked above");
        let lhs = self.operand_stack.pop().expect("checked above");
        let inputs = [rhs.ssa, lhs.ssa];
        let result_value = u64::from(predicate(lhs.value, rhs.value));
        match self
            .recorder
            .record_op(op, &inputs, Some(ObservedType::Bool))
        {
            RecordResult::Ok { value: Some(ssa) }
            | RecordResult::NeedsGuard {
                value: Some(ssa), ..
            } => {
                self.operand_stack
                    .push(StackCell::new(result_value, ssa, ObservedType::Bool));
                StepOutcome::Continue
            }
            _ => StepOutcome::Abort,
        }
    }

    fn step_local_get(&mut self, idx: u32, op: &Op) -> StepOutcome {
        let (raw, ty) = match self.args.get(idx as usize) {
            Some((raw, ir_ty)) => (*raw, observed_from_ir(*ir_ty)),
            None => {
                self.recorder
                    .abort(AbortReason::UnsupportedOp("LocalGetUnderflow"));
                return StepOutcome::Abort;
            }
        };
        match self.recorder.record_op(op, &[], Some(ty)) {
            RecordResult::Ok { value: Some(ssa) }
            | RecordResult::NeedsGuard {
                value: Some(ssa), ..
            } => {
                self.operand_stack.push(StackCell::new(raw, ssa, ty));
                StepOutcome::Continue
            }
            _ => StepOutcome::Abort,
        }
    }

    fn step_let_get(&mut self, idx: u32, ty: IrType, op: &Op) -> StepOutcome {
        let observed = observed_from_ir(ty);
        match self.let_slots.get(&idx).copied() {
            Some(cell) => match self.recorder.record_op(op, &[], Some(observed)) {
                RecordResult::Ok { value: Some(ssa) }
                | RecordResult::NeedsGuard {
                    value: Some(ssa), ..
                } => {
                    self.operand_stack
                        .push(StackCell::new(cell.value, ssa, observed));
                    StepOutcome::Continue
                }
                _ => StepOutcome::Abort,
            },
            None => {
                self.recorder
                    .abort(AbortReason::UnsupportedOp("LetGetUnbound"));
                StepOutcome::Abort
            }
        }
    }

    fn step_let_set(&mut self, idx: u32, op: &Op) -> StepOutcome {
        let top = match self.operand_stack.pop() {
            Some(c) => c,
            None => {
                self.recorder
                    .abort(AbortReason::UnsupportedOp("LetSetUnderflow"));
                return StepOutcome::Abort;
            }
        };
        // Recorder's `apply_outcome` rebinds the let-slot via the
        // first input SSA — we mirror that with our concrete-value
        // table here.
        let inputs = [top.ssa];
        let _ = self.recorder.record_op(op, &inputs, None);
        self.let_slots.insert(idx, top);
        StepOutcome::Continue
    }
}

#[derive(Debug)]
enum StepOutcome {
    /// The walker may continue with the next op.
    Continue,
    /// The walker hit a `Return` op; carries the return value.
    Return(u64),
    /// The walker / recorder aborted; the caller should fall back to
    /// the generic backend.
    Abort,
}

fn observed_from_ir(ty: IrType) -> ObservedType {
    match ty {
        IrType::I32 => ObservedType::I32,
        IrType::I64 => ObservedType::I64,
        IrType::F64 => ObservedType::F64,
        IrType::Bool => ObservedType::Bool,
        _ => ObservedType::Ptr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_ir::TaggedOp;
    use relon_parser::TokenRange;

    fn tag(op: Op) -> TaggedOp {
        TaggedOp {
            op,
            range: TokenRange::default(),
        }
    }

    #[test]
    fn const_then_return_records_and_runs() {
        let mut recorder = RecorderState::new();
        let body = vec![tag(Op::ConstI64(42)), tag(Op::Return)];
        let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &[], &body);
        match outcome {
            RecordingOutcome::Recorded { recorder, result } => {
                assert_eq!(result, 42);
                assert!(recorder.is_terminated());
            }
            RecordingOutcome::Aborted { reason, .. } => {
                panic!("expected Recorded, got Aborted({reason:?})");
            }
        }
    }

    #[test]
    fn add_two_consts_yields_sum() {
        let mut recorder = RecorderState::new();
        let body = vec![
            tag(Op::ConstI64(11)),
            tag(Op::ConstI64(22)),
            tag(Op::Add(IrType::I64)),
            tag(Op::Return),
        ];
        let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &[], &body);
        match outcome {
            RecordingOutcome::Recorded { result, .. } => assert_eq!(result, 33),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn sub_mul_div_compose_correctly() {
        let mut recorder = RecorderState::new();
        // ((10 - 3) * 2) / 7 = 14 / 7 = 2
        let body = vec![
            tag(Op::ConstI64(10)),
            tag(Op::ConstI64(3)),
            tag(Op::Sub(IrType::I64)),
            tag(Op::ConstI64(2)),
            tag(Op::Mul(IrType::I64)),
            tag(Op::ConstI64(7)),
            tag(Op::Div(IrType::I64)),
            tag(Op::Return),
        ];
        let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &[], &body);
        match outcome {
            RecordingOutcome::Recorded { result, .. } => assert_eq!(result, 2),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn local_get_pulls_from_args() {
        // LocalGet today is exclusively used for the wasm-handshake
        // slots which the recorder seeds as I32 (lowering.rs `ty_hint
        // = ObservedType::I32`). Pass an I32-typed slot so the
        // recorder doesn't flag a mismatch.
        let mut recorder = RecorderState::new();
        let body = vec![tag(Op::LocalGet(0)), tag(Op::Return)];
        let args = [(99u64, IrType::I32)];
        let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &args, &body);
        match outcome {
            RecordingOutcome::Recorded { result, .. } => assert_eq!(result, 99),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn let_set_then_let_get_round_trips() {
        let mut recorder = RecorderState::new();
        let body = vec![
            tag(Op::ConstI64(77)),
            tag(Op::LetSet {
                idx: 0,
                ty: IrType::I64,
            }),
            tag(Op::LetGet {
                idx: 0,
                ty: IrType::I64,
            }),
            tag(Op::Return),
        ];
        let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &[], &body);
        match outcome {
            RecordingOutcome::Recorded { result, .. } => assert_eq!(result, 77),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn cmp_lt_produces_bool() {
        let mut recorder = RecorderState::new();
        let body = vec![
            tag(Op::ConstI64(3)),
            tag(Op::ConstI64(7)),
            tag(Op::Lt(IrType::I64)),
            tag(Op::Return),
        ];
        let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &[], &body);
        match outcome {
            RecordingOutcome::Recorded { result, .. } => assert_eq!(result, 1),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn div_by_zero_aborts_cleanly() {
        let mut recorder = RecorderState::new();
        let body = vec![
            tag(Op::ConstI64(10)),
            tag(Op::ConstI64(0)),
            tag(Op::Div(IrType::I64)),
            tag(Op::Return),
        ];
        let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &[], &body);
        assert!(matches!(outcome, RecordingOutcome::Aborted { .. }));
    }

    #[test]
    fn float_arith_aborts() {
        let mut recorder = RecorderState::new();
        let body = vec![
            tag(Op::ConstI64(1)),
            tag(Op::ConstI64(2)),
            tag(Op::Add(IrType::F64)),
            tag(Op::Return),
        ];
        let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &[], &body);
        assert!(matches!(outcome, RecordingOutcome::Aborted { .. }));
    }
}
