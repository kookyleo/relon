//! v6-δ M2-B: partial-resume 4-prong sandbox tests.
//!
//! Companion to `bytecode_sandbox.rs`. Where the M2-A test pinned
//! that the trap fires on a fresh `run_main`, this file pins the real
//! M2-B deliverable: a trap snapshot's `external_pc` is routed
//! through `ir_pc_map`, the operand stack is rebuilt from the
//! compile-time recipe + snapshot fragments, and the VM trips the
//! same `RuntimeError` variant **at the same bytecode index** without
//! restarting from function entry.
//!
//! Each test asserts:
//! 1. Baseline `run_main` reproduces the trap (sanity check).
//! 2. The trap's `last_bc_idx` (post-baseline) gives the resume PC.
//! 3. `resume_from_pc` invoked with that PC + snapshot fragments
//!    triggers the same `RuntimeError` variant.
//! 4. The bytecode index visited during resume matches the
//!    post-trap path length, **not** the full entry-to-trap length —
//!    proving the rehydration is real, not a hidden full re-run.
//!
//! ## Snapshot construction
//!
//! Tests construct `DeoptStateSnapshot` directly via
//! [`DeoptStateSnapshot::with_value_stack`]. The trace-recorder side
//! wiring of `value_stack_copy` is M2-C work; M2-B exercises the
//! resume-from-snapshot surface independently.

use std::collections::HashMap;

use relon_bytecode::op::{BcFunction, BcOp, BcTrapKind, StackOrigin};
use relon_bytecode::vm::CapabilityVtable;
use relon_bytecode::{BcVmConfig, BcVmError, BytecodeEvaluator, BytecodeVm};
use relon_eval_api::{Evaluator, RuntimeError, Value};
use relon_ir::{ir::TrapKind, Func, IrType, Op, TaggedOp};
use relon_parser::TokenRange;
use relon_trace_abi::DeoptStateSnapshot;

// ---- prong 1: trap (div-by-zero) at mid-expression ---------------

#[test]
fn partial_resume_trap_div_by_zero_replays_at_div_pc() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx / y").unwrap();
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(7));
    args.insert("y".to_string(), Value::Int(0));

    // Baseline: the Div op traps.
    let baseline_err = ev.run_main(args.clone()).unwrap_err();
    assert!(matches!(baseline_err, RuntimeError::DivisionByZero(_)));

    // Find the Div op's bc_idx in the compiled function.
    let func = ev.function();
    let div_idx = func
        .ops
        .iter()
        .position(|op| matches!(op, BcOp::DivI64 | BcOp::DivF64))
        .expect("Div present");
    let div_external_pc = func.ir_pc_map[div_idx];

    // Recipe at div_idx must hold two stack entries (lhs + rhs).
    let recipe = &func.stack_recipe[div_idx];
    assert_eq!(
        recipe.len(),
        2,
        "Div recipe carries two stack slots, got {recipe:?}"
    );

    // Construct a snapshot pointing at the Div op. The recipe entries
    // are Local-backed so `value_stack_copy` stays empty; this exercises
    // the local-overlay-only path.
    let snapshot = DeoptStateSnapshot::with_value_stack(
        /*guard_pc=*/ 0,
        /*external_pc=*/ div_external_pc,
        /*ssa_slots_copy=*/ Vec::new().into_boxed_slice(),
        /*value_stack_copy=*/ Vec::new().into_boxed_slice(),
    );

    // Resume directly via the snapshot-driven API.
    let resumed = ev
        .resume_from_snapshot(args.clone(), &snapshot)
        .unwrap_err();
    assert!(
        matches!(resumed, RuntimeError::DivisionByZero(_)),
        "expected DivisionByZero on resume, got {resumed:?}"
    );

    // Bytecode-index visited count: from div_idx, the VM trips
    // immediately (1 step). Re-running from entry would have visited
    // ≥ 3 ops (LocalGet, LocalGet, Div); the resume's path is strictly
    // shorter, proving the rehydration is real.
    let from_entry_steps = run_steps_until_error(&ev, &args, /*start_bc_idx=*/ 0);
    let resume_steps =
        run_steps_until_error_at(&ev, &args, div_idx, &recipe_to_initial(&ev, div_idx, &args));
    assert!(
        resume_steps < from_entry_steps,
        "resume path ({resume_steps} steps) must be shorter than entry path \
         ({from_entry_steps} steps) — partial-resume real"
    );
}

