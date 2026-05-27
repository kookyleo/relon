//! Phase J.1 Phase 1 audit: walk the **production**-lowered IR for
//! W1 / W2 / W4 / W4_long / W10 (the same Relon sources cmp_lua feeds
//! through `try_build_bytecode`) through the trace recorder + JIT
//! install + dispatcher pipeline. Captures the precise stage at which
//! each workload diverges from the trace tier.
//!
//! ## Captured diagnostics per workload
//!
//! * Lowered IR top-level + nested `Block` / `Loop` body op stream.
//! * `RecorderState` outcome (`Recorded` / `Aborted { reason }`).
//! * Direct `TraceJitState::jit_compile_trace_for_fn` outcome.
//! * Full `install_recorder_trace_warmup` outcome.
//! * Direct `JITedTraceFn::invoke_raw` status code + `result_slot`.
//! * Deopt snapshot contents (`external_pc`, `ssa_slots_copy`,
//!   `value_stack_copy`) — load-bearing because the bytecode VM's
//!   `resume_from_deopt` reads these to continue dispatch.
//! * Verify-style cross-check: bytecode answer vs trace-tier answer
//!   for `n=32`.
//! * `ir_body_is_recorder_safe` predicate truth value (the gate
//!   `wire_trace_tier` consults before installing).
//! * `JitEvaluator::active_tier` after one `run_main` call.
//!
//! ## Phase 1 finding summary (per the J.1 spec deliverable)
//!
//! Pre-J.2 baseline (commit 7df7551, the audit baseline):
//!
//! | Workload     | Recorder | JIT install | Predicate | Verify | Active tier |
//! |--------------|----------|-------------|-----------|--------|-------------|
//! | W1           | OK       | OK          | accepts   | FAIL (0 vs 496) | Bytecode |
//! | W2           | OK       | OK          | accepts   | OK     | **Trace**   |
//! | W4           | OK       | OK          | accepts   | FAIL (1 vs 32)  | Bytecode |
//! | W4_long      | OK       | OK          | accepts   | FAIL (1 vs 32)  | Bytecode |
//! | W10          | OK       | OK          | rejects (`Op::If`) | FAIL (0 vs 4) | Bytecode |
//!
//! Across every failing row the deopt snapshot's `ssa_slots_copy` was
//! all-zeros — the trace runtime never wrote the loop-completed
//! `acc` / `i` into the deopt snapshot.
//!
//! ## Phase J.2 post-fix snapshot
//!
//! The fix wires a per-loop-iteration spill that stores each loop-
//! carried phi value into `TraceContext::ssa_slots[let_slot]` at
//! every `MarkLoopHead` re-entry. The post-fix audit produces:
//!
//! | Workload     | Verify          | Active tier | Notes |
//! |--------------|-----------------|-------------|-------|
//! | W1           | OK (496 vs 496) | **Trace**   | acc + i recovered |
//! | W2           | OK              | **Trace**   | unchanged, was already passing |
//! | W4           | FAIL (33 vs 32) | Bytecode    | snapshot OK but resume PC drifts (J.3) |
//! | W4_long      | FAIL (33 vs 32) | Bytecode    | same as W4 |
//! | W10          | FAIL (0 vs 4)   | Bytecode    | predicate still rejects `Op::If` (J.3) |
//!
//! W4 / W4_long still surface the wrong value because the deopt's
//! `external_pc` for the loop-exit BrIf maps to a bytecode index
//! that re-runs the final iteration's `count++`. The J.2 fix lands
//! the correct loop-carried state in the snapshot (`ssa_slots_copy[2]`
//! = `n` for both), but the resume-PC alignment for inner-loop-exit
//! guards under nested Block / Loop scopes still needs widening —
//! tracked as a Phase J.3 follow-up.
//!
//! This test never fails — it prints diagnostics and asserts only
//! that the audit ran. The post-J.2 deliverable is the captured
//! stdout + this header summary.

use std::collections::BTreeMap;

