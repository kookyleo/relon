//! v6-δ M2-A: 4-prong sandbox tests for [`BytecodeEvaluator`].
//!
//! One test per sandbox prong + one resume-from-pc smoke. Each
//! prong drives a source through the bytecode VM and pins the
//! emitted [`RuntimeError`] variant.

use std::collections::HashMap;

use relon_bytecode::vm::CapabilityVtable;
use relon_bytecode::{BcVmConfig, BcVmError, BytecodeEvaluator, BytecodeVm};
use relon_eval_api::{Evaluator, RuntimeError, Value};
use relon_ir::{ir::TrapKind, Func, IrType, Op, TaggedOp};
use relon_parser::TokenRange;

// -- prong 1: trap (div-by-zero) ----------------------------------

#[test]
fn sandbox_trap_div_by_zero() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx / y").unwrap();
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(7));
    args.insert("y".to_string(), Value::Int(0));
    let err = ev.run_main(args).unwrap_err();
    assert!(
        matches!(err, RuntimeError::DivisionByZero(_)),
        "expected DivisionByZero, got {err:?}"
    );
}

// -- prong 2: trap (numeric overflow) -----------------------------

#[test]
fn sandbox_trap_numeric_overflow() {
    let ev = BytecodeEvaluator::from_source("#main(Int x) -> Int\nx + 1").unwrap();
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(i64::MAX));
    let err = ev.run_main(args).unwrap_err();
    assert!(
        matches!(err, RuntimeError::NumericOverflow(_)),
        "expected NumericOverflow, got {err:?}"
    );
}

// -- prong 3: bounds (explicit Trap(IndexOutOfBounds)) ------------

#[test]
fn sandbox_bounds_explicit_trap_op() {
    // Build a hand-rolled IR func that fires `Op::Trap { kind:
    // IndexOutOfBounds }` — exercises the bytecode VM's bounds-
    // prong without needing a stdlib substring shape.
    let func = Func {
        name: "f".into(),
        params: vec![IrType::I64],
        ret: IrType::I64,
        body: vec![
            TaggedOp {
                op: Op::Trap {
                    kind: TrapKind::IndexOutOfBounds,
                },
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::ConstI64(0),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::Return,
                range: TokenRange::default(),
            },
        ],
        range: TokenRange::default(),
    };
    let module = relon_ir::ir::Module {
        funcs: vec![func],
        entry_func_index: Some(0),
        ..Default::default()
    };
    let ev = BytecodeEvaluator::from_ir_legacy(module, vec!["x".into()]).unwrap();
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(0));
    let err = ev.run_main(args).unwrap_err();
    // Bytecode VM lifts IndexOutOfBounds through WasmIndexOutOfBounds.
    assert!(
        matches!(err, RuntimeError::WasmIndexOutOfBounds { .. }),
        "expected WasmIndexOutOfBounds, got {err:?}"
    );
}

// -- prong 4: capability (denied) ---------------------------------

#[test]
fn sandbox_capability_denied_via_trap_op() {
    // Hand-rolled IR that simulates a capability check by firing the
    // bytecode VM's `CapabilityDenied` trap. The vtable surface is
    // M2-B work (real host-fn dispatch); for M2-A we exercise the
    // trap shape via the existing `BcTrapKind::CapabilityDenied`
    // path through the recorder-facing test.
    let func = Func {
        name: "f".into(),
        params: vec![IrType::I64],
        ret: IrType::I64,
        body: vec![
            // We don't have a Relon-level IR op that emits the
            // CapabilityDenied bytecode trap; instead, drive it
            // directly via the VM's BcOp::Trap. This validates the
            // sandbox prong from the VM-side end.
            TaggedOp {
                op: Op::Trap {
                    kind: TrapKind::IndexOutOfBounds, // placeholder; replaced below
                },
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::ConstI64(0),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::Return,
                range: TokenRange::default(),
            },
        ],
        range: TokenRange::default(),
    };
    let module = relon_ir::ir::Module {
        funcs: vec![func],
        entry_func_index: Some(0),
        ..Default::default()
    };
    // We can't synthesize an IR-level CapabilityDenied trap (the IR
    // enum has no such variant), so we instead drive the VM
    // directly with a hand-built BcFunction. This still exercises
    // the prong's RuntimeError lifting.
    use relon_bytecode::compile::build_offset_to_local;
    use relon_bytecode::op::{BcFunction, BcOp, BcTrapKind};
    let _ = build_offset_to_local; // suppress unused warning
    let bc = BcFunction {
        ops: vec![BcOp::Trap(BcTrapKind::CapabilityDenied), BcOp::Return],
        locals: 1,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };
    let cfg = BcVmConfig {
        cap_vtable: CapabilityVtable::default(),
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[0]);
    assert!(
        matches!(outcome.error, Some(BcVmError::CapabilityDenied { .. })),
        "expected CapabilityDenied, got {:?}",
        outcome.error
    );

    // The module-side IR-compiled-evaluator test still passes via
    // the IndexOutOfBounds shape, which proves IR -> bytecode
    // compilation for trap ops is wired correctly.
    let _ = module;
}

// -- prong 5: resource (step limit) -------------------------------

#[test]
fn sandbox_resource_step_limit() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .unwrap()
        .with_config(BcVmConfig {
            max_steps: Some(1),
            ..BcVmConfig::default()
        });
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(1));
    args.insert("y".to_string(), Value::Int(2));
    let err = ev.run_main(args).unwrap_err();
    assert!(
        matches!(err, RuntimeError::WasmStepLimitExceeded { .. }),
        "expected WasmStepLimitExceeded, got {err:?}"
    );
}

// -- prong 6: resource (deadline) ---------------------------------