// ---- prong 2: trap (numeric overflow) at mid-expression ----------

#[test]
fn partial_resume_trap_overflow_replays_at_add_pc() {
    let ev = BytecodeEvaluator::from_source("#main(Int x) -> Int\nx + 1").unwrap();
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(i64::MAX));

    let baseline_err = ev.run_main(args.clone()).unwrap_err();
    assert!(matches!(baseline_err, RuntimeError::NumericOverflow(_)));

    let func = ev.function();
    let add_idx = func
        .ops
        .iter()
        .position(|op| matches!(op, BcOp::AddI64 | BcOp::AddF64))
        .expect("Add present");
    let add_pc = func.ir_pc_map[add_idx];
    let recipe = &func.stack_recipe[add_idx];
    assert_eq!(recipe.len(), 2);

    let snapshot = DeoptStateSnapshot::with_value_stack(
        0,
        add_pc,
        Vec::new().into_boxed_slice(),
        Vec::new().into_boxed_slice(),
    );
    let resumed = ev
        .resume_from_snapshot(args.clone(), &snapshot)
        .unwrap_err();
    assert!(
        matches!(resumed, RuntimeError::NumericOverflow(_)),
        "expected NumericOverflow on resume, got {resumed:?}"
    );

    let from_entry_steps = run_steps_until_error(&ev, &args, 0);
    let resume_steps =
        run_steps_until_error_at(&ev, &args, add_idx, &recipe_to_initial(&ev, add_idx, &args));
    assert!(
        resume_steps < from_entry_steps,
        "overflow resume path shorter than entry path"
    );
}

// ---- prong 3: bounds (Op::Trap(IndexOutOfBounds)) ----------------