use relon::JitEvaluator;
use relon_bytecode::BytecodeEvaluator;
use relon_codegen_native::{
    global_trace_jit_state, install_recorder_trace_warmup_with_offset_map, RecordingOutcome,
    TraceRecordingEvaluator,
};
use relon_eval_api::{Evaluator, Value};
use relon_ir::{IrType, Op, TaggedOp};
use relon_trace_recorder::RecorderState;

/// W1 simple sum (matches `w1_relon_src` in cmp_lua).
const W1_SRC: &str = "#import list from \"std/list\"\n\
                       #main(Int n) -> Int\n\
                       list.sum(range(n))";

/// W2 sum-of-products (matches `w2_relon_src` in cmp_lua).
const W2_SRC: &str = "#import list from \"std/list\"\n\
                       #main(Int n) -> Int\n\
                       list.sum(range(n).map((i) => (i + 1) * (i + 2)))";

/// W4 string-contains scan (matches `w4_relon_src` in cmp_lua).
const W4_SRC: &str = "#import list from \"std/list\"\n\
                       #main(Int n) -> Int\n\
                       range(n)\n\
                         .map((i) => \"axb\")\n\
                         .filter((s) => s.contains(\"x\"))\n\
                         .len()";

/// W10 config-eval (bytecode-friendly variant; matches
/// `w10_relon_src_bytecode` in cmp_lua).
const W10_SRC: &str = "#import list from \"std/list\"\n\
                        #main(Int n) -> Int\n\
                        list.sum(range(n).map((i) =>\n\
                          (i % 3 == 0 || i % 3 == 1) &&\n\
                          (i % 4 == 0 || i % 4 == 1) &&\n\
                          (i % 24 >= 8 && i % 24 < 18) ? 1 : 0))";