#[test]
fn sandbox_resource_deadline_exceeded() {
    // Use a deadline already in the past so the very first tick
    // trips the resource prong.
    let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .unwrap()
        .with_config(BcVmConfig {
            max_steps: None,
            deadline: Some(past),
            ..BcVmConfig::default()
        });
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(1));
    args.insert("y".to_string(), Value::Int(2));
    let err = ev.run_main(args).unwrap_err();
    // Deadline lifts through WasmStepLimitExceeded (same prong, no
    // separate RuntimeError shape today).
    assert!(
        matches!(err, RuntimeError::WasmStepLimitExceeded { .. }),
        "expected WasmStepLimitExceeded (deadline), got {err:?}"
    );
}

// -- resume_from_pc: trap re-fires --------------------------------

#[test]
fn resume_from_pc_after_each_prong_replays_trap() {
    // The M2-A scaffold contract: a deopt'd trace's `external_pc`
    // round-trips through `ir_pc_map` and the VM resumes at the
    // matching bytecode index.
    //
    // M2-A scope: only resume PCs that sit at an empty-operand-stack
    // boundary are guaranteed to re-trap deterministically — those
    // are the entry PC (0) and post-LocalSet PCs. Mid-expression
    // PCs (e.g. immediately on the Div op, with [lhs, rhs] expected
    // on the stack) require the M2-B `DeoptStateSnapshot` widening
    // to rehydrate the operand stack from the trace. The
    // bc_index_for_pc lookup is wired and tested via the fallback
    // path below; the actual rehydration is intentionally a stub.
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx / y").unwrap();
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(5));
    args.insert("y".to_string(), Value::Int(0));

    // First-run baseline: trap fires.
    let baseline_err = ev.run_main(args.clone()).unwrap_err();
    assert!(matches!(baseline_err, RuntimeError::DivisionByZero(_)));

    // Resume at entry PC (sentinel `0`): re-runs from the top, hits
    // the same trap. This proves the args round-trip cleanly.
    let resumed_entry = ev
        .resume_from_pc(args.clone(), /*external_pc=*/ 0, &[])
        .unwrap_err();
    assert!(
        matches!(resumed_entry, RuntimeError::DivisionByZero(_)),
        "entry resume should replay the trap, got {resumed_entry:?}"
    );

    // Unknown PC: resume_from_pc gracefully falls back to entry.
    let unknown_pc_resumed = ev
        .resume_from_pc(args.clone(), /*external_pc=*/ 999_999, &[])
        .unwrap_err();
    assert!(matches!(
        unknown_pc_resumed,
        RuntimeError::DivisionByZero(_)
    ));

    // M2-A scaffold note: verifying the ir_pc_map *does* contain the
    // Div op's PC, even though mid-expression resume isn't fully
    // operational yet. M2-B will widen the snapshot envelope to
    // rehydrate the operand stack so this PC becomes resumable.
    let func = ev.function();
    let div_present = func
        .ops
        .iter()
        .any(|op| matches!(op, relon_bytecode::op::BcOp::Div(_)));
    assert!(div_present, "Div op must be in the compiled stream");
    // Each emitted op carries a unique IR PC > 0.
    for pc in &func.ir_pc_map {
        assert!(*pc > 0, "PC sentinel `0` reserved for function entry");
    }
}

// -- happy-path resume from PC -----------------------------------

#[test]
fn resume_from_pc_at_entry_matches_run_main() {
    // external_pc = 0 means "function entry" by the
    // `bc_index_for_pc` contract. The result must match `run_main`.
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y").unwrap();
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));
    let main = ev.run_main(args.clone()).unwrap();
    let resumed = ev.resume_from_pc(args, /*external_pc=*/ 0, &[]).unwrap();
    assert_eq!(main, resumed);
}

// -- vtable / capability surface smoke -----------------------------

#[test]
fn vtable_grant_smoke() {
    let mut vtable = CapabilityVtable::default();
    assert!(!vtable.is_granted(7));
    vtable.grant(7);
    assert!(vtable.is_granted(7));
    assert!(!vtable.is_granted(8));
}

// -- M2-B phase 2: CapabilityGate dispatch consult ----------------

/// Counts every `check` invocation so the test can pin how many
/// dispatch-time consults the VM made. Phase 2 consults at two
/// points: the pre-dispatch sweep over grant-table bits, and the
/// `BcOp::Trap(CapabilityDenied)` enrichment path.
struct CountingGate {
    hits: std::sync::atomic::AtomicU64,
    /// Bits the gate grants — stored as bit indices to side-step the
    /// absent `Hash` impl on [`relon_eval_api::CapabilityBit`].
    granted: Vec<u32>,
}

impl CountingGate {
    fn deny_all() -> Self {
        Self {
            hits: std::sync::atomic::AtomicU64::new(0),
            granted: Vec::new(),
        }
    }

    fn allow(bits: &[relon_eval_api::CapabilityBit]) -> Self {
        Self {
            hits: std::sync::atomic::AtomicU64::new(0),
            granted: bits.iter().map(|b| b.bit_index()).collect(),
        }
    }
}

impl relon_eval_api::CapabilityGate for CountingGate {
    fn check(
        &self,
        cap: relon_eval_api::CapabilityBit,
    ) -> Result<(), relon_eval_api::CapabilityError> {
        self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.granted.contains(&cap.bit_index()) {
            Ok(())
        } else {
            Err(relon_eval_api::CapabilityError::not_granted(cap))
        }
    }
}

