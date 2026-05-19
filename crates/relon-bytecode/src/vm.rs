//! Stack-based bytecode VM with 4-prong sandbox engagement.
//!
//! Dispatch is `match`-based — computed-goto would require nightly
//! rust + the unstable `naked_functions` feature, and the M2-A target
//! is not perf (that's M2-C). The single tick-per-op resource counter
//! lives on [`BcVmConfig::max_steps`]; bounds / trap / capability
//! prongs trip through [`BcVmError`] variants that lift cleanly into
//! `relon_eval_api::RuntimeError`.

use std::time::Instant;

use relon_eval_api::RuntimeError;
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
#[derive(Debug, Clone, Default)]
pub struct CapabilityVtable {
    grants: Vec<bool>,
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
                    return Err(BcVmError::CapabilityDenied { cap_bit: u32::MAX })
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