/// Walk `body` through the trace recorder and report what happened.
fn audit_source(label: &str, src: &str, n: i64) {
    eprintln!("\n========== {label} ==========");
    eprintln!("source:\n{src}");

    // Build BytecodeEvaluator so we get the production-lowered IR body
    // (same one wire_trace_tier consumes).
    let ev = match BytecodeEvaluator::from_source(src) {
        Ok(ev) => ev,
        Err(e) => {
            eprintln!("[{label}] BytecodeEvaluator::from_source failed: {e}");
            return;
        }
    };
    let reg = ev.recording_registration_data();
    eprintln!(
        "[{label}] body.len = {}, param_tys = {:?}",
        reg.body.len(),
        reg.param_tys
    );

    eprintln!("[{label}] recursive IR op stream:");
    dump_ops(&reg.body, 0);

    // Drive the recorder over the body.
    let args: Vec<(u64, IrType)> = reg.param_tys.iter().map(|ty| (n as u64, *ty)).collect();
    let mut recorder = RecorderState::new();
    let offset_map: BTreeMap<u32, u32> = reg.field_offset_to_local.clone();
    let outcome = TraceRecordingEvaluator::record_and_run_with_offset_map(
        &mut recorder,
        &args,
        &reg.body,
        offset_map,
    );
    match outcome {
        RecordingOutcome::Recorded { result, .. } => {
            eprintln!("[{label}] RECORDED ok, result = {result}");
        }
        RecordingOutcome::Aborted {
            reason,
            partial_result,
        } => {
            eprintln!("[{label}] ABORTED: reason = {reason:?}, partial = {partial_result}");
            // Try to find offending op by name.
            let aborted_on = find_first_unsupported(&reg.body);
            if let Some((path, op_name)) = aborted_on {
                eprintln!(
                    "[{label}] static scan for first unsupported-class op: {op_name} at body path {path:?}"
                );
            } else {
                eprintln!(
                    "[{label}] static scan: no obviously-unsupported op variant — abort came from a structural / dynamic check"
                );
            }
            return;
        }
    }

    // Recorder accepted the IR — try direct JIT compile of the recorded
    // buffer first so we surface the **post-record** error (emitter
    // rejected op, optimizer pass panic'd, etc.) — `__relon_jump_to_recorder`
    // logs via `tracing` which isn't visible without a subscriber, so
    // we exercise the call path manually to capture the error.
    {
        let mut recorder2 = RecorderState::new();
        let outcome2 = TraceRecordingEvaluator::record_and_run_with_offset_map(
            &mut recorder2,
            &args,
            &reg.body,
            reg.field_offset_to_local.clone(),
        );
        match outcome2 {
            RecordingOutcome::Recorded {
                recorder: boxed, ..
            } => {
                let state = global_trace_jit_state();
                // Pick a low, deterministic fn_id within MAX_FN_ID range
                // (= 1024 today). Use 0..100 for the direct probe, 100..200
                // for the warmup probe to keep the two channels separate.
                let probe_fn_id: u32 = label
                    .bytes()
                    .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32))
                    % 100;
                state.invalidate_trace(probe_fn_id);
                match state.jit_compile_trace_for_fn(probe_fn_id, *boxed) {
                    Ok(_) => {
                        eprintln!("[{label}] direct jit_compile_trace_for_fn OK");
                    }
                    Err(e) => {
                        eprintln!("[{label}] direct jit_compile_trace_for_fn FAILED: {e:?}");
                    }
                }
            }
            RecordingOutcome::Aborted { reason, .. } => {
                eprintln!("[{label}] (re-record for direct jit_compile aborted: {reason:?})");
            }
        }
    }

    // Recorder accepted the IR — try the **install** path (record →
    // optimise → emit → JIT). This is what `wire_trace_tier` actually
    // calls, and where post-record failures (e.g. emitter rejects an
    // op) surface.
    eprintln!("[{label}] attempting full install_recorder_trace_warmup ...");
    let fn_id: u32 = 100
        + (label
            .bytes()
            .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32))
            % 100);
    let warmup_args: Vec<u64> = vec![n as u64];
    let offset_map_clone: BTreeMap<u32, u32> = reg.field_offset_to_local.clone();
    match install_recorder_trace_warmup_with_offset_map(
        fn_id,
        reg.body.clone(),
        reg.param_tys.clone(),
        offset_map_clone,
        &warmup_args,
    ) {
        Ok(_trace) => {
            eprintln!("[{label}] INSTALL ok — trace JIT pipeline accepts this body");
        }
        Err(reason) => {
            eprintln!("[{label}] INSTALL FAILED: {reason}");
        }
    }

    // Probe the JIT'd trace directly via `invoke_with_fallback`.
    // The trace lookup state holds the trace; we drive it with the
    // production args and look at the raw `result_slot`. If the trace
    // is supposed to run the full N iters, this should report 32 / 4.
    {
        let direct_fn_id: u32 = 500
            + (label
                .bytes()
                .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32))
                % 100);
        let probe_n: i64 = 32;
        let warmup_args: Vec<u64> = vec![probe_n as u64];
        match install_recorder_trace_warmup_with_offset_map(
            direct_fn_id,
            reg.body.clone(),
            reg.param_tys.clone(),
            reg.field_offset_to_local.clone(),
            &warmup_args,
        ) {
            Ok(_) => {
                let state = global_trace_jit_state();
                if let Some(trace) = state.lookup_trace(direct_fn_id) {
                    let mut tctx =
                        relon_trace_abi::TraceContext::with_capacity(trace.guard_table_len());
                    let args: [u64; 1] = [probe_n as u64];
                    let status = unsafe { trace.invoke_raw(&mut tctx as *mut _, args.as_ptr()) };
                    eprintln!(
                        "[{label}] direct trace invoke @ n={probe_n}: status={status}, result_slot={}",
                        tctx.result_slot
                    );
                }
            }
            Err(e) => eprintln!("[{label}] direct trace install failed: {e}"),
        }
    }

    // Probe the deopt snapshot the trace produces. This is the key
    // datum for understanding whether the post-loop bytecode resume
    // can recover the analytic answer: the snapshot's `ssa_slots_copy`
    // must contain the loop-completed acc/i values.
    {
        use std::sync::Arc;
        let snap_fn_id: u32 = 700
            + (label
                .bytes()
                .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32))
                % 100);
        let probe_n: i64 = 32;
        let warmup_args: Vec<u64> = vec![probe_n as u64];
        match install_recorder_trace_warmup_with_offset_map(
            snap_fn_id,
            reg.body.clone(),
            reg.param_tys.clone(),
            reg.field_offset_to_local.clone(),
            &warmup_args,
        ) {
            Ok(_) => {
                // Hook a lookup that captures the snapshot details.
                let lookup_handle: relon_bytecode::InstalledTraceLookupHandle =
                    Arc::new(relon_codegen_native::CraneliftTraceLookup);
                let _ = lookup_handle;
                // Drive via the state directly so we can see snapshot.
                let state = global_trace_jit_state();
                let args_arr: [u64; 1] = [probe_n as u64];
                let captured_snap = std::cell::Cell::new(None::<(u64, u64, Vec<u64>, Vec<u64>)>);
                let fallback_ret = unsafe {
                    state.invoke_with_resume(
                        snap_fn_id,
                        args_arr.as_ptr(),
                        64,
                        |_args, resume_pc, snap| {
                            if let Some(s) = snap {
                                captured_snap.set(Some((
                                    s.external_pc,
                                    resume_pc.unwrap_or(0),
                                    s.ssa_slots_copy.to_vec(),
                                    s.value_stack_copy.to_vec(),
                                )));
                            }
                            0u64
                        },
                    )
                };
                if let Some((ext_pc, resume_pc, ssa, vs)) = captured_snap.take() {
                    eprintln!(
                        "[{label}] deopt snapshot: external_pc={ext_pc} resume_pc={resume_pc} ssa_slots(len={}) = {:?} value_stack(len={}) = {:?} fallback_ret={fallback_ret}",
                        ssa.len(),
                        &ssa[..ssa.len().min(16)],
                        vs.len(),
                        vs,
                    );
                } else {
                    eprintln!(
                        "[{label}] no deopt — trace completed clean? fallback_ret={fallback_ret}"
                    );
                }
            }
            Err(e) => eprintln!("[{label}] snap-probe install failed: {e}"),
        }
    }

    // Probe install-then-invoke directly to see whether the freshly
    // installed trace returns the same answer the bytecode VM
    // produces. Mirrors `verify_installed_trace_against_bytecode`
    // (which is `pub(crate)` inside `relon::jit`).
    if reg.param_tys.len() == 1 && matches!(reg.param_tys[0], IrType::I64) {
        let verify_fn_id: u32 = 300
            + (label
                .bytes()
                .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32))
                % 100);
        let probe_n: i64 = 32;
        let warmup_args: Vec<u64> = vec![probe_n as u64];
        match install_recorder_trace_warmup_with_offset_map(
            verify_fn_id,
            reg.body.clone(),
            reg.param_tys.clone(),
            reg.field_offset_to_local.clone(),
            &warmup_args,
        ) {
            Ok(_) => {
                // Bytecode answer (no trace lookup).
                let mut bc_args = std::collections::HashMap::new();
                bc_args.insert("n".to_string(), Value::Int(probe_n));
                let bc_answer = Evaluator::run_main(&ev, bc_args.clone()).ok();

                // Traced answer — re-build a fresh BytecodeEvaluator
                // with the trace_lookup hooked so dispatcher-switch
                // fires.
                use std::sync::Arc;
                let lookup_handle: relon_bytecode::InstalledTraceLookupHandle =
                    Arc::new(relon_codegen_native::CraneliftTraceLookup);
                let ev_hooked = BytecodeEvaluator::from_source(src)
                    .unwrap()
                    .with_fn_id(verify_fn_id)
                    .with_trace_lookup(lookup_handle);
                let traced = Evaluator::run_main(&ev_hooked, bc_args).ok();
                eprintln!(
                    "[{label}] verify @ n={probe_n}: bytecode = {bc_answer:?}, trace = {traced:?}"
                );
            }
            Err(e) => {
                eprintln!("[{label}] verify install failed: {e}");
            }
        }
    }

    // Probe the same `ir_body_is_recorder_safe` predicate the wrapper
    // uses internally. The predicate is private so we re-implement it
    // here to keep the diagnostic precise.
    let (saw_loop, has_if, bad_storefield) = {
        fn walk(ops: &[TaggedOp], saw_loop: &mut bool, has_if: &mut bool, bad_sf: &mut bool) {
            for t in ops {
                match &t.op {
                    Op::If { .. } => *has_if = true,
                    Op::StoreField { ty, .. } if !matches!(ty, IrType::I64) => *bad_sf = true,
                    Op::Loop { body, .. } => {
                        *saw_loop = true;
                        walk(body, saw_loop, has_if, bad_sf);
                    }
                    Op::Block { body, .. } => walk(body, saw_loop, has_if, bad_sf),
                    _ => {}
                }
            }
        }
        let mut sl = false;
        let mut hi = false;
        let mut bs = false;
        walk(&reg.body, &mut sl, &mut hi, &mut bs);
        (sl, hi, bs)
    };
    eprintln!(
        "[{label}] ir_body_is_recorder_safe view: saw_loop={saw_loop} has_if={has_if} bad_storefield={bad_storefield} -> accepts={}",
        saw_loop && !has_if && !bad_storefield
    );

    // Drive JitEvaluator::new — the **production** wrapper bench `relon_jit`
    // row uses. Reports whether the wrapper auto-escalates this source
    // through `wire_trace_tier` to the Trace tier.
    match JitEvaluator::new(src) {
        Ok(jit) => {
            eprintln!(
                "[{label}] JitEvaluator::new ok; active_tier = {:?}",
                jit.active_tier()
            );
            let mut args = std::collections::HashMap::new();
            args.insert("n".to_string(), Value::Int(n));
            match Evaluator::run_main(&jit, args) {
                Ok(v) => eprintln!("[{label}] JitEvaluator::run_main result = {v:?}"),
                Err(e) => eprintln!("[{label}] JitEvaluator::run_main err = {e}"),
            }
            eprintln!("[{label}] post-call active_tier = {:?}", jit.active_tier());
        }
        Err(e) => {
            eprintln!("[{label}] JitEvaluator::new failed: {e}");
        }
    }
}