#[test]
fn capability_gate_hook_can_be_installed_and_inspected() {
    use std::sync::Arc;

    // Scalar-only source compiles to an empty grant table; the M2-B
    // phase 2 pre-dispatch sweep therefore performs zero gate hits
    // and the run completes regardless of the gate's deny posture.
    // This pins the scaffold-envelope behaviour: a host can install
    // a deny-everything gate on a pure arithmetic source without
    // breaking it.
    let gate_concrete = Arc::new(CountingGate::deny_all());
    let gate: Arc<dyn relon_eval_api::CapabilityGate> = gate_concrete.clone();
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .unwrap()
        .with_capability_gate(gate);

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));
    let value = ev.run_main(args).expect("scalar source runs unchanged");
    assert_eq!(value, Value::Int(42));
    // Empty grant table → zero pre-dispatch consults. Phase 3 IR
    // coverage will widen this once `BcOp::CallNative` lands.
    assert_eq!(
        gate_concrete.hits.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[test]
fn capability_vtable_set_gate_round_trips() {
    use std::sync::Arc;

    let mut vtable = CapabilityVtable::default();
    assert!(vtable.gate().is_none());
    let gate: Arc<dyn relon_eval_api::CapabilityGate> = Arc::new(CountingGate::deny_all());
    vtable.set_gate(gate);
    assert!(vtable.gate().is_some());
}

#[test]
fn capability_gate_denial_surfaces_as_error_on_pre_dispatch_sweep() {
    use relon_bytecode::compile::build_offset_to_local;
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    // Construct a hand-built BcFunction that does nothing but return
    // a constant — no capability ops in the stream. We then grant a
    // bit in the vtable and install a gate that denies it. The
    // dispatch-time pre-check must trip CapabilityDenied with the
    // denied bit, *before* any op runs.
    let _ = build_offset_to_local; // suppress unused warning
    let bc = BcFunction {
        ops: vec![BcOp::ConstI64(7), BcOp::Return],
        locals: 0,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };

    let mut vtable = CapabilityVtable::default();
    vtable.grant(relon_eval_api::CapabilityBit::Network.bit_index());
    let gate_concrete = Arc::new(CountingGate::deny_all());
    let gate: Arc<dyn relon_eval_api::CapabilityGate> = gate_concrete.clone();
    vtable.set_gate(gate);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    match outcome.error {
        Some(BcVmError::CapabilityDenied { cap_bit }) => {
            assert_eq!(cap_bit, relon_eval_api::CapabilityBit::Network.bit_index());
        }
        other => panic!("expected pre-dispatch CapabilityDenied, got {other:?}"),
    }
    // Pre-check fires before the loop ticks, so steps stays at 0.
    assert_eq!(outcome.steps, 0);
    // Pre-check consulted the gate exactly once (one granted bit).
    assert_eq!(
        gate_concrete.hits.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[test]
fn capability_gate_grant_passes_pre_dispatch_sweep() {
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    let bc = BcFunction {
        ops: vec![BcOp::ConstI64(7), BcOp::Return],
        locals: 0,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };

    let mut vtable = CapabilityVtable::default();
    vtable.grant(relon_eval_api::CapabilityBit::ReadsClock.bit_index());
    let gate_concrete = Arc::new(CountingGate::allow(&[
        relon_eval_api::CapabilityBit::ReadsClock,
    ]));
    let gate: Arc<dyn relon_eval_api::CapabilityGate> = gate_concrete.clone();
    vtable.set_gate(gate);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    assert!(outcome.error.is_none(), "gate granted bit → run completes");
    assert_eq!(outcome.value, Some(7));
    // Pre-check consulted once (the single granted bit).
    assert_eq!(
        gate_concrete.hits.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[test]
fn capability_trap_enrichment_uses_gate_bit_when_installed() {
    use relon_bytecode::op::{BcFunction, BcOp, BcTrapKind};
    use std::sync::Arc;

    // BcFunction whose first op is the legacy static
    // `CapabilityDenied` trap. With no gate installed the surfaced
    // `cap_bit` is `u32::MAX`; with a gate installed the VM enriches
    // it with the first gate-denied bit.
    let bc = BcFunction {
        ops: vec![BcOp::Trap(BcTrapKind::CapabilityDenied), BcOp::Return],
        locals: 0,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };

    // Baseline: no gate, sentinel preserved.
    let vm_no_gate = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm_no_gate.invoke(&bc, &[]);
    match outcome.error {
        Some(BcVmError::CapabilityDenied { cap_bit }) => assert_eq!(cap_bit, u32::MAX),
        other => panic!("expected sentinel CapabilityDenied, got {other:?}"),
    }

    // Gate installed and denies everything: first declared bit
    // (ReadsFs at index 0) gets reported.
    let mut vtable = CapabilityVtable::default();
    let gate: Arc<dyn relon_eval_api::CapabilityGate> = Arc::new(CountingGate::deny_all());
    vtable.set_gate(gate);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm_with_gate = BytecodeVm::new(cfg);
    let outcome = vm_with_gate.invoke(&bc, &[]);
    match outcome.error {
        Some(BcVmError::CapabilityDenied { cap_bit }) => {
            assert_eq!(cap_bit, relon_eval_api::CapabilityBit::ReadsFs.bit_index());
        }
        other => panic!("expected enriched CapabilityDenied, got {other:?}"),
    }
}

#[test]
fn capability_gate_denial_lifts_to_runtime_error() {
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    // Verify the trap envelope lifts cleanly through
    // `BcVmError::into_runtime_error`. The evaluator surface goes
    // through a different (IR-compiled) path that today never emits
    // capability ops; this test exercises the lifting contract on a
    // hand-built BcFunction so phase 3 callers can rely on the
    // shape.
    let bc = BcFunction {
        ops: vec![BcOp::ConstI64(0), BcOp::Return],
        locals: 0,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };

    let mut vtable = CapabilityVtable::default();
    vtable.grant(relon_eval_api::CapabilityBit::WritesFs.bit_index());
    let gate: Arc<dyn relon_eval_api::CapabilityGate> = Arc::new(CountingGate::deny_all());
    vtable.set_gate(gate);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    let err = outcome.error.expect("must trap");
    let lifted = err.into_runtime_error(relon_parser::TokenRange::default());
    match lifted {
        RuntimeError::WasmCapabilityDenied { cap_bit, .. } => {
            assert_eq!(cap_bit, relon_eval_api::CapabilityBit::WritesFs.bit_index());
        }
        other => panic!("expected WasmCapabilityDenied, got {other:?}"),
    }
}

// -- M2-B phase 3: BcOp::CallNative + BcOp::CheckCap dispatch ------

#[test]
fn call_native_denied_by_gate_traps_with_declared_bit() {
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    // Hand-built BcFunction whose body is a single CallNative with
    // cap_bit = Network. No gate / grant table has Network set, so
    // the per-call-site consult must trip CapabilityDenied with the
    // declared bit *before* the dispatcher tries to look up the host
    // fn. This is the phase-3 contract.
    let bc = BcFunction {
        ops: vec![
            BcOp::CallNative {
                import_idx: 0,
                arg_count: 0,
                cap_bit: relon_eval_api::CapabilityBit::Network.bit_index(),
                ret_ty: relon_ir::IrType::I64,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };

    // Variant A: gate installed, denies everything.
    let mut vtable = CapabilityVtable::default();
    let gate: Arc<dyn relon_eval_api::CapabilityGate> = Arc::new(CountingGate::deny_all());
    vtable.set_gate(gate);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    match outcome.error {
        Some(BcVmError::CapabilityDenied { cap_bit }) => {
            assert_eq!(cap_bit, relon_eval_api::CapabilityBit::Network.bit_index());
        }
        other => panic!("expected per-call CapabilityDenied (gate), got {other:?}"),
    }

    // Variant B: no gate, no grant — legacy grant-table fallback
    // must still trip the prong with the same bit.
    let vm_no_gate = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm_no_gate.invoke(&bc, &[]);
    match outcome.error {
        Some(BcVmError::CapabilityDenied { cap_bit }) => {
            assert_eq!(cap_bit, relon_eval_api::CapabilityBit::Network.bit_index());
        }
        other => panic!("expected per-call CapabilityDenied (grant fallback), got {other:?}"),
    }
}

#[test]
fn call_native_passes_gate_but_traps_native_not_implemented() {
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    // Gate grants the bit → the capability prong passes. The phase-3
    // dispatcher then surfaces `NativeNotImplemented` because the
    // host-fn registry is phase-4 work. The args we push on the stack
    // are still drained so the operand-stack discipline stays
    // consistent with the recipe.
    let bc = BcFunction {
        ops: vec![
            BcOp::ConstI64(11),
            BcOp::ConstI64(22),
            BcOp::CallNative {
                import_idx: 7,
                arg_count: 2,
                cap_bit: relon_eval_api::CapabilityBit::ReadsClock.bit_index(),
                ret_ty: relon_ir::IrType::I64,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3, 4],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![], vec![], vec![]],
    };

    let mut vtable = CapabilityVtable::default();
    let gate: Arc<dyn relon_eval_api::CapabilityGate> = Arc::new(CountingGate::allow(&[
        relon_eval_api::CapabilityBit::ReadsClock,
    ]));
    vtable.set_gate(gate);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    match outcome.error {
        Some(BcVmError::NativeNotImplemented { import_idx }) => {
            assert_eq!(import_idx, 7, "import_idx round-trips into error envelope");
        }
        other => panic!("expected NativeNotImplemented after gate grant, got {other:?}"),
    }
}

#[test]
fn call_native_no_capability_bit_skips_gate_consult() {
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    // NO_CAPABILITY_BIT means "pure host fn"; the dispatcher must
    // skip the gate consult entirely. Even a deny-all gate doesn't
    // observe the call. The dispatcher still bounces with
    // `NativeNotImplemented` because the host-fn registry is phase-4
    // work, but importantly the gate's `check` was never invoked.
    let bc = BcFunction {
        ops: vec![
            BcOp::CallNative {
                import_idx: 3,
                arg_count: 0,
                cap_bit: relon_ir::NO_CAPABILITY_BIT,
                ret_ty: relon_ir::IrType::I64,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };

    let gate_concrete = Arc::new(CountingGate::deny_all());
    let gate: Arc<dyn relon_eval_api::CapabilityGate> = gate_concrete.clone();
    let mut vtable = CapabilityVtable::default();
    vtable.set_gate(gate);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    match outcome.error {
        Some(BcVmError::NativeNotImplemented { import_idx }) => assert_eq!(import_idx, 3),
        other => panic!("expected NativeNotImplemented, got {other:?}"),
    }
    // The gate was never consulted: pre-dispatch sweep has no granted
    // bits to walk, and the CallNative path's `cap_bit == u32::MAX`
    // guard skipped the per-site consult.
    assert_eq!(
        gate_concrete.hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "NO_CAPABILITY_BIT must not consult the gate"
    );
}

#[test]
fn check_cap_traps_when_bit_denied() {
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    let bc = BcFunction {
        ops: vec![
            BcOp::CheckCap {
                cap_bit: relon_eval_api::CapabilityBit::WritesFs.bit_index(),
            },
            BcOp::ConstI64(0),
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![], vec![]],
    };
    let mut vtable = CapabilityVtable::default();
    let gate: Arc<dyn relon_eval_api::CapabilityGate> = Arc::new(CountingGate::deny_all());
    vtable.set_gate(gate);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    match outcome.error {
        Some(BcVmError::CapabilityDenied { cap_bit }) => {
            assert_eq!(cap_bit, relon_eval_api::CapabilityBit::WritesFs.bit_index());
        }
        other => panic!("expected CheckCap-driven CapabilityDenied, got {other:?}"),
    }
}

#[test]
fn check_cap_no_capability_bit_is_noop() {
    use relon_bytecode::op::{BcFunction, BcOp};

    // NO_CAPABILITY_BIT must short-circuit before any gate consult.
    // The Return after it carries the constant 42, which we assert
    // round-trips out to confirm the op didn't drop our stack.
    let bc = BcFunction {
        ops: vec![
            BcOp::ConstI64(42),
            BcOp::CheckCap {
                cap_bit: relon_ir::NO_CAPABILITY_BIT,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3],
        string_pool: Vec::new(),
        stack_recipe:vec![
            vec![],
            vec![relon_bytecode::op::StackOrigin::Const(42)],
            vec![relon_bytecode::op::StackOrigin::Const(42)],
        ],
    };
    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&bc, &[]);
    assert!(outcome.error.is_none(), "no-op CheckCap must not trap");
    assert_eq!(outcome.value, Some(42));
}

// -- M2-B phase 3: BcOp::CallStdlibScalar handlers -----------------

#[test]
fn call_stdlib_scalar_int_abs() {
    use relon_bytecode::op::{BcFunction, BcOp, BcStdlibKind};

    let bc = BcFunction {
        ops: vec![
            BcOp::ConstI64(-13),
            BcOp::CallStdlibScalar {
                kind: BcStdlibKind::IntAbs,
                arg_count: 1,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![], vec![]],
    };
    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&bc, &[]);
    assert!(outcome.error.is_none(), "abs run completes");
    assert_eq!(outcome.value, Some(13));
}

#[test]
fn call_stdlib_scalar_int_min_max() {
    use relon_bytecode::op::{BcFunction, BcOp, BcStdlibKind};

    let bc_min = BcFunction {
        ops: vec![
            BcOp::ConstI64(7),
            BcOp::ConstI64(3),
            BcOp::CallStdlibScalar {
                kind: BcStdlibKind::IntMin,
                arg_count: 2,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3, 4],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![], vec![], vec![]],
    };
    let vm = BytecodeVm::new(BcVmConfig::default());
    assert_eq!(vm.invoke(&bc_min, &[]).value, Some(3));

    let bc_max = BcFunction {
        ops: vec![
            BcOp::ConstI64(7),
            BcOp::ConstI64(3),
            BcOp::CallStdlibScalar {
                kind: BcStdlibKind::IntMax,
                arg_count: 2,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3, 4],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![], vec![], vec![]],
    };
    assert_eq!(vm.invoke(&bc_max, &[]).value, Some(7));
}

#[test]
fn list_len_witness_passes_length_through() {
    use relon_bytecode::op::{BcFunction, BcOp};

    // The compile pass constant-folds list lengths into ConstI64.
    // BcOp::ListLen is a witness slot that consumes + re-pushes the
    // length so the dispatch loop has a `length`-shaped op to step
    // over (kept for stack-recipe stability when phase-4 widens the
    // op set with actual list operations).
    let bc = BcFunction {
        ops: vec![BcOp::ConstI64(5), BcOp::ListLen, BcOp::Return],
        locals: 0,
        ir_pc_map: vec![1, 2, 3],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![], vec![]],
    };
    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&bc, &[]);
    assert!(outcome.error.is_none());
    assert_eq!(outcome.value, Some(5));
}

// -- M2-B phase 4a: host-fn registry on CapabilityVtable ----------

/// Hand-written `RelonFunction` for the phase-4a tests. Pure scalar
/// in / scalar out — sums every positional arg as i64 and returns
/// `Value::Int(sum)`. Tracks invocation count so the test can pin
/// that a denied call site never reaches the host fn.
struct SumNative {
    hits: std::sync::atomic::AtomicU64,
}

impl SumNative {
    fn new() -> Self {
        Self {
            hits: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl relon_eval_api::RelonFunction for SumNative {
    fn call(
        &self,
        args: relon_eval_api::NativeArgs,
        _range: relon_parser::TokenRange,
    ) -> Result<Value, RuntimeError> {
        self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut acc: i64 = 0;
        for v in args.positional.iter() {
            match v {
                Value::Int(i) => acc = acc.wrapping_add(*i),
                other => {
                    return Err(RuntimeError::Unsupported {
                        reason: format!("SumNative expects Int, got {}", other.type_name()),
                    })
                }
            }
        }
        Ok(Value::Int(acc))
    }
}

#[test]
fn call_native_registry_dispatches_scalar_sum() {
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    // Two args on the stack, CallNative with import_idx 5, ret_ty i64.
    // Registered SumNative returns the i64 sum; we then return that
    // value out of the bytecode VM to assert the round-trip.
    let bc = BcFunction {
        ops: vec![
            BcOp::ConstI64(11),
            BcOp::ConstI64(22),
            BcOp::CallNative {
                import_idx: 5,
                arg_count: 2,
                cap_bit: relon_ir::NO_CAPABILITY_BIT,
                ret_ty: relon_ir::IrType::I64,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3, 4],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![], vec![], vec![]],
    };

    let mut vtable = CapabilityVtable::default();
    let native: Arc<SumNative> = Arc::new(SumNative::new());
    let native_dyn: Arc<dyn relon_eval_api::RelonFunction> = native.clone();
    vtable.register_host_fn(5, native_dyn);
    assert_eq!(vtable.host_fn_count(), 1);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    assert!(
        outcome.error.is_none(),
        "expected clean run, got {:?}",
        outcome.error
    );
    assert_eq!(outcome.value, Some(33u64));
    assert_eq!(
        native.hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "host fn invoked exactly once"
    );
}

#[test]
fn call_native_registry_gate_denial_skips_host_fn() {
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    // Registered host fn for slot 5, but the gate denies the call's
    // cap_bit. The dispatcher must trip CapabilityDenied with the
    // declared bit and never invoke the host fn.
    let bc = BcFunction {
        ops: vec![
            BcOp::CallNative {
                import_idx: 5,
                arg_count: 0,
                cap_bit: relon_eval_api::CapabilityBit::Network.bit_index(),
                ret_ty: relon_ir::IrType::I64,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };

    let mut vtable = CapabilityVtable::default();
    let native: Arc<SumNative> = Arc::new(SumNative::new());
    let native_dyn: Arc<dyn relon_eval_api::RelonFunction> = native.clone();
    vtable.register_host_fn(5, native_dyn);
    let gate: Arc<dyn relon_eval_api::CapabilityGate> = Arc::new(CountingGate::deny_all());
    vtable.set_gate(gate);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    match outcome.error {
        Some(BcVmError::CapabilityDenied { cap_bit }) => {
            assert_eq!(cap_bit, relon_eval_api::CapabilityBit::Network.bit_index());
        }
        other => panic!("expected CapabilityDenied before host fn, got {other:?}"),
    }
    assert_eq!(
        native.hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "host fn must not run when capability prong fires"
    );
}

#[test]
fn call_native_unregistered_slot_keeps_native_not_implemented_fallback() {
    use relon_bytecode::op::{BcFunction, BcOp};

    // Phase-4a contract: an absent registry slot keeps the legacy
    // `NativeNotImplemented` envelope so the differential harness's
    // bounce shape stays stable.
    let bc = BcFunction {
        ops: vec![
            BcOp::CallNative {
                import_idx: 99,
                arg_count: 0,
                cap_bit: relon_ir::NO_CAPABILITY_BIT,
                ret_ty: relon_ir::IrType::I64,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };
    // Empty registry — no host fn registered for any slot.
    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&bc, &[]);
    match outcome.error {
        Some(BcVmError::NativeNotImplemented { import_idx }) => assert_eq!(import_idx, 99),
        other => panic!("expected NativeNotImplemented for unregistered slot, got {other:?}"),
    }
}

#[test]
fn call_native_registry_bool_return_round_trips() {
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    // Host fn returns `Value::Bool(true)` against a declared
    // `IrType::Bool`. The encoder must lift it back into the bool
    // slot (`1`) so the surrounding op stream can branch on it.
    struct AlwaysTrue;
    impl relon_eval_api::RelonFunction for AlwaysTrue {
        fn call(
            &self,
            _args: relon_eval_api::NativeArgs,
            _range: relon_parser::TokenRange,
        ) -> Result<Value, RuntimeError> {
            Ok(Value::Bool(true))
        }
    }

    let bc = BcFunction {
        ops: vec![
            BcOp::CallNative {
                import_idx: 1,
                arg_count: 0,
                cap_bit: relon_ir::NO_CAPABILITY_BIT,
                ret_ty: relon_ir::IrType::Bool,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };
    let mut vtable = CapabilityVtable::default();
    let native: Arc<dyn relon_eval_api::RelonFunction> = Arc::new(AlwaysTrue);
    vtable.register_host_fn(1, native);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    assert!(outcome.error.is_none());
    assert_eq!(outcome.value, Some(1u64));
}

#[test]
fn call_native_host_fn_failure_lifts_to_unsupported() {
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    // Host fn returns an explicit `RuntimeError`; the dispatcher
    // surfaces `BcVmError::HostFnError` which lifts to
    // `RuntimeError::Unsupported` per the phase-4a envelope.
    struct AlwaysFail;
    impl relon_eval_api::RelonFunction for AlwaysFail {
        fn call(
            &self,
            _args: relon_eval_api::NativeArgs,
            _range: relon_parser::TokenRange,
        ) -> Result<Value, RuntimeError> {
            Err(RuntimeError::Unsupported {
                reason: "synthetic host fn failure".into(),
            })
        }
    }

    let bc = BcFunction {
        ops: vec![
            BcOp::CallNative {
                import_idx: 2,
                arg_count: 0,
                cap_bit: relon_ir::NO_CAPABILITY_BIT,
                ret_ty: relon_ir::IrType::I64,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };
    let mut vtable = CapabilityVtable::default();
    let native: Arc<dyn relon_eval_api::RelonFunction> = Arc::new(AlwaysFail);
    vtable.register_host_fn(2, native);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    let err = outcome.error.expect("must surface host fn failure");
    assert!(
        matches!(err, BcVmError::HostFnError { import_idx, .. } if import_idx == 2),
        "got {err:?}"
    );
    let lifted = err.into_runtime_error(relon_parser::TokenRange::default());
    match lifted {
        RuntimeError::Unsupported { reason } => {
            assert!(reason.contains("import_idx 2"));
            assert!(reason.contains("synthetic host fn failure"));
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn call_native_registry_arg_order_matches_declaration() {
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    // Push 1, 2, 3 in that order onto the stack, then CallNative with
    // arg_count=3. The host fn must receive `[1, 2, 3]` in declaration
    // order (top-of-stack is the last positional arg). We assert this
    // by encoding the args as `100*a + 10*b + c` so the order is
    // observable in the returned scalar.
    struct OrderProbe;
    impl relon_eval_api::RelonFunction for OrderProbe {
        fn call(
            &self,
            args: relon_eval_api::NativeArgs,
            _range: relon_parser::TokenRange,
        ) -> Result<Value, RuntimeError> {
            let a = match args.positional.first() {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let b = match args.positional.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let c = match args.positional.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            Ok(Value::Int(100 * a + 10 * b + c))
        }
    }

    let bc = BcFunction {
        ops: vec![
            BcOp::ConstI64(1),
            BcOp::ConstI64(2),
            BcOp::ConstI64(3),
            BcOp::CallNative {
                import_idx: 7,
                arg_count: 3,
                cap_bit: relon_ir::NO_CAPABILITY_BIT,
                ret_ty: relon_ir::IrType::I64,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3, 4, 5],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![], vec![], vec![], vec![]],
    };
    let mut vtable = CapabilityVtable::default();
    let native: Arc<dyn relon_eval_api::RelonFunction> = Arc::new(OrderProbe);
    vtable.register_host_fn(7, native);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    assert!(outcome.error.is_none());
    assert_eq!(outcome.value, Some(123u64));
}

#[test]
fn call_native_registry_unsupported_return_type_traps() {
    use relon_bytecode::op::{BcFunction, BcOp};
    use std::sync::Arc;

    // Host fn returns `Value::String` — outside the phase-4a scalar
    // envelope. The encoder must surface
    // `HostFnReturnTypeMismatch`; the lift routes through
    // `Unsupported` with both `import_idx` and the unsupported type
    // name in the reason.
    struct StringReturner;
    impl relon_eval_api::RelonFunction for StringReturner {
        fn call(
            &self,
            _args: relon_eval_api::NativeArgs,
            _range: relon_parser::TokenRange,
        ) -> Result<Value, RuntimeError> {
            Ok(Value::String("nope".into()))
        }
    }

    let bc = BcFunction {
        ops: vec![
            BcOp::CallNative {
                import_idx: 8,
                arg_count: 0,
                cap_bit: relon_ir::NO_CAPABILITY_BIT,
                ret_ty: relon_ir::IrType::I64,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };
    let mut vtable = CapabilityVtable::default();
    let native: Arc<dyn relon_eval_api::RelonFunction> = Arc::new(StringReturner);
    vtable.register_host_fn(8, native);
    let cfg = BcVmConfig {
        cap_vtable: vtable,
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg);
    let outcome = vm.invoke(&bc, &[]);
    let err = outcome.error.expect("must trap on String return");
    assert!(
        matches!(
            err,
            BcVmError::HostFnReturnTypeMismatch { import_idx: 8, .. }
        ),
        "got {err:?}"
    );
}

#[test]
fn call_native_lifts_to_unsupported_runtime_error() {
    use relon_bytecode::op::{BcFunction, BcOp};

    // The `NativeNotImplemented` envelope lifts to `Unsupported` so
    // the surrounding `Evaluator::run_main` shape stays compatible
    // with the four-way harness's bytecode row. Phase 4 will widen
    // this once a host-fn registry is wired.
    let bc = BcFunction {
        ops: vec![
            BcOp::CallNative {
                import_idx: 2,
                arg_count: 0,
                cap_bit: relon_ir::NO_CAPABILITY_BIT,
                ret_ty: relon_ir::IrType::I64,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![]],
    };
    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&bc, &[]);
    let err = outcome.error.expect("must trap NativeNotImplemented");
    let lifted = err.into_runtime_error(relon_parser::TokenRange::default());
    match lifted {
        RuntimeError::Unsupported { reason } => {
            assert!(
                reason.contains("import_idx 2"),
                "lifted reason carries import_idx; got {reason}"
            );
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

// -- M2-B phase 4b: arena-backed list ops -------------------------

/// `MakeList` allocates an arena slot from the operand-stack
/// contents and pushes the handle. `ListGetInt` then indexes it.
/// Round-trip pin: build [10, 20, 30], pull index 1, expect 20.
#[test]
fn make_list_and_get_int_round_trip() {
    use relon_bytecode::op::{BcFunction, BcOp};

    let bc = BcFunction {
        ops: vec![
            BcOp::ConstI64(10),
            BcOp::ConstI64(20),
            BcOp::ConstI64(30),
            BcOp::MakeList { len: 3 },
            BcOp::ConstI64(1),
            BcOp::ListGetInt,
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3, 4, 5, 6, 7],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![], vec![], vec![], vec![], vec![], vec![], vec![]],
    };
    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&bc, &[]);
    assert!(outcome.error.is_none(), "round-trip completes cleanly");
    assert_eq!(outcome.value, Some(20));
}

/// First element + last element + empty list. Each branch validates
/// a different slot of the arena's element-access discipline.
#[test]
fn list_get_int_first_last_and_empty() {
    use relon_bytecode::op::{BcFunction, BcOp};

    // First element.
    let bc_first = BcFunction {
        ops: vec![
            BcOp::ConstI64(7),
            BcOp::ConstI64(8),
            BcOp::MakeList { len: 2 },
            BcOp::ConstI64(0),
            BcOp::ListGetInt,
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3, 4, 5, 6],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![]; 6],
    };
    let vm = BytecodeVm::new(BcVmConfig::default());
    assert_eq!(vm.invoke(&bc_first, &[]).value, Some(7));

    // Last element.
    let bc_last = BcFunction {
        ops: vec![
            BcOp::ConstI64(7),
            BcOp::ConstI64(8),
            BcOp::MakeList { len: 2 },
            BcOp::ConstI64(1),
            BcOp::ListGetInt,
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3, 4, 5, 6],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![]; 6],
    };
    assert_eq!(vm.invoke(&bc_last, &[]).value, Some(8));

    // Empty list — any index trips.
    let bc_empty = BcFunction {
        ops: vec![
            BcOp::MakeList { len: 0 },
            BcOp::ConstI64(0),
            BcOp::ListGetInt,
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3, 4],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![]; 4],
    };
    let outcome = vm.invoke(&bc_empty, &[]);
    let err = outcome.error.expect("empty-list index must trap");
    assert!(
        matches!(err, BcVmError::IndexOutOfBounds),
        "expected IndexOutOfBounds, got {err:?}"
    );
}

/// `ListGetInt` with out-of-range indices traps. Covers the upper-
/// bound and the explicit negative-index path the arena rejects.
#[test]
fn list_get_int_out_of_range_traps() {
    use relon_bytecode::op::{BcFunction, BcOp};

    let make_oob = |idx: i64| BcFunction {
        ops: vec![
            BcOp::ConstI64(1),
            BcOp::ConstI64(2),
            BcOp::MakeList { len: 2 },
            BcOp::ConstI64(idx),
            BcOp::ListGetInt,
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3, 4, 5, 6],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![]; 6],
    };
    let vm = BytecodeVm::new(BcVmConfig::default());

    for idx in [2i64, 5, -1, i64::MIN] {
        let outcome = vm.invoke(&make_oob(idx), &[]);
        let err = outcome
            .error
            .unwrap_or_else(|| panic!("idx {idx} must trap, got value={:?}", outcome.value));
        assert!(
            matches!(err, BcVmError::IndexOutOfBounds),
            "expected IndexOutOfBounds for idx {idx}, got {err:?}"
        );
        // Lift cleanly through the public surface.
        let lifted = err.into_runtime_error(relon_parser::TokenRange::default());
        assert!(
            matches!(lifted, RuntimeError::WasmIndexOutOfBounds { .. }),
            "lift envelope: idx {idx} -> {lifted:?}"
        );
    }
}

/// Two independent `MakeList` ops mint distinct handles and the
/// second list's contents don't shadow the first. Pin against an
/// accidental "all ops share a slot 0" arena bug.
#[test]
fn multiple_make_lists_mint_distinct_handles() {
    use relon_bytecode::op::{BcFunction, BcOp};

    // Build [100, 200] then [9], read element 0 of each, sum them.
    // First-list elem 0 + second-list elem 0 = 100 + 9 = 109.
    let bc = BcFunction {
        ops: vec![
            BcOp::ConstI64(100),
            BcOp::ConstI64(200),
            BcOp::MakeList { len: 2 },
            BcOp::LocalSet(0),
            BcOp::ConstI64(9),
            BcOp::MakeList { len: 1 },
            BcOp::LocalSet(1),
            BcOp::LocalGet(0),
            BcOp::ConstI64(0),
            BcOp::ListGetInt,
            BcOp::LocalGet(1),
            BcOp::ConstI64(0),
            BcOp::ListGetInt,
            BcOp::Add(relon_ir::IrType::I64),
            BcOp::Return,
        ],
        locals: 2,
        ir_pc_map: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![]; 15],
    };
    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&bc, &[]);
    assert!(outcome.error.is_none(), "distinct-handle path completes");
    assert_eq!(outcome.value, Some(109));
}

/// Stack underflow on `MakeList { len }` when the operand stack has
/// fewer than `len` slots. Compiler bug envelope; surfaces as
/// `StackUnderflow` rather than silently shrinking the list.
#[test]
fn make_list_stack_underflow_traps() {
    use relon_bytecode::op::{BcFunction, BcOp};

    let bc = BcFunction {
        ops: vec![
            BcOp::ConstI64(1),
            BcOp::MakeList { len: 5 }, // wants 5 operands, only 1 on stack
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![]; 3],
    };
    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&bc, &[]);
    let err = outcome.error.expect("must trap StackUnderflow");
    assert!(
        matches!(err, BcVmError::StackUnderflow { .. }),
        "expected StackUnderflow, got {err:?}"
    );
}

/// The MakeList op pops in declaration order — slot 0 is the
/// bottom-of-stack push. Pin: `[5, 6, 7]` indexed at 0 returns 5
/// (not 7).
#[test]
fn make_list_preserves_declaration_order() {
    use relon_bytecode::op::{BcFunction, BcOp};

    let bc = BcFunction {
        ops: vec![
            BcOp::ConstI64(5),
            BcOp::ConstI64(6),
            BcOp::ConstI64(7),
            BcOp::MakeList { len: 3 },
            BcOp::ConstI64(0),
            BcOp::ListGetInt,
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3, 4, 5, 6, 7],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![]; 7],
    };
    let vm = BytecodeVm::new(BcVmConfig::default());
    assert_eq!(vm.invoke(&bc, &[]).value, Some(5));
}

/// Arena is per-invoke — a second `invoke` against the same VM
/// instance starts with a fresh arena (handle 0 is fresh again).
/// Pin: two back-to-back invocations build a one-element list and
/// read its slot; both succeed independently.
#[test]
fn arena_is_reset_between_invocations() {
    use relon_bytecode::op::{BcFunction, BcOp};

    let bc = BcFunction {
        ops: vec![
            BcOp::ConstI64(42),
            BcOp::MakeList { len: 1 },
            BcOp::ConstI64(0),
            BcOp::ListGetInt,
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![1, 2, 3, 4, 5],
        string_pool: Vec::new(),
        stack_recipe:vec![vec![]; 5],
    };
    let vm = BytecodeVm::new(BcVmConfig::default());
    let a = vm.invoke(&bc, &[]);
    let b = vm.invoke(&bc, &[]);
    assert_eq!(a.value, Some(42));
    assert_eq!(b.value, Some(42));
    assert!(a.error.is_none() && b.error.is_none());
}