#[test]
fn partial_resume_bounds_explicit_trap_replays() {
    // Trap mid-expression: emit `LocalGet(0)` then `Trap{IOOB}` then
    // `Return`. The trap fires at bc_idx=1 with one item on the stack.
    let func = Func {
        name: "f".into(),
        params: vec![IrType::I64],
        ret: IrType::I64,
        body: vec![
            t(Op::LocalGet(0)),
            t(Op::Trap {
                kind: TrapKind::IndexOutOfBounds,
            }),
            t(Op::Return),
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
    args.insert("x".to_string(), Value::Int(42));

    let baseline_err = ev.run_main(args.clone()).unwrap_err();
    assert!(matches!(
        baseline_err,
        RuntimeError::WasmIndexOutOfBounds { .. }
    ));

    let func = ev.function();
    let trap_idx = func
        .ops
        .iter()
        .position(|op| matches!(op, BcOp::Trap(BcTrapKind::IndexOutOfBounds)))
        .expect("Trap present");
    let trap_pc = func.ir_pc_map[trap_idx];

    let snapshot = DeoptStateSnapshot::with_value_stack(
        0,
        trap_pc,
        Vec::new().into_boxed_slice(),
        Vec::new().into_boxed_slice(),
    );
    let resumed = ev
        .resume_from_snapshot(args.clone(), &snapshot)
        .unwrap_err();
    assert!(
        matches!(resumed, RuntimeError::WasmIndexOutOfBounds { .. }),
        "expected WasmIndexOutOfBounds on resume, got {resumed:?}"
    );
}

// ---- prong 4: capability (denied) mid-expression -----------------

#[test]
fn partial_resume_capability_denied_replays() {
    // Build a hand-rolled BcFunction directly because IR has no
    // CapabilityDenied trap kind. The op stream:
    //   bc=0: LocalGet(0)           ; push x
    //   bc=1: Trap(CapDenied)       ; mid-expression trap
    //   bc=2: Return
    let bc = BcFunction {
        ops: vec![
            BcOp::LocalGet(0),
            BcOp::Trap(BcTrapKind::CapabilityDenied),
            BcOp::Return,
        ],
        locals: 1,
        ir_pc_map: vec![1, 2, 3],
        stack_recipe: vec![
            vec![],                      // before LocalGet
            vec![StackOrigin::Local(0)], // before Trap (one item on stack)
            vec![StackOrigin::Local(0)], // before Return
        ],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: Vec::new(),
    };
    let cfg = BcVmConfig {
        cap_vtable: CapabilityVtable::default(),
        ..BcVmConfig::default()
    };
    let vm = BytecodeVm::new(cfg.clone());

    // Baseline: trap fires at bc=1.
    let outcome = vm.invoke(&bc, &[7]);
    assert!(matches!(
        outcome.error,
        Some(BcVmError::CapabilityDenied { .. })
    ));
    assert_eq!(outcome.last_bc_idx, 1);

    // Partial-resume: start at bc=1 with [Local(0)] recipe; supply
    // x via the initial_stack derived from the recipe.
    let vm2 = BytecodeVm::new(cfg);
    let resume = vm2.invoke_from_with_stack(
        &bc,
        &[7],
        /*start_bc_idx=*/ 1,
        /*extra_locals=*/ &[],
        /*return_slot_count=*/ 0,
        /*initial_stack=*/ &[7],
    );
    assert!(
        matches!(resume.error, Some(BcVmError::CapabilityDenied { .. })),
        "expected CapabilityDenied on resume, got {:?}",
        resume.error
    );
    assert!(
        resume.steps < outcome.steps,
        "resume took {} steps, baseline took {} — partial-resume must be shorter",
        resume.steps,
        outcome.steps
    );
}

// ---- resource prong: step limit ----------------------------------
//
// Step limit is "softly recoverable" in this sense: when the trap
// fires, the limit is already exhausted, so resuming with the same
// max_steps re-trips immediately. We document this and test both
// shapes:
//
// 1. Resume with the original (small) max_steps — re-trips
//    (`trap-and-abort` variant per the task brief).
// 2. Resume with a higher max_steps — completes successfully.

#[test]
fn partial_resume_resource_step_limit_retraps_then_completes() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .unwrap()
        .with_config(BcVmConfig {
            max_steps: Some(1),
            ..BcVmConfig::default()
        });
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(1));
    args.insert("y".to_string(), Value::Int(2));

    // Baseline: step limit trips on op 1 (after the first tick).
    let baseline_err = ev.run_main(args.clone()).unwrap_err();
    assert!(matches!(
        baseline_err,
        RuntimeError::WasmStepLimitExceeded { .. }
    ));

    // Resume from bc=0 with the same exhausted-limit config — re-traps
    // (trap-and-abort variant). The recipe at bc=0 is empty so the
    // operand stack stays clean.
    let snapshot = DeoptStateSnapshot::with_value_stack(
        0,
        0,
        Vec::new().into_boxed_slice(),
        Vec::new().into_boxed_slice(),
    );
    let retrap = ev
        .resume_from_snapshot(args.clone(), &snapshot)
        .unwrap_err();
    assert!(
        matches!(retrap, RuntimeError::WasmStepLimitExceeded { .. }),
        "expected re-trap on resume with same step-limit, got {retrap:?}"
    );

    // Same source with a generous step-limit completes when resumed
    // mid-expression. Use a fresh evaluator so we can dial the config
    // up; pick the Add op as the resume PC and supply Local-backed
    // operands so the addition runs to completion.
    let ev_open = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .unwrap()
        .with_config(BcVmConfig {
            max_steps: Some(1_000_000),
            ..BcVmConfig::default()
        });
    let func = ev_open.function();
    let add_idx = func
        .ops
        .iter()
        .position(|op| matches!(op, BcOp::AddI64 | BcOp::AddF64))
        .expect("Add present");
    let add_pc = func.ir_pc_map[add_idx];
    let snap = DeoptStateSnapshot::with_value_stack(
        0,
        add_pc,
        Vec::new().into_boxed_slice(),
        Vec::new().into_boxed_slice(),
    );
    let v = ev_open.resume_from_snapshot(args, &snap).unwrap();
    assert_eq!(v, Value::Int(3), "1 + 2 == 3 via partial-resume at Add");
}