/// Recursive op-stream dumper. Walks into Block / Loop bodies so the
/// inner stdlib chain shows up.
fn dump_ops(ops: &[TaggedOp], indent: usize) {
    let pad = "  ".repeat(indent);
    for (i, t) in ops.iter().enumerate() {
        eprintln!("{pad}[{i:>3}] {}", op_summary(&t.op));
        match &t.op {
            Op::Block { body, .. } | Op::Loop { body, .. } => {
                dump_ops(body, indent + 2);
            }
            _ => {}
        }
    }
}

/// Stable, short summary of an op for the dump.
fn op_summary(op: &Op) -> String {
    match op {
        Op::ConstI64(v) => format!("ConstI64({v})"),
        Op::ConstBool(b) => format!("ConstBool({b})"),
        Op::ConstString { .. } => "ConstString".into(),
        Op::Add(t) => format!("Add({t:?})"),
        Op::Sub(t) => format!("Sub({t:?})"),
        Op::Mul(t) => format!("Mul({t:?})"),
        Op::Div(t) => format!("Div({t:?})"),
        Op::Eq(t) => format!("Eq({t:?})"),
        Op::Ne(t) => format!("Ne({t:?})"),
        Op::Lt(t) => format!("Lt({t:?})"),
        Op::Le(t) => format!("Le({t:?})"),
        Op::Gt(t) => format!("Gt({t:?})"),
        Op::Ge(t) => format!("Ge({t:?})"),
        Op::LocalGet(idx) => format!("LocalGet({idx})"),
        Op::LetGet { idx, ty } => format!("LetGet({idx}, {ty:?})"),
        Op::LetSet { idx, ty } => format!("LetSet({idx}, {ty:?})"),
        Op::LoadField { offset, ty } => format!("LoadField(off={offset}, {ty:?})"),
        Op::LoadStringPtr { offset } => format!("LoadStringPtr(off={offset})"),
        Op::StoreField { offset, ty } => format!("StoreField(off={offset}, {ty:?})"),
        Op::Block { body, result_ty } => {
            format!("Block(len={}, ty={result_ty:?})", body.len())
        }
        Op::Loop { body, result_ty } => {
            format!("Loop(len={}, ty={result_ty:?})", body.len())
        }
        Op::Br { label_depth } => format!("Br(depth={label_depth})"),
        Op::BrIf { label_depth } => format!("BrIf(depth={label_depth})"),
        Op::Return => "Return".into(),
        Op::Call {
            fn_index,
            arg_count,
            ..
        } => {
            format!("Call(fn_index={fn_index}, arg_count={arg_count})")
        }
        Op::CallClosure { .. } => "CallClosure".into(),
        Op::CallNative { .. } => "CallNative".into(),
        Op::If { .. } => "If".into(),
        Op::Select { .. } => "Select".into(),
        Op::MakeClosure { .. } => "MakeClosure".into(),
        other => format!("{other:?}"),
    }
}

