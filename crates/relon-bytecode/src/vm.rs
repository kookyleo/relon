//! Stack-based bytecode VM with 4-prong sandbox engagement.
//!
//! Dispatch is `match`-based — computed-goto would require nightly
//! rust + the unstable `naked_functions` feature, and the M2-A target
//! is not perf (that's M2-C). The single tick-per-op resource counter
//! lives on [`BcVmConfig::max_steps`]; bounds / trap / capability
//! prongs trip through [`BcVmError`] variants that lift cleanly into
//! `relon_eval_api::RuntimeError`.

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use relon_eval_api::{CapabilityBit, CapabilityError, CapabilityGate, RuntimeError};
use relon_ir::IrType;
use relon_parser::TokenRange;
use thiserror::Error;

use crate::op::{BcFunction, BcOp, BcTrapKind, ExternalPc};

/// Raw VM-value slot. The bytecode VM is homogeneous on `u64` so
/// the dispatch arms don't switch on a tagged enum on every op.
/// Comparison / arithmetic ops decode the slot per IR type — `i64`
/// uses the slot directly; `f64` interprets via `f64::from_bits`;
/// `Bool` / `Null` take the low 32 bits.
pub type VmValue = u64;

/// VM configuration. Mirrors the sandbox knobs the tree-walker /
/// cranelift backends respect, on a per-call basis.
#[derive(Debug, Clone)]
pub struct BcVmConfig {
    /// Hard cap on dispatched bytecode ops. `None` disables resource
    /// accounting (used by perf benchmarks). Tree-walker parity:
    /// matches `Capabilities::max_steps`.
    pub max_steps: Option<u64>,
    /// Optional wall-clock deadline for the call. Mirrors the
    /// cranelift `SandboxState::deadline_ns` knob. v6-δ M2-A reads
    /// this on every tick when set; M2-C will sample less frequently
    /// to claw back overhead.
    pub deadline: Option<Instant>,
    /// Capability vtable indexed by `cap_bit`. Empty slots cause a
    /// guarded call to trip [`BcVmError::CapabilityDenied`]; non-empty
    /// slots are reserved for M2-B when host-fn dispatch lands.
    pub cap_vtable: CapabilityVtable,
}

impl Default for BcVmConfig {
    fn default() -> Self {
        // 1M instruction cap by default — generous enough that the
        // legacy-i64 corpus is uncapped in practice but tight enough
        // that an accidental infinite loop in a unit test surfaces
        // before the process hangs.
        Self {
            max_steps: Some(1_000_000),
            deadline: None,
            cap_vtable: CapabilityVtable::default(),
        }
    }
}

/// Per-call capability table. The slot at index `cap_bit` carries
/// `Some(_)` when the host has granted access; the actual host-fn
/// pointer payload is M2-B work. Today the table tracks **grants
/// only** so the capability prong can fire in the M2-A sandbox tests.
///
/// ## M2-B phase 1 — `CapabilityGate` hook
///
/// The optional `gate` field carries the unified
/// [`CapabilityGate`] policy (`relon_eval_api::capability`). When set,
/// it becomes the canonical authority for "is this capability bit
/// granted?" questions a future `BcOp::CallNative` / `BcOp::CheckCap`
/// op asks at dispatch time. The grant-bool vector stays as the
/// fallback for code paths that haven't been migrated yet — phase 2
/// will land the dispatch-side consult and trim the fallback's reach.
///
/// Native-fn slot payloads land in phase 2 alongside the new ops;
/// phase 1 only parks the trait hook so the field shape stops moving
/// across upcoming commits.
#[derive(Clone, Default)]
pub struct CapabilityVtable {
    grants: Vec<bool>,
    /// M2-B phase 1: optional shared gate consulted on every guarded
    /// op (phase 2 wires the actual `BcOp::CheckCap` consult). `None`
    /// preserves the M2-A grant-table-only behaviour so existing
    /// callers don't observe a change.
    gate: Option<Arc<dyn CapabilityGate>>,
}

