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
//! lowering rules accept today, **plus v6-γ M5 widening**:
//!
//! - Const ops: `ConstI32` / `ConstI64` / `ConstBool`.
//! - Arithmetic: `Add` / `Sub` / `Mul` / `Div` (I64 only; the recorder
//!   aborts on float arith). `Mod` is recognised but the recorder
//!   lowering surfaces it as `UnsupportedOp("Mod")` — the walker
//!   forwards the abort so the corpus harness sees the correct
//!   reason rather than a silent skip.
//! - Comparisons: `Eq` / `Ne` / `Lt` / `Le` / `Gt` / `Ge`.
//! - Locals: `LocalGet` / `LetGet` / `LetSet`.
//! - Control flow: `Op::If { result_ty, then_body, else_body }` and
//!   `Op::Select { ty }`. The walker follows the **taken** branch
//!   based on the runtime value of the condition operand, emits a
//!   `Guard NotNull(cond_ssa)` so the trace deopts if a future
//!   invocation's condition flips, and recurses into the taken body.
//!   Trace-IR has no native `If` op; this single-arm specialisation
//!   matches the LuaJIT-style trace-tier philosophy.
//! - Terminator: `Return`.
//!
//! Anything outside this set (strings, list ops, calls, loops) aborts
//! the recording with `AbortReason::UnsupportedOp`. The
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
use relon_trace_recorder::{AbortReason, LookupKind, LoopCarry, RecordResult, RecorderState};

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
        self.walk_body(body, &mut result_value);
        result_value
    }

    /// Walk `body` in place against the current operand stack /
    /// recorder state. Updates `result_value` on `Return`. Used both
    /// for the outer function body and for the recursive `Op::If`
    /// taken-branch traversal.
    fn walk_body(&mut self, body: &[TaggedOp], result_value: &mut u64) -> WalkExit {
        for tagged in body {
            if self.recorder.is_aborted() || self.recorder.is_terminated() {
                return WalkExit::Aborted;
            }
            match self.step_one(&tagged.op) {
                StepOutcome::Continue => {}
                StepOutcome::Return(v) => {
                    *result_value = v;
                    return WalkExit::Returned;
                }
                StepOutcome::Abort => {
                    // Recorder has already flipped its sticky abort
                    // flag. Walker exits with whatever happens to be
                    // on top of the stack so callers can fall back
                    // to the generic path with at least one valid
                    // value.
                    if let Some(top) = self.operand_stack.last() {
                        *result_value = top.value;
                    }
                    return WalkExit::Aborted;
                }
                StepOutcome::BreakOut(depth) => {
                    return WalkExit::BreakOut(depth);
                }
            }
        }
        WalkExit::Fallthrough
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

            // Integer arithmetic: I32 / I64 / Bool widths all pass
            // through the recorder lowering as cranelift int ops
            // (`binary_arith` rejects only F64). The walker performs
            // the arithmetic in i64 wrapping space regardless of the
            // declared tag — sufficient because the corpus's Int
            // values fit i64 and the cranelift backend itself widens
            // narrower types to i64 at emit time.
            Op::Add(ty) if !matches!(ty, IrType::F64) => {
                self.step_arith(op, |a, b| (a as i64).wrapping_add(b as i64) as u64)
            }
            Op::Sub(ty) if !matches!(ty, IrType::F64) => {
                self.step_arith(op, |a, b| (a as i64).wrapping_sub(b as i64) as u64)
            }
            Op::Mul(ty) if !matches!(ty, IrType::F64) => {
                self.step_arith(op, |a, b| (a as i64).wrapping_mul(b as i64) as u64)
            }
            Op::Div(ty) if !matches!(ty, IrType::F64) => self.step_div(op),

            // Comparisons: same envelope as arith. Walker forwards
            // the op verbatim so the recorder's `binary_cmp` rule
            // accepts each numeric tag.
            Op::Eq(_) => self.step_cmp(op, |a, b| a == b),
            Op::Ne(_) => self.step_cmp(op, |a, b| a != b),
            Op::Lt(_) => self.step_cmp(op, |a, b| (a as i64) < (b as i64)),
            Op::Le(_) => self.step_cmp(op, |a, b| (a as i64) <= (b as i64)),
            Op::Gt(_) => self.step_cmp(op, |a, b| (a as i64) > (b as i64)),
            Op::Ge(_) => self.step_cmp(op, |a, b| (a as i64) >= (b as i64)),

            Op::LocalGet(idx) => self.step_local_get(*idx, op),
            Op::LetGet { idx, ty } => self.step_let_get(*idx, *ty, op),
            Op::LetSet { idx, .. } => self.step_let_set(*idx, op),

            // v6-γ M5: `Op::If` taken-branch specialisation. We pop
            // the boolean condition (already recorded as a `Cmp`
            // result), emit a `NotNull(cond)` branch guard so the
            // installed trace deopts if a future invocation flips
            // the branch, then recurse into the taken body. The
            // trace-IR has no native If op; we never call
            // `record_op(&Op::If, ...)` (which would abort).
            Op::If {
                result_ty: _,
                then_body,
                else_body,
            } => self.step_if(then_body, else_body),

            // v6-γ M5: `Op::Select` — `[val_true, val_false, cond] -> result`.
            // Same taken-branch specialisation as `If` but inline
            // (no nested body).
            Op::Select { ty: _ } => self.step_select(),

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

            // ε-M0: structured control-flow recursion.
            //
            // `Op::Block { body }` opens a new label frame; the inner
            // body's `Op::Br { label_depth: 0 }` exits the block.
            // We do NOT emit a trace-IR op for the block itself — it's
            // a static scoping construct visible only via the
            // surrounding `BrIf` / `Br` `label_depth` arithmetic.
            Op::Block { result_ty, body } => self.step_block(*result_ty, body),

            // `Op::Loop { body }` opens a back-edge label frame and
            // emits matching `MarkLoopHead` / `MarkLoopBack` markers
            // around the body. The body's `Op::Br { label_depth: 0 }`
            // is the back-edge (continue); a deeper depth exits.
            Op::Loop { result_ty, body } => self.step_loop(*result_ty, body),

            // `Op::Br { label_depth }` is a static branch out of the
            // enclosing labelled construct. We bubble up through
            // `walk_body` as `WalkExit::BreakOut(depth)`; the matching
            // block / loop frame catches it.
            Op::Br { label_depth } => {
                let depth = *label_depth;
                // Record the IR op so the recorder's PC counter stays
                // aligned with the IR walker's view (mirrors the
                // bytecode compiler's per-op `ir_pc_next` increment).
                let _ = self.recorder.record_op(op, &[], None);
                StepOutcome::BreakOut(depth)
            }

            // `Op::BrIf { label_depth }` — pop a Bool cond; if truthy
            // we behave as `Op::Br`, otherwise fall through. We emit a
            // branch guard so the trace deopts on the polarity flip.
            //
            // Polarity matters: when the recording observed the
            // **fall-through** path (cond=0), the trace's runtime
            // must keep the polarity stable — i.e. deopt if a future
            // iteration's `cond` becomes truthy (the BrIf would have
            // taken its branch). [`emit_branch_falsy_guard`] does
            // exactly that by synthesising a `Cmp(Eq, cond, 0)` SSA
            // and guarding on its NotNull. When the taken-arm was
            // recorded (cond!=0), the historical NotNull(cond) guard
            // is correct.
            Op::BrIf { label_depth } => {
                let cond = match self.operand_stack.pop() {
                    Some(c) => c,
                    None => {
                        self.recorder
                            .abort(AbortReason::UnsupportedOp("BrIfUnderflow"));
                        return StepOutcome::Abort;
                    }
                };
                let taken = cond.value != 0;
                if taken {
                    // Branch taken at recording → deopt if a future
                    // cond is 0 (would fall through).
                    let _ = self.recorder.emit_branch_guard(cond.ssa, taken);
                    StepOutcome::BreakOut(*label_depth)
                } else {
                    // Branch NOT taken at recording → deopt if a
                    // future cond is non-zero (would branch).
                    let _ = self.recorder.emit_branch_falsy_guard(cond.ssa);
                    StepOutcome::Continue
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

    /// v6-γ M5: walker side of `Op::If`. The cranelift backend's IR
    /// emits the cond op into the same body before the `Op::If`, so
    /// the condition's SSA / runtime value already sits on the
    /// operand stack. We pop, guard, and recurse.
    fn step_if(&mut self, then_body: &[TaggedOp], else_body: &[TaggedOp]) -> StepOutcome {
        let cond = match self.operand_stack.pop() {
            Some(c) => c,
            None => {
                self.recorder
                    .abort(AbortReason::UnsupportedOp("IfUnderflow"));
                return StepOutcome::Abort;
            }
        };
        // Bool is non-zero-truthy at the wasm slot level; non-zero
        // means take the `then` arm.
        let taken_truthy = cond.value != 0;
        let _guard = self.recorder.emit_branch_guard(cond.ssa, taken_truthy);
        let arm = if taken_truthy { then_body } else { else_body };
        let mut result = 0u64;
        match self.walk_body(arm, &mut result) {
            WalkExit::Fallthrough => StepOutcome::Continue,
            WalkExit::Returned => StepOutcome::Return(result),
            WalkExit::Aborted => StepOutcome::Abort,
            // ε-M0: a Br inside the If's arm pierces the If wrapper —
            // the `If` is not a labelled construct, so we forward the
            // unmodified depth up to the enclosing Block / Loop frame.
            WalkExit::BreakOut(d) => StepOutcome::BreakOut(d),
        }
    }

    /// v6-γ M5: walker side of `Op::Select`. Pops `(cond, false_val,
    /// true_val)` (top first → cond), emits the same branch guard as
    /// `step_if`, and pushes the chosen value.
    fn step_select(&mut self) -> StepOutcome {
        if self.operand_stack.len() < 3 {
            self.recorder
                .abort(AbortReason::UnsupportedOp("SelectUnderflow"));
            return StepOutcome::Abort;
        }
        let cond = self.operand_stack.pop().expect("checked above");
        let val_false = self.operand_stack.pop().expect("checked above");
        let val_true = self.operand_stack.pop().expect("checked above");
        let taken_truthy = cond.value != 0;
        let _guard = self.recorder.emit_branch_guard(cond.ssa, taken_truthy);
        let chosen = if taken_truthy { val_true } else { val_false };
        self.operand_stack.push(chosen);
        StepOutcome::Continue
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

    /// ε-M0: walker side of `Op::Block { result_ty, body }`.
    ///
    /// A block is a forward-exit-only construct: inner `Op::Br` /
    /// `Op::BrIf` with `label_depth = 0` jumps past the block end.
    /// Nested constructs decrement the depth as they're crossed.
    ///
    /// For `result_ty: Some(_)`, a `Br`-out-with-yield leaves the
    /// yielded value on the operand stack. We rely on the caller's
    /// `Op::Br` lowering to NOT clear the stack; the IR contract
    /// matches the cranelift-AOT side (see codegen.rs `Op::Block`
    /// frame handling).
    fn step_block(&mut self, result_ty: Option<IrType>, body: &[TaggedOp]) -> StepOutcome {
        let _ = result_ty;
        let mut inner_result = 0u64;
        match self.walk_body(body, &mut inner_result) {
            WalkExit::Fallthrough => StepOutcome::Continue,
            WalkExit::Returned => StepOutcome::Return(inner_result),
            WalkExit::Aborted => StepOutcome::Abort,
            WalkExit::BreakOut(0) => {
                // Block exit: branch landed here. Continue with the
                // outer body. The Br-side already pushed any yield
                // value (for typed blocks) onto the operand stack.
                StepOutcome::Continue
            }
            WalkExit::BreakOut(d) => StepOutcome::BreakOut(d - 1),
        }
    }

    /// ε-M0: walker side of `Op::Loop { result_ty, body }`.
    ///
    /// A loop's `body` is recorded **once**; the recorder emits
    /// `MarkLoopHead` / `MarkLoopBack` markers around the body and
    /// the cranelift emitter wires the back-edge. Inner
    /// `Op::Br { label_depth: 0 }` is the back-edge (continue); deeper
    /// depths exit.
    ///
    /// Loop-carried let-slots are detected by a pre-scan: any
    /// let-slot that gets `LetSet` inside the body becomes a φ
    /// carried through the head/back markers.
    fn step_loop(&mut self, result_ty: Option<IrType>, body: &[TaggedOp]) -> StepOutcome {
        let _ = result_ty;

        // Pre-scan to collect every let-slot that gets re-assigned
        // anywhere in the body tree. These are the loop-carried
        // slots — the recorder emits a φ pair for each.
        let carried_slots = collect_let_set_slots(body);

        // Build the recorder's view: for each carried slot, the
        // current cell's SSA is the φ's `init`, and we remember its
        // observed type. Slots that the body writes without an
        // outer-scope seed (e.g. `Op::If` yield-sink slots) get a
        // synthetic zero seed so the recorder can still produce a
        // valid φ pair — the body's first `LetSet` will overwrite
        // the seed before any `LetGet` reads it.
        let mut carries: Vec<LoopCarry> = Vec::with_capacity(carried_slots.len());
        let mut carry_slot_idx: Vec<u32> = Vec::with_capacity(carried_slots.len());
        let mut synth_seeds: Vec<(u32, StackCell)> = Vec::new();
        for slot in &carried_slots {
            let cell = match self.let_slots.get(slot).copied() {
                Some(c) => c,
                None => {
                    // ε-M0 relaxation: emit a synthetic
                    // `Op::ConstI64(0)` pre-loop to seed the slot.
                    // The recorder's lowering for `Op::ConstI64`
                    // emits the matching `TraceOp::ConstI64` to the
                    // buffer; we feed the produced SSA into the
                    // φ-pair's `init`. This lets the body's
                    // `Op::LetSet` rebind freely on the first
                    // iteration without aborting.
                    let res =
                        self.recorder
                            .record_op(&Op::ConstI64(0), &[], Some(ObservedType::I64));
                    let ssa = match res {
                        relon_trace_recorder::RecordResult::Ok { value: Some(v) } => v,
                        relon_trace_recorder::RecordResult::NeedsGuard {
                            value: Some(v), ..
                        } => v,
                        _ => {
                            self.recorder
                                .abort(AbortReason::UnsupportedOp("LoopCarriedSynthSeed"));
                            return StepOutcome::Abort;
                        }
                    };
                    let synth = StackCell::new(0u64, ssa, ObservedType::I64);
                    synth_seeds.push((*slot, synth));
                    synth
                }
            };
            // Pass the recorder's `LookupKind::Let(slot)` key so it
            // rebinds `ir_to_ssa[Let(slot)] = phi_ssa` for body
            // recording. Critical: without this rebind, body
            // `LetGet(slot)` lowering resolves to the stale pre-loop
            // SSA and the φ never sees any body update.
            carries.push(LoopCarry::with_key(
                cell.ssa,
                cell.ty,
                LookupKind::Let(*slot),
            ));
            carry_slot_idx.push(*slot);
        }
        // Persist the synthetic seeds into the walker's let_slots so
        // subsequent body reads observe them too (the recorder side
        // already has the SSA mapping via `with_key`).
        for (slot, cell) in synth_seeds {
            self.let_slots.insert(slot, cell);
        }

        // Open the loop frame on the recorder. This appends a
        // `MarkLoopHead { loop_id, phis }` to the buffer; the returned
        // φ SSAs are the new SSA ids visible inside the body for the
        // carried slots.
        let phi_ssas = self.recorder.begin_loop(&carries);
        if phi_ssas.len() != carries.len() {
            // begin_loop short-circuits on a stale-state recorder.
            return StepOutcome::Abort;
        }

        // Re-bind the walker's let_slots so subsequent `LetGet` reads
        // (during body recording) observe the φ SSA, while keeping the
        // concrete u64 value identical (we still record only one
        // iteration).
        for (slot, phi) in carry_slot_idx.iter().zip(phi_ssas.iter()) {
            if let Some(cell) = self.let_slots.get_mut(slot) {
                cell.ssa = *phi;
            }
        }

        // Walk the body once. The body's `Op::Br { label_depth: 0 }`
        // is the back-edge; deeper depths are forward exits.
        let mut inner_result = 0u64;
        let exit = self.walk_body(body, &mut inner_result);

        // Collect the post-body SSAs for the carried slots — these
        // drive the `MarkLoopBack` next_values.
        let mut next_values: Vec<SsaVar> = Vec::with_capacity(carry_slot_idx.len());
        for slot in &carry_slot_idx {
            let v = self
                .let_slots
                .get(slot)
                .map(|c| c.ssa)
                .unwrap_or(SsaVar::NONE);
            next_values.push(v);
        }
        // Emit the back-edge marker.
        if !self.recorder.end_loop(&next_values) {
            return StepOutcome::Abort;
        }

        match exit {
            WalkExit::Fallthrough | WalkExit::BreakOut(0) => {
                // Fall-through end of body == back-edge target on the
                // recorded trace. Caller's next op is reached only
                // when the loop falls out (a deeper Br lands past us);
                // since we recorded only the taken iteration the
                // outer walker proceeds with whatever the runtime
                // value happened to be at body end. The `MarkLoopBack`
                // already emitted handles the recorded loop's
                // back-edge runtime semantics.
                StepOutcome::Continue
            }
            WalkExit::BreakOut(d) => StepOutcome::BreakOut(d - 1),
            WalkExit::Returned => StepOutcome::Return(inner_result),
            WalkExit::Aborted => StepOutcome::Abort,
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
    /// ε-M0: an `Op::Br` / `Op::BrIf` with the given label_depth.
    /// Propagates up through `walk_body` as `WalkExit::BreakOut(depth)`.
    BreakOut(u32),
}

/// Outcome of `walk_body`: either the body fell off the end (no
/// terminator), hit a `Return`, or aborted. Used by `step_if`'s
/// nested-body recursion to decide whether to keep walking the
/// outer body or propagate the exit up.
#[derive(Debug, Clone, Copy)]
enum WalkExit {
    /// Body finished without `Return` — the outer walker keeps going.
    Fallthrough,
    /// Body produced a `Return`; the caller should propagate.
    Returned,
    /// Body aborted; caller should propagate.
    Aborted,
    /// ε-M0: structured `Op::Br` / `Op::BrIf` with a non-zero
    /// label_depth. The current frame consumed depth 0 (one frame),
    /// so it propagates `BreakOut(depth - 1)` upward. The frame whose
    /// remaining `depth == 0` is the one the branch targets and
    /// converts the exit back into `Fallthrough` (block exit) or back
    /// into a back-edge (loop continue, handled by the loop frame
    /// itself before it returns).
    BreakOut(u32),
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

/// ε-M0: walk an IR op tree and collect every let-slot index that is
/// the target of an `Op::LetSet` anywhere in the tree.
///
/// Used by [`TraceRecordingEvaluator::step_loop`] to decide which
/// let-slots need a φ pair: a slot the body assigns to is a
/// loop-carried value; a slot the body only reads is loop-invariant
/// and can keep its outer SSA binding.
///
/// Recurses through nested `Op::Block`, `Op::Loop`, `Op::If`; ignores
/// `Op::LetGet` (read-only access).
fn collect_let_set_slots(body: &[TaggedOp]) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    walk_let_set(body, &mut out, &mut seen);
    out
}

fn walk_let_set(body: &[TaggedOp], out: &mut Vec<u32>, seen: &mut std::collections::HashSet<u32>) {
    for tagged in body {
        match &tagged.op {
            Op::LetSet { idx, .. } if seen.insert(*idx) => {
                out.push(*idx);
            }
            Op::Block { body, .. } | Op::Loop { body, .. } => {
                walk_let_set(body, out, seen);
            }
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                walk_let_set(then_body, out, seen);
                walk_let_set(else_body, out, seen);
            }
            _ => {}
        }
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