/// Static scan for first op the recorder would refuse. Mirrors the
/// rejection rules in `relon_trace_recorder::lowering::lower_op` /
/// `unsupported_op_name` — kept as a coarse pre-screen so the audit
/// can flag the offender even when the recorder's abort message is
/// generic.
fn find_first_unsupported(body: &[TaggedOp]) -> Option<(Vec<usize>, &'static str)> {
    fn walk(ops: &[TaggedOp], path: &mut Vec<usize>) -> Option<(Vec<usize>, &'static str)> {
        for (i, t) in ops.iter().enumerate() {
            path.push(i);
            let name: Option<&'static str> = match &t.op {
                Op::If { .. } => Some("If"),
                Op::Select { .. } => Some("Select"),
                Op::CallClosure { .. } => Some("CallClosure"),
                Op::CallNative { .. } => Some("CallNative"),
                Op::MakeClosure { .. } => Some("MakeClosure"),
                Op::BrTable { .. } => Some("BrTable"),
                Op::Trap { .. } => Some("Trap"),
                Op::ReadStringLen => Some("ReadStringLen"),
                Op::ConstString { .. } => Some("ConstString"),
                Op::LoadStringPtr { .. } => Some("LoadStringPtr (raw, may abort)"),
                Op::ConstListInt { .. } => Some("ConstListInt"),
                _ => None,
            };
            if let Some(n) = name {
                return Some((path.clone(), n));
            }
            // Recurse into Block / Loop bodies.
            match &t.op {
                Op::Block { body, .. } | Op::Loop { body, .. } => {
                    if let Some(hit) = walk(body, path) {
                        return Some(hit);
                    }
                }
                _ => {}
            }
            path.pop();
        }
        None
    }
    let mut path = Vec::new();
    walk(body, &mut path)
}

#[test]
fn j1_w1_audit() {
    // W1 baseline: simplest stdlib chain (list.sum(range(n))).
    // Established to escalate to Trace tier by tier-breakdown tests.
    audit_source("W1", W1_SRC, 32);
}

#[test]
fn j1_w2_audit() {
    // W2 baseline: known shape that escalates via the `_fixture` row
    // (cmp_lua hand-built IR). Run the production source through this
    // audit to confirm whether it shares the same post-loop SSA
    // propagation gap as W4 / W10.
    audit_source("W2", W2_SRC, 32);
}

#[test]
fn j1_w4_audit() {
    audit_source("W4", W4_SRC, 32);
}

#[test]
fn j1_w10_audit() {
    audit_source("W10", W10_SRC, 32);
}

/// W4_long shares the same Relon source as W4 (only the haystack
/// literal differs and lives outside the source — see cmp_lua's W4_long
/// fixture for context). We still run the audit so any deltas in the
/// `tracing` log show up clearly.
#[test]
fn j1_w4_long_audit() {
    audit_source("W4_long", W4_SRC, 32);
}