impl fmt::Debug for CapabilityVtable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `dyn CapabilityGate` doesn't carry `Debug`; print presence
        // only so the surrounding `BcVmConfig` debug stays cheap and
        // doesn't force the trait surface wider than necessary.
        f.debug_struct("CapabilityVtable")
            .field("grants", &self.grants)
            .field("has_gate", &self.gate.is_some())
            .finish()
    }
}

impl CapabilityVtable {
    /// Grant capability `cap_bit`. Subsequent guarded calls against
    /// the slot see `Some` and proceed.
    pub fn grant(&mut self, cap_bit: u32) {
        let idx = cap_bit as usize;
        if idx >= self.grants.len() {
            self.grants.resize(idx + 1, false);
        }
        self.grants[idx] = true;
    }

    /// Inspect whether `cap_bit` is currently granted.
    pub fn is_granted(&self, cap_bit: u32) -> bool {
        self.grants.get(cap_bit as usize).copied().unwrap_or(false)
    }

    /// M2-B phase 1: install the unified [`CapabilityGate`] policy.
    /// Phase 2 wires this into the dispatch path; phase 1 only parks
    /// the slot so callers can opt in ahead of time.
    pub fn set_gate(&mut self, gate: Arc<dyn CapabilityGate>) {
        self.gate = Some(gate);
    }

    /// M2-B phase 1: read the installed gate, if any. Returns `None`
    /// when the caller hasn't opted into the unified policy boundary —
    /// the phase-2 dispatch consult treats this as "fall back to the
    /// grant-bool table".
    pub fn gate(&self) -> Option<&Arc<dyn CapabilityGate>> {
        self.gate.as_ref()
    }

    /// M2-B phase 2: consult the installed [`CapabilityGate`] for a
    /// single `cap_bit`. Returns `Ok(())` when:
    ///
    /// - no gate is installed (legacy fallback — grant table remains
    ///   the only check), OR
    /// - the gate explicitly grants the bit.
    ///
    /// Returns `Err(CapabilityError)` when the gate denies the bit.
    /// `cap_bit` outside the [`CapabilityBit`] enum's lower range is
    /// also treated as "no gate authority" — the legacy host-supplied
    /// `BcOp::Trap(CapabilityDenied)` carries `u32::MAX` which falls
    /// into this bucket and surfaces as the historical unbit-tagged
    /// denial.
    ///
    /// This helper is the dispatch-side single consult entry point
    /// future `BcOp::CallNative` / `BcOp::CheckCap` ops will use; it
    /// also drives the pre-dispatch `consult_all_granted_bits` sweep
    /// run from [`BytecodeVm::invoke_from_with_stack`].
    pub fn consult_gate(&self, cap_bit: u32) -> Result<(), CapabilityError> {
        let Some(gate) = self.gate.as_ref() else {
            return Ok(());
        };
        let Some(bit) = decode_cap_bit(cap_bit) else {
            return Ok(());
        };
        gate.check(bit)
    }

    /// M2-B phase 2: sweep every grant-table bit through the installed
    /// gate. Used as a dispatch-time pre-check so a gate that denies a
    /// previously-granted bit (e.g. trust-level downgrade between
    /// `grant` and `invoke`) trips the capability prong before any
    /// guarded op runs. No-op when no gate is installed; no-op when no
    /// bits are granted (the scaffold's default-empty-vtable case).
    pub fn consult_all_granted_bits(&self) -> Result<(), CapabilityError> {
        if self.gate.is_none() {
            return Ok(());
        }
        for (idx, granted) in self.grants.iter().enumerate() {
            if *granted {
                self.consult_gate(idx as u32)?;
            }
        }
        Ok(())
    }
}