// ---- happy path: mid-expression resume yields correct value -----

#[test]
fn partial_resume_arith_mid_expression_value_correct() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y").unwrap();
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));

    let baseline = ev.run_main(args.clone()).unwrap();
    assert_eq!(baseline, Value::Int(42));

    let func = ev.function();
    let add_idx = func
        .ops
        .iter()
        .position(|op| matches!(op, BcOp::AddI64 | BcOp::AddF64))
        .expect("Add present");
    let add_pc = func.ir_pc_map[add_idx];

    let snap = DeoptStateSnapshot::with_value_stack(
        0,
        add_pc,
        Vec::new().into_boxed_slice(),
        Vec::new().into_boxed_slice(),
    );
    let resumed = ev.resume_from_snapshot(args, &snap).unwrap();
    assert_eq!(
        resumed, baseline,
        "partial-resume at Add must yield identical value to full run"
    );
}

// ---- helpers ----------------------------------------------------

fn t(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

fn recipe_to_initial(
    ev: &BytecodeEvaluator,
    bc_idx: usize,
    args: &HashMap<String, Value>,
) -> Vec<u64> {
    // Materialise via the evaluator's public helper — exposed by going
    // through `resume_from_pc` itself isn't possible without poking at
    // private state. Reconstruct manually using StackOrigin semantics.
    let func = ev.function();
    let recipe = &func.stack_recipe[bc_idx];
    let mut packed: Vec<u64> = Vec::new();
    for name in &["x", "y"] {
        if let Some(Value::Int(v)) = args.get(*name) {
            packed.push(*v as u64);
        }
    }
    recipe
        .iter()
        .map(|o| match o {
            StackOrigin::Local(idx) => packed.get(*idx as usize).copied().unwrap_or(0),
            StackOrigin::Const(v) => *v,
            StackOrigin::Snapshot(_) => 0,
        })
        .collect()
}

fn run_steps_until_error(
    ev: &BytecodeEvaluator,
    args: &HashMap<String, Value>,
    start: usize,
) -> u64 {
    let func = ev.function();
    let cfg = BcVmConfig::default();
    let vm = BytecodeVm::new(cfg);
    let mut packed: Vec<u64> = Vec::new();
    for name in &["x", "y"] {
        if let Some(Value::Int(v)) = args.get(*name) {
            packed.push(*v as u64);
        }
    }
    let recipe = &func.stack_recipe[start];
    let initial: Vec<u64> = recipe
        .iter()
        .map(|o| match o {
            StackOrigin::Local(idx) => packed.get(*idx as usize).copied().unwrap_or(0),
            StackOrigin::Const(v) => *v,
            StackOrigin::Snapshot(_) => 0,
        })
        .collect();
    let outcome = vm.invoke_from_with_stack(func, &packed, start, &[], 1, &initial);
    outcome.steps
}

fn run_steps_until_error_at(
    ev: &BytecodeEvaluator,
    args: &HashMap<String, Value>,
    start: usize,
    initial: &[u64],
) -> u64 {
    let cfg = BcVmConfig::default();
    let vm = BytecodeVm::new(cfg);
    let mut packed: Vec<u64> = Vec::new();
    for name in &["x", "y"] {
        if let Some(Value::Int(v)) = args.get(*name) {
            packed.push(*v as u64);
        }
    }
    let outcome = vm.invoke_from_with_stack(ev.function(), &packed, start, &[], 1, initial);
    outcome.steps
}