/// M2-B phase 2: walk every declared [`CapabilityBit`] and return the
/// first one the supplied gate denies. Used to give the static
/// `BcOp::Trap(CapabilityDenied)` site a meaningful `cap_bit` to
/// report when a gate is installed but the trap fires from a
/// hand-built BcFunction that didn't carry the bit through the IR.
fn first_denied_bit(gate: &Arc<dyn CapabilityGate>) -> Option<u32> {
    for bit in [
        CapabilityBit::ReadsFs,
        CapabilityBit::WritesFs,
        CapabilityBit::Network,
        CapabilityBit::ReadsClock,
        CapabilityBit::ReadsEnv,
        CapabilityBit::UsesRng,
    ] {
        if gate.check(bit).is_err() {
            return Some(bit.bit_index());
        }
    }
    None
}

/// Map a numeric `cap_bit` to a [`CapabilityBit`] variant. Returns
/// `None` for bits outside the declared range — callers treat that as
/// "the bit predates the gate API and falls through to the legacy
/// grant-table path".
fn decode_cap_bit(cap_bit: u32) -> Option<CapabilityBit> {
    match cap_bit {
        0 => Some(CapabilityBit::ReadsFs),
        1 => Some(CapabilityBit::WritesFs),
        2 => Some(CapabilityBit::Network),
        3 => Some(CapabilityBit::ReadsClock),
        4 => Some(CapabilityBit::ReadsEnv),
        5 => Some(CapabilityBit::UsesRng),
        _ => None,
    }
}

/// VM-side error. Lifted to [`RuntimeError`] via
/// [`BcVmError::into_runtime_error`] so the surrounding evaluator
/// matches the tree-walker / cranelift `Evaluator::run_main` shape.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum BcVmError {
    /// Resource-prong trip: instruction count exceeded
    /// `BcVmConfig::max_steps`. Mirrors
    /// `RuntimeError::WasmStepLimitExceeded`.
    #[error("bytecode VM step limit exceeded after {steps} ops")]
    StepLimitExceeded {
        /// Total ops dispatched before the trip.
        steps: u64,
    },
    /// Resource-prong trip: wall-clock deadline elapsed.
    #[error("bytecode VM deadline exceeded")]
    DeadlineExceeded,
    /// Trap-prong trip: integer divide-by-zero / mod-by-zero.
    #[error("integer division by zero")]
    DivisionByZero,
    /// Trap-prong trip: signed integer overflow on Add / Sub / Mul.
    /// Tree-walker parity: matches `RuntimeError::NumericOverflow`.
    #[error("signed integer overflow")]
    NumericOverflow,
    /// Bounds-prong trip: a bytecode jump landed past the op stream
    /// (compiler bug / malformed bytecode).
    #[error("bytecode jump target {target} out of range (op count {ops})")]
    JumpOutOfRange {
        /// Requested target index.
        target: usize,
        /// Total op count.
        ops: usize,
    },
    /// Bounds-prong trip: an explicit `Op::Trap(IndexOutOfBounds)`
    /// fired (typically from stdlib substring / list index).
    #[error("index out of bounds")]
    IndexOutOfBounds,
    /// Trap-prong trip: empty-list reducer trap.
    #[error("operation on empty list")]
    EmptyList,
    /// Trap-prong trip: malformed UTF-8 trap.
    #[error("invalid utf-8")]
    InvalidUtf8,
    /// Capability-prong trip: a guarded call against an empty
    /// vtable slot.
    #[error("capability denied: cap_bit {cap_bit}")]
    CapabilityDenied {
        /// The denied capability bit.
        cap_bit: u32,
    },
    /// Defensive: the operand stack underflowed. Symptom of a
    /// compiler bug (the IR-level vstack validator should catch
    /// this); flagged loudly rather than silently propagating zero.
    #[error("operand stack underflow at bc_idx {bc_idx}")]
    StackUnderflow {
        /// Bytecode index of the offending op.
        bc_idx: usize,
    },
}

impl BcVmError {
    /// Lift a VM error into the public [`RuntimeError`] surface. The
    /// caller's `entry_range` is the `#main` declaration range, used
    /// when the trap envelope has no narrower source location to
    /// attach.
    pub fn into_runtime_error(self, entry_range: TokenRange) -> RuntimeError {
        match self {
            BcVmError::StepLimitExceeded { .. } => RuntimeError::WasmStepLimitExceeded {
                range: Some(entry_range),
            },
            BcVmError::DeadlineExceeded => RuntimeError::WasmStepLimitExceeded {
                range: Some(entry_range),
            },
            BcVmError::DivisionByZero => RuntimeError::DivisionByZero(entry_range),
            BcVmError::NumericOverflow => RuntimeError::NumericOverflow(entry_range),
            BcVmError::JumpOutOfRange { .. }
            | BcVmError::IndexOutOfBounds
            | BcVmError::EmptyList
            | BcVmError::InvalidUtf8 => RuntimeError::WasmIndexOutOfBounds { range: entry_range },
            BcVmError::CapabilityDenied { cap_bit } => RuntimeError::WasmCapabilityDenied {
                cap_bit,
                range: entry_range,
            },
            BcVmError::StackUnderflow { bc_idx } => RuntimeError::Unsupported {
                reason: format!("bytecode VM stack underflow at bc_idx {bc_idx}"),
            },
        }
    }
}

/// Outcome of a single bytecode run: either a return value or a
/// trap. The `last_bc_idx` field tells the partial-resume path which
/// bytecode op tripped the trap so it can re-derive the matching IR
/// PC for diagnostics.
#[derive(Debug)]
pub struct BcRunOutcome {
    /// Successful return value, when the run completed via `Return`.
    /// For buffer-protocol entries this is the IR-level `bytes_written`
    /// stand-in; the actual return data lives in `final_locals` at the
    /// schema-defined return-field slot indices.
    pub value: Option<VmValue>,
    /// Trap, when any sandbox prong fired.
    pub error: Option<BcVmError>,
    /// Bytecode index of the last op the VM saw before exit. Maps
    /// back to IR PC via `BcFunction::ir_pc_map[last_bc_idx]`.
    pub last_bc_idx: usize,
    /// Total ops dispatched.
    pub steps: u64,
    /// Local-slot snapshot at exit. The buffer-protocol return
    /// unpacks return-field values from this; the legacy-i64 path
    /// ignores it.
    pub final_locals: Vec<VmValue>,
}

/// Stack-based VM. Stateful across calls (counters reset per
/// [`BytecodeVm::invoke`]).
pub struct BytecodeVm {
    config: BcVmConfig,
}

impl BytecodeVm {
    /// Build a new VM with the supplied config.
    pub fn new(config: BcVmConfig) -> Self {
        Self { config }
    }

    /// Mutable accessor on the active config — used by tests that
    /// flip a single knob (cap grant, max_steps) between invocations.
    pub fn config_mut(&mut self) -> &mut BcVmConfig {
        &mut self.config
    }

    /// Read-only accessor on the active config.
    pub fn config(&self) -> &BcVmConfig {
        &self.config
    }

    /// M2-B phase 2: pre-dispatch consult of the installed
    /// [`CapabilityGate`]. Convenience pass-through to
    /// [`CapabilityVtable::consult_all_granted_bits`] — exposed on the
    /// VM so callers that hold a [`BytecodeVm`] can opt into the
    /// pre-check without reaching into the config. Returns `Ok(())`
    /// when no gate is installed or every granted bit passes.
    pub fn consult_capability_gate(&self) -> Result<(), CapabilityError> {
        self.config.cap_vtable.consult_all_granted_bits()
    }

    /// Invoke `func` with the supplied `args` filling locals
    /// `0..args.len()`. The dispatch loop ticks against
    /// `max_steps`, watches `deadline`, and surfaces trap-prong
    /// failures via [`BcRunOutcome::error`].
    pub fn invoke(&self, func: &BcFunction, args: &[VmValue]) -> BcRunOutcome {
        self.invoke_from(
            func,
            args,
            /*start_bc_idx=*/ 0,
            /*extra_locals=*/ &[],
        )
    }

    /// Invoke `func` starting at a non-zero bytecode index, with the
    /// supplied `extra_locals` overlaid past the args slot. This is
    /// the partial-resume entry point — M2-B will widen it to also
    /// rehydrate the operand stack from the deopt snapshot.
    pub fn invoke_from(
        &self,
        func: &BcFunction,
        args: &[VmValue],
        start_bc_idx: usize,
        extra_locals: &[VmValue],
    ) -> BcRunOutcome {
        self.invoke_from_with_locals(
            func,
            args,
            start_bc_idx,
            extra_locals,
            /*return_slot_count=*/ 0,
        )
    }

    /// Invoke `func` while reserving `return_slot_count` virtual
    /// locals past the args slot for the buffer-protocol epilogue's
    /// `StoreField` lowerings. Returns the full local-slot snapshot
    /// in [`BcRunOutcome::final_locals`] so the evaluator can lift
    /// the return-field values back into a `Value`.
    pub fn invoke_from_with_locals(
        &self,
        func: &BcFunction,
        args: &[VmValue],
        start_bc_idx: usize,
        extra_locals: &[VmValue],
        return_slot_count: u32,
    ) -> BcRunOutcome {
        self.invoke_from_with_stack(
            func,
            args,
            start_bc_idx,
            extra_locals,
            return_slot_count,
            /*initial_stack=*/ &[],
        )
    }

    /// v6-δ M2-B partial-resume entry: same as
    /// [`Self::invoke_from_with_locals`] but pre-seeds the operand
    /// stack with `initial_stack` (bottom-up) before dispatching.
    /// This is the path
    /// [`crate::evaluator::BytecodeEvaluator::resume_from_pc`] uses
    /// to rehydrate mid-expression deopts: the recipe in
    /// `BcFunction::stack_recipe[start_bc_idx]` tells the evaluator
    /// what values to push (locals / consts / snapshot slots), the
    /// evaluator materialises them into a `Vec<u64>`, and the VM
    /// continues dispatch at `start_bc_idx` exactly where the trace
    /// would have been.
    pub fn invoke_from_with_stack(
        &self,
        func: &BcFunction,
        args: &[VmValue],
        start_bc_idx: usize,
        extra_locals: &[VmValue],
        return_slot_count: u32,
        initial_stack: &[VmValue],
    ) -> BcRunOutcome {
        let needed = (func.locals as usize)
            .max(args.len() + return_slot_count as usize + extra_locals.len());
        let mut locals = vec![0u64; needed];
        for (i, v) in args.iter().enumerate() {
            if i < locals.len() {
                locals[i] = *v;
            }
        }
        // Overlay extra locals past the args + return slots. The
        // trace-JIT deopt snapshot stores let-bound slots that the
        // recorder observed; let-locals sit in the bytecode VM past
        // the input + return reservations.
        let overlay_base = args.len() + return_slot_count as usize;
        for (i, v) in extra_locals.iter().enumerate() {
            let idx = overlay_base + i;
            if idx < locals.len() {
                locals[idx] = *v;
            }
        }
        let mut stack: Vec<VmValue> = Vec::with_capacity(16.max(initial_stack.len()));
        stack.extend_from_slice(initial_stack);
        let mut pc = start_bc_idx;
        let mut steps: u64 = 0;
        let mut last_bc_idx = pc;
        let exit = |value: Option<VmValue>,
                    error: Option<BcVmError>,
                    last_bc_idx: usize,
                    steps: u64,
                    locals: Vec<VmValue>|
         -> BcRunOutcome {
            BcRunOutcome {
                value,
                error,
                last_bc_idx,
                steps,
                final_locals: locals,
            }
        };
        // M2-B phase 2 dispatch-time pre-check: if a `CapabilityGate`
        // is installed, every grant-table bit must still pass the
        // gate before the first op runs. Mirrors the cranelift
        // backend's vtable-build-time consult — the bytecode VM
        // doesn't have a vtable-build phase, so the pre-check runs
        // once at invoke entry (cost: one virtual call per granted
        // bit, paid by callers that opted into the gate). The scaffold
        // grant table is empty for the standard `from_source` path so
        // this is a no-op there; hand-built BcFunctions that grant
        // bits get the consult for free.
        //
        // Note: this is enforcement, not advisory — a denial here
        // surfaces as `RuntimeError::WasmCapabilityDenied` exactly
        // like a `BcOp::Trap(CapabilityDenied)` would. Phase 3 IR
        // coverage expansion will widen this into per-call-site
        // consults via the new `BcOp::CheckCap` / `BcOp::CallNative`
        // ops; phase 2 keeps the consult at the dispatch boundary
        // where the existing scaffold ops live.
        if let Err(err) = self.config.cap_vtable.consult_all_granted_bits() {
            return exit(
                None,
                Some(BcVmError::CapabilityDenied {
                    cap_bit: err.cap.bit_index(),
                }),
                last_bc_idx,
                steps,
                locals,
            );
        }
        loop {
            // Resource prong: tick.
            steps += 1;
            if let Some(limit) = self.config.max_steps {
                if steps > limit {
                    return exit(
                        None,
                        Some(BcVmError::StepLimitExceeded { steps }),
                        last_bc_idx,
                        steps,
                        locals,
                    );
                }
            }
            if let Some(d) = self.config.deadline {
                if Instant::now() >= d {
                    return exit(
                        None,
                        Some(BcVmError::DeadlineExceeded),
                        last_bc_idx,
                        steps,
                        locals,
                    );
                }
            }
            if pc >= func.ops.len() {
                return exit(
                    None,
                    Some(BcVmError::JumpOutOfRange {
                        target: pc,
                        ops: func.ops.len(),
                    }),
                    last_bc_idx,
                    steps,
                    locals,
                );
            }
            last_bc_idx = pc;
            match self.dispatch_one(&func.ops[pc], &mut stack, &mut locals, pc) {
                Ok(StepOutcome::Advance) => pc += 1,
                Ok(StepOutcome::Jump(target)) => {
                    if target > func.ops.len() {
                        return exit(
                            None,
                            Some(BcVmError::JumpOutOfRange {
                                target,
                                ops: func.ops.len(),
                            }),
                            last_bc_idx,
                            steps,
                            locals,
                        );
                    }
                    pc = target;
                }
                Ok(StepOutcome::Return(v)) => {
                    return exit(Some(v), None, last_bc_idx, steps, locals);
                }
                Err(e) => {
                    return exit(None, Some(e), last_bc_idx, steps, locals);
                }
            }
        }
    }

    fn dispatch_one(
        &self,
        op: &BcOp,
        stack: &mut Vec<VmValue>,
        locals: &mut [VmValue],
        bc_idx: usize,
    ) -> Result<StepOutcome, BcVmError> {
        match op {
            BcOp::ConstI64(v) => {
                stack.push(*v as u64);
            }
            BcOp::ConstI32(v) => {
                stack.push(*v as u32 as u64);
            }
            BcOp::LocalGet(idx) => {
                let i = *idx as usize;
                if i >= locals.len() {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                stack.push(locals[i]);
            }
            BcOp::LocalSet(idx) => {
                let v = pop(stack, bc_idx)?;
                let i = *idx as usize;
                if i >= locals.len() {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                locals[i] = v;
            }
            BcOp::Add(ty) => arith_binop(stack, *ty, bc_idx, ArithOp::Add)?,
            BcOp::Sub(ty) => arith_binop(stack, *ty, bc_idx, ArithOp::Sub)?,
            BcOp::Mul(ty) => arith_binop(stack, *ty, bc_idx, ArithOp::Mul)?,
            BcOp::Div(ty) => arith_binop(stack, *ty, bc_idx, ArithOp::Div)?,
            BcOp::Mod(ty) => arith_binop(stack, *ty, bc_idx, ArithOp::Mod)?,
            BcOp::Eq(ty) => cmp_binop(stack, *ty, bc_idx, CmpOp::Eq)?,
            BcOp::Ne(ty) => cmp_binop(stack, *ty, bc_idx, CmpOp::Ne)?,
            BcOp::Lt(ty) => cmp_binop(stack, *ty, bc_idx, CmpOp::Lt)?,
            BcOp::Le(ty) => cmp_binop(stack, *ty, bc_idx, CmpOp::Le)?,
            BcOp::Gt(ty) => cmp_binop(stack, *ty, bc_idx, CmpOp::Gt)?,
            BcOp::Ge(ty) => cmp_binop(stack, *ty, bc_idx, CmpOp::Ge)?,
            BcOp::Jump(target) => return Ok(StepOutcome::Jump(*target)),
            BcOp::JumpIfTrue(target) => {
                let cond = pop(stack, bc_idx)? as u32;
                if cond != 0 {
                    return Ok(StepOutcome::Jump(*target));
                }
            }
            BcOp::JumpIfFalse(target) => {
                let cond = pop(stack, bc_idx)? as u32;
                if cond == 0 {
                    return Ok(StepOutcome::Jump(*target));
                }
            }
            BcOp::Return => {
                // The buffer-protocol IR ends `run_main` with
                // `StoreField + Return` — the codegen synthesises a
                // trailing `bytes_written` push, but the bytecode VM
                // skips that since the return-field values are
                // unpacked from the virtual locals slot. Tolerate an
                // empty stack by returning `0`; tests that genuinely
                // depend on the popped value (legacy-i64 direct-IR
                // path) leave a single operand and we lift it
                // through.
                let v = stack.pop().unwrap_or(0);
                return Ok(StepOutcome::Return(v));
            }
            BcOp::Trap(kind) => match kind {
                BcTrapKind::IndexOutOfBounds => return Err(BcVmError::IndexOutOfBounds),
                BcTrapKind::EmptyList => return Err(BcVmError::EmptyList),
                BcTrapKind::InvalidUtf8 => return Err(BcVmError::InvalidUtf8),
                BcTrapKind::CapabilityDenied => {
                    // M2-B phase 2: when a `CapabilityGate` is
                    // installed, consult it for the first denied
                    // grant-table bit so the surfaced `cap_bit`
                    // matches the policy authority rather than the
                    // legacy `u32::MAX` sentinel. The sentinel is
                    // preserved when no gate is installed (legacy
                    // behaviour) so existing tests / hand-built
                    // BcFunctions don't observe a change.
                    //
                    // Phase 3 will replace this BC-level static trap
                    // with `BcOp::CallNative`-driven per-site
                    // consults; phase 2's role is to prove the gate
                    // is reachable from the dispatch path and to
                    // standardise the error envelope.
                    let cap_bit = match self.config.cap_vtable.gate() {
                        Some(gate) => first_denied_bit(gate).unwrap_or(u32::MAX),
                        None => u32::MAX,
                    };
                    return Err(BcVmError::CapabilityDenied { cap_bit });
                }
            },
        }
        Ok(StepOutcome::Advance)
    }
}

enum StepOutcome {
    Advance,
    Jump(usize),
    Return(VmValue),
}

fn pop(stack: &mut Vec<VmValue>, bc_idx: usize) -> Result<VmValue, BcVmError> {
    stack.pop().ok_or(BcVmError::StackUnderflow { bc_idx })
}

#[derive(Clone, Copy)]
enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

fn arith_binop(
    stack: &mut Vec<VmValue>,
    ty: IrType,
    bc_idx: usize,
    op: ArithOp,
) -> Result<(), BcVmError> {
    let rhs = pop(stack, bc_idx)?;
    let lhs = pop(stack, bc_idx)?;
    let out = match ty {
        IrType::I64 | IrType::I32 => {
            let lhs = lhs as i64;
            let rhs = rhs as i64;
            let r = match op {
                ArithOp::Add => lhs.checked_add(rhs).ok_or(BcVmError::NumericOverflow)?,
                ArithOp::Sub => lhs.checked_sub(rhs).ok_or(BcVmError::NumericOverflow)?,
                ArithOp::Mul => lhs.checked_mul(rhs).ok_or(BcVmError::NumericOverflow)?,
                ArithOp::Div => {
                    if rhs == 0 {
                        return Err(BcVmError::DivisionByZero);
                    }
                    // i64::MIN / -1 wraps in two's complement; tree-walker traps.
                    lhs.checked_div(rhs).ok_or(BcVmError::NumericOverflow)?
                }
                ArithOp::Mod => {
                    if rhs == 0 {
                        return Err(BcVmError::DivisionByZero);
                    }
                    lhs.checked_rem(rhs).ok_or(BcVmError::NumericOverflow)?
                }
            };
            r as u64
        }
        IrType::F64 => {
            let lhs = f64::from_bits(lhs);
            let rhs = f64::from_bits(rhs);
            let r = match op {
                ArithOp::Add => lhs + rhs,
                ArithOp::Sub => lhs - rhs,
                ArithOp::Mul => lhs * rhs,
                ArithOp::Div => lhs / rhs,
                ArithOp::Mod => lhs % rhs,
            };
            r.to_bits()
        }
        // Boolean / pointer types reject arith; the compiler should
        // never have lowered them in the first place.
        _ => {
            return Err(BcVmError::StackUnderflow { bc_idx });
        }
    };
    stack.push(out);
    Ok(())
}

#[derive(Clone, Copy)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

fn cmp_binop(
    stack: &mut Vec<VmValue>,
    ty: IrType,
    bc_idx: usize,
    op: CmpOp,
) -> Result<(), BcVmError> {
    let rhs = pop(stack, bc_idx)?;
    let lhs = pop(stack, bc_idx)?;
    let cmp = match ty {
        IrType::I64 | IrType::I32 => {
            let lhs = lhs as i64;
            let rhs = rhs as i64;
            match op {
                CmpOp::Eq => lhs == rhs,
                CmpOp::Ne => lhs != rhs,
                CmpOp::Lt => lhs < rhs,
                CmpOp::Le => lhs <= rhs,
                CmpOp::Gt => lhs > rhs,
                CmpOp::Ge => lhs >= rhs,
            }
        }
        IrType::F64 => {
            let lhs = f64::from_bits(lhs);
            let rhs = f64::from_bits(rhs);
            match op {
                CmpOp::Eq => lhs == rhs,
                CmpOp::Ne => lhs != rhs,
                CmpOp::Lt => lhs < rhs,
                CmpOp::Le => lhs <= rhs,
                CmpOp::Gt => lhs > rhs,
                CmpOp::Ge => lhs >= rhs,
            }
        }
        // Treat the rest as i64 (Bool / Null lifted to i32 via the
        // low 32 bits would still be representable through the i64
        // path, since the compiler only emits Eq/Ne for them).
        _ => {
            let lhs = lhs as i64;
            let rhs = rhs as i64;
            match op {
                CmpOp::Eq => lhs == rhs,
                CmpOp::Ne => lhs != rhs,
                _ => return Err(BcVmError::StackUnderflow { bc_idx }),
            }
        }
    };
    stack.push(if cmp { 1 } else { 0 });
    Ok(())
}

/// Discover the IR PC the VM was sitting on right before it tripped
/// — used by partial-resume and diagnostics. M2-A surface keeps the
/// PC accessible without leaking the bytecode-side index.
pub fn ir_pc_at(func: &BcFunction, bc_idx: usize) -> ExternalPc {
    func.ir_pc_map.get(bc_idx).copied().unwrap_or(0)
}
