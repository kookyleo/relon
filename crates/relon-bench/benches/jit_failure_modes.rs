//! Trace-JIT failure-mode fixtures (2026-05-21).
//!
//! Existing benches (`trace_jit_hot_loop`, `cmp_lua`) cover hot loops
//! where the trace recorder + JIT install path wins by 30 - 50% over
//! the cranelift AOT baseline (`trace_jit_loop` vs `cranelift_aot_loop`
//! lands in the 0.66× - 1.47× band). Those rows do not exercise the
//! three pathological paths the design doc calls out as trace-JIT's
//! Achilles heels:
//!
//! 1. **trace abort** — recorder hits an `UnsupportedOp` mid-loop and
//!    refuses to install. Subsequent calls keep paying the
//!    `lookup_trace` miss + a generic-backend fallback per call.
//! 2. **high deopt rate** — a guard fires every invocation, so the
//!    trace entry, guard check, `save_deopt` shim, and host fallback
//!    all run for nothing. Cranelift-AOT path skips every one.
//! 3. **cold workload** — workload finishes in fewer iterations than
//!    the trace JIT's amortisation window. Trace install / lookup +
//!    `TraceContext::with_hooks` per call dominate.
//!
//! The three workloads here mirror the `trace_jit_hot_loop` IR fixture
//! `sum_loop_let_slot_body` (recorder-driven `Op::Loop` body) so the
//! comparison stays apples-to-apples: same source-side shape, same
//! `cranelift_aot` AOT path, only the trace-JIT install /
//! invoke surface differs.
//!
//! Per fixture, three rows:
//!
//! - `tree_walk` — `TreeWalkEvaluator::run_main` for ground truth.
//! - `cranelift_aot` — `AotEvaluator::run_main`. The honest
//!   AOT baseline for the workload.
//! - `trace_jit` — invokes through `TraceJitState::invoke_with_fallback`
//!   where the fallback re-runs through the same `AotEvaluator`.
//!   The expectation per fixture:
//!
//!   - Fixture A (abort): trace never installs, every call hits the
//!     fallback. Expected delta vs `cranelift_aot`: lookup miss
//!     overhead (~few ns / call) plus the recorder's setup-time
//!     amortisation. **Hypothesis: trace_jit ≳ cranelift_aot.**
//!   - Fixture B (high deopt): trace installs, runs, deopts via an
//!     `IsZero` BrIf-polarity guard, falls back. Expected delta:
//!     trace prologue + guard fire + `save_deopt` + fallback.
//!     **Hypothesis: trace_jit > cranelift_aot.**
//!   - Fixture C (cold): trace installs but `n` per call is small
//!     (50). The per-call trace lookup + `TraceContext` setup amortises
//!     across only 50 inner iters. **Hypothesis: trace_jit ≥
//!     cranelift_aot at per-element cost.**
//!
//! Methodology mirrors `trace_jit_hot_loop`'s 6-trap hardening
//! (black_box × ≥ 2, `WARMUP_ITERS = 10_000` + `WARMUP_TIME_CAP_MS`
//! warmup, cache-prefill, `HOT_LOOP_N`-based throughput). The dedicated
//! file keeps the focus on failure modes; nothing here mutates the
//! global trace state used by the other benches because each fixture
//! uses an isolated `fn_id` slot below `MAX_FN_ID`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use relon_bench::quiescence::verify_quiescence;
use relon_codegen_native::{
    clear_recording, global_trace_jit_state, register_recording, AotEvaluator,
    RecordingRegistration, SandboxConfig, MAX_FN_ID,
};
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::TokenRange;

/// `HOT_LOOP_N` matches the `trace_jit_hot_loop` bench so the
/// per-iter cost comparison lines up with the existing rows.
const HOT_LOOP_N: u64 = 1_000_000;

/// Tree-walker scale — µs / iter class, so we drop two orders of
/// magnitude to keep the row's wall-clock under the warmup time cap.
/// Per-element-cost surfaces correctly via `Throughput::Elements`.
const TREE_WALK_LOOP_N: u64 = 10_000;

/// Cold-workload row's per-call `n`. Below the trace JIT's `HotCounter`
/// default threshold (`10`) by a factor that only amortises the
/// per-call setup cost across ~50 inner iters; the LuaJIT design doc
/// note that any sub-100-iter workload defeats trace tier holds here.
const COLD_LOOP_N: u64 = 50;

/// Methodology: 10k explicit warmup invocations before the timed
/// region. Identical to `trace_jit_hot_loop` so the steady-state
/// guarantee matches.
const WARMUP_ITERS: u64 = 10_000;

/// Wall-clock cap on the warmup loop. Some failure-mode rows fall
/// back through the tree-walker which is µs / call class; without
/// the cap a 10k warmup pass would push the per-row wall-clock past
/// 30 s.
const WARMUP_TIME_CAP_MS: u128 = 200;

/// Sample count for criterion. `100` is the floor enforced by the
/// `methodology_validators` test on the sibling `trace_jit_hot_loop`
/// bench (`p99.9 ≥ 1 tail sample`); we settle there because the
/// failure-mode rows compound the bench wall-clock — each row
/// already runs `HOT_LOOP_N × outer_calls` inner iterations against
/// up to three backends.
const SAMPLE_SIZE: usize = 100;

/// Same cache-prefill + warmup pattern as the existing benches:
/// one prefill `routine()` call, then `WARMUP_ITERS` capped by
/// `WARMUP_TIME_CAP_MS`, then time `iters` invocations.
#[inline(always)]
fn timed_with_warmup<F: FnMut()>(iters: u64, mut routine: F) -> Duration {
    // Trap D — cache-prefill.
    routine();
    // Trap B — explicit warmup before timed region.
    let warmup_start = Instant::now();
    let cap = Duration::from_millis(WARMUP_TIME_CAP_MS as u64);
    for _ in 0..WARMUP_ITERS {
        routine();
        if warmup_start.elapsed() >= cap {
            break;
        }
    }
    let start = Instant::now();
    for _ in 0..iters {
        routine();
    }
    start.elapsed()
}

/// Convenience tag wrapper for IR ops.
fn t(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

// =====================================================================
// =====  shared IR builders  ==========================================
// =====================================================================

/// Body slot layout for every fixture: `let_slot 0 = i`, `let_slot 1 =
/// acc`. The IR shape matches `trace_jit_hot_loop::sum_loop_let_slot_body`
/// so the recorder / AOT codegen take the same paths.
const SLOT_I: u32 = 0;
const SLOT_ACC: u32 = 1;

/// Hot-loop body: `acc = 0; i = 1; while i <= n { acc += i; i += 1 };
/// return acc`. Same shape as `sum_loop_let_slot_body` in
/// `trace_jit_hot_loop` — recorder lowers `Add(I64)` to `TraceOp::Add`
/// with an `ArithOverflow(dst)` guard, cranelift AOT lowers it to a
/// straight `iadd` (wrapping). Used by Fixture C (cold) where the
/// only difference vs the `trace_jit_hot_loop` row is the per-call
/// `n` (50 vs 1_000_000).
fn sum_loop_body() -> Vec<TaggedOp> {
    vec![
        // i = 1
        t(Op::ConstI64(1)),
        t(Op::LetSet {
            idx: SLOT_I,
            ty: IrType::I64,
        }),
        // acc = 0
        t(Op::ConstI64(0)),
        t(Op::LetSet {
            idx: SLOT_ACC,
            ty: IrType::I64,
        }),
        t(Op::Block {
            result_ty: None,
            body: vec![t(Op::Loop {
                result_ty: None,
                body: vec![
                    // exit when i > n
                    t(Op::LetGet {
                        idx: SLOT_I,
                        ty: IrType::I64,
                    }),
                    t(Op::LocalGet(0)),
                    t(Op::Gt(IrType::I64)),
                    t(Op::BrIf { label_depth: 1 }),
                    // acc += i
                    t(Op::LetGet {
                        idx: SLOT_ACC,
                        ty: IrType::I64,
                    }),
                    t(Op::LetGet {
                        idx: SLOT_I,
                        ty: IrType::I64,
                    }),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: SLOT_ACC,
                        ty: IrType::I64,
                    }),
                    // i += 1
                    t(Op::LetGet {
                        idx: SLOT_I,
                        ty: IrType::I64,
                    }),
                    t(Op::ConstI64(1)),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: SLOT_I,
                        ty: IrType::I64,
                    }),
                    t(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        t(Op::LetGet {
            idx: SLOT_ACC,
            ty: IrType::I64,
        }),
        t(Op::Return),
    ]
}

/// Fixture A body: the sum-loop with a `BitAnd(I64)` masking step
/// inside the loop. The mask is `i64::MAX`, so the analytic result
/// matches `sum_loop_body` — but `Op::BitAnd` is `AbortReason::
/// UnsupportedOp("BitAnd")` in the trace recorder (see
/// `relon-trace-recorder::lowering::lower_op`). Cranelift AOT lowers
/// `BitAnd(I64)` natively to one `band` instruction. Net effect: the
/// trace recorder aborts at install time; every `invoke_with_fallback`
/// call hits the fallback (here: the cranelift AOT evaluator).
fn bitand_loop_body() -> Vec<TaggedOp> {
    vec![
        // i = 1
        t(Op::ConstI64(1)),
        t(Op::LetSet {
            idx: SLOT_I,
            ty: IrType::I64,
        }),
        // acc = 0
        t(Op::ConstI64(0)),
        t(Op::LetSet {
            idx: SLOT_ACC,
            ty: IrType::I64,
        }),
        t(Op::Block {
            result_ty: None,
            body: vec![t(Op::Loop {
                result_ty: None,
                body: vec![
                    t(Op::LetGet {
                        idx: SLOT_I,
                        ty: IrType::I64,
                    }),
                    t(Op::LocalGet(0)),
                    t(Op::Gt(IrType::I64)),
                    t(Op::BrIf { label_depth: 1 }),
                    // acc += (i & i64::MAX)
                    t(Op::LetGet {
                        idx: SLOT_ACC,
                        ty: IrType::I64,
                    }),
                    t(Op::LetGet {
                        idx: SLOT_I,
                        ty: IrType::I64,
                    }),
                    t(Op::ConstI64(i64::MAX)),
                    t(Op::BitAnd(IrType::I64)),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: SLOT_ACC,
                        ty: IrType::I64,
                    }),
                    t(Op::LetGet {
                        idx: SLOT_I,
                        ty: IrType::I64,
                    }),
                    t(Op::ConstI64(1)),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: SLOT_I,
                        ty: IrType::I64,
                    }),
                    t(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        t(Op::LetGet {
            idx: SLOT_ACC,
            ty: IrType::I64,
        }),
        t(Op::Return),
    ]
}

/// Fixture B (high deopt) body: sum-loop guarded by a toggle. The
/// trace recorder follows the toggle's record-time value (0) into the
/// loop body and emits an `IsZero(toggle)` guard at the BrIf
/// fall-through site (recorder convention: BrIf `cond=0` means "stay
/// on the recorded path"). At runtime the bench passes a non-zero
/// toggle so the guard predicate `(toggle == 0)` is false on the very
/// first iteration — the trace entry returns `GuardFailed` and the
/// host fallback runs the full loop through `AotEvaluator`.
///
/// Cranelift-AOT lowers the same IR straightforwardly: the BrIf
/// branches out of the loop on the first iteration (because toggle !=
/// 0). The two backends therefore produce the same analytic result
/// (no iterations executed) — what differs is the per-call work:
///
/// - cranelift_aot: function prologue, one `LocalGet`, one `iconst 0`,
///   one `icmp`, one `brif`, return. ≈ ten machine instructions.
/// - trace_jit: lookup_trace, `TraceContext` setup, trace entry
///   prologue, the recorded BrIf gets baked away by the recorder
///   following the fall-through path, the `IsZero(toggle)` guard
///   evaluates to 0, `save_deopt` snapshot, return GuardFailed,
///   `invoke_with_fallback` resolves the snapshot and invokes the
///   fallback closure — which then re-runs the same workload through
///   cranelift_aot.
///
/// Net: per-call trace_jit pays full AOT cost + the deopt machinery
/// cost. The expected delta is hundreds of ns, not the ~5 ns lookup
/// miss seen on Fixture A.
///
/// Param layout (LocalGet indices):
/// - 0 — `n: I64` (loop bound)
/// - 1 — `toggle: I64` (0 at record time, non-zero at measurement)
fn toggle_loop_body() -> Vec<TaggedOp> {
    vec![
        // i = 1
        t(Op::ConstI64(1)),
        t(Op::LetSet {
            idx: SLOT_I,
            ty: IrType::I64,
        }),
        // acc = 0
        t(Op::ConstI64(0)),
        t(Op::LetSet {
            idx: SLOT_ACC,
            ty: IrType::I64,
        }),
        t(Op::Block {
            result_ty: None,
            body: vec![t(Op::Loop {
                result_ty: None,
                body: vec![
                    // exit when i > n  (normal loop exit; BrIf fires
                    // false at record-time when n >= 1)
                    t(Op::LetGet {
                        idx: SLOT_I,
                        ty: IrType::I64,
                    }),
                    t(Op::LocalGet(0)),
                    t(Op::Gt(IrType::I64)),
                    t(Op::BrIf { label_depth: 1 }),
                    // *** the deopt-driver BrIf ***
                    // exit if toggle != 0
                    t(Op::LocalGet(1)),
                    t(Op::ConstI64(0)),
                    t(Op::Ne(IrType::I64)),
                    t(Op::BrIf { label_depth: 1 }),
                    // acc += i
                    t(Op::LetGet {
                        idx: SLOT_ACC,
                        ty: IrType::I64,
                    }),
                    t(Op::LetGet {
                        idx: SLOT_I,
                        ty: IrType::I64,
                    }),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: SLOT_ACC,
                        ty: IrType::I64,
                    }),
                    // i += 1
                    t(Op::LetGet {
                        idx: SLOT_I,
                        ty: IrType::I64,
                    }),
                    t(Op::ConstI64(1)),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: SLOT_I,
                        ty: IrType::I64,
                    }),
                    t(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        t(Op::LetGet {
            idx: SLOT_ACC,
            ty: IrType::I64,
        }),
        t(Op::Return),
    ]
}

// =====================================================================
// =====  cranelift-AOT evaluator builders  ============================
// =====================================================================

fn build_aot_one_arg(body: Vec<TaggedOp>) -> AotEvaluator {
    let ir = IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64],
            ret: IrType::I64,
            body,
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    };
    AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["n".to_string()])
        .expect("cranelift AOT compile (1 arg)")
}

fn build_aot_two_args(body: Vec<TaggedOp>) -> AotEvaluator {
    let ir = IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body,
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    };
    AotEvaluator::from_ir_direct(
        ir,
        SandboxConfig::default(),
        vec!["n".to_string(), "toggle".to_string()],
    )
    .expect("cranelift AOT compile (2 args)")
}

// =====================================================================
// =====  bytecode-VM evaluator builders (B-4 deopt-recovery row)  =====
// =====================================================================
//
// Bytecode-coverage-expansion B-4: the deopt-recovery panel needs a
// `relon_deopt_to_bytecode` row that mirrors the existing
// `relon_deopt_to_tree_walk` baseline. Build the same IR shape the
// AOT helpers use so the trace's `IsZero(toggle)` guard fires
// identically on cold input, then route the fallback through the
// bytecode VM instead of the tree-walker. The expected steady-state
// ratio: bytecode resume should land at least an order of magnitude
// below the tree-walker fallback — the design target is ≥ 5×.

fn build_bytecode_two_args(body: Vec<TaggedOp>) -> relon_bytecode::BytecodeEvaluator {
    let ir = IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body,
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    };
    relon_bytecode::BytecodeEvaluator::from_ir_legacy(
        ir,
        vec!["n".to_string(), "toggle".to_string()],
    )
    .expect("bytecode VM compile (2 args)")
}

// =====================================================================
// =====  tree-walker evaluator builder  ===============================
// =====================================================================
//
// The tree-walker can't run a hand-built IR module directly — it
// requires a parsed AST. To keep the tree-walker row honest without
// hand-rolling Relon source for each fixture, we use a single
// representative Relon source (`list.sum(range(n))`) for the
// sum-loop and `cold` rows, and skip the tree-walker row for Fixture A
// (since `BitAnd` isn't surface syntax). The intent of the tree-walker
// row is the "what does the unspecialised path cost" datum; the same
// number applies across all three failure-mode shapes because the
// tree-walker has no IC / type-spec machinery.

fn build_tree_walker_sum() -> (TreeWalkEvaluator, Arc<Scope>) {
    let src = "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))";
    let node = relon_parser::parse_document(src).expect("tree-walk parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    (
        TreeWalkEvaluator::new(Arc::new(ctx)),
        Arc::new(Scope::default()),
    )
}

fn args_n_arg(n: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert("n".to_string(), Value::Int(n));
    m
}

fn args_n_toggle(n: i64, toggle: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(2);
    m.insert("n".to_string(), Value::Int(n));
    m.insert("toggle".to_string(), Value::Int(toggle));
    m
}

// =====================================================================
// =====  recorder install helpers  ====================================
// =====================================================================

/// Synthetic fn_id slots — picked below the ranges used by
/// `trace_jit_hot_loop` (`MAX_FN_ID - 4`, `MAX_FN_ID - 2`) and
/// `cmp_lua` (`MAX_FN_ID - 5..MAX_FN_ID - 12`).
const FIX_A_FN_ID: u32 = (MAX_FN_ID - 20) as u32;
const FIX_B_FN_ID: u32 = (MAX_FN_ID - 21) as u32;
const FIX_C_FN_ID: u32 = (MAX_FN_ID - 22) as u32;

/// Register a recording for `fn_id`, drive `__relon_jump_to_recorder`
/// with the provided warmup args, and return whether the trace was
/// installed. Used by Fixture A (returns `false` because BitAnd
/// aborts) and Fixture B / C (returns `true`).
fn try_install_recorder_trace(
    fn_id: u32,
    body: Vec<TaggedOp>,
    param_tys: Vec<IrType>,
    warmup_args: &[u64],
) -> bool {
    let _ = clear_recording(fn_id);
    let state = global_trace_jit_state();
    state.invalidate_trace(fn_id);
    register_recording(
        fn_id,
        RecordingRegistration {
            body,
            param_tys,
            ..Default::default()
        },
    );
    unsafe {
        relon_codegen_native::trace_install::__relon_jump_to_recorder(fn_id, warmup_args.as_ptr());
    }
    state.lookup_trace(fn_id).is_some()
}

// =====================================================================
// =====  bench entry  =================================================
// =====================================================================

fn bench_jit_failure_modes(c: &mut Criterion) {
    // Quiescence gate matches the other LuaJIT-comparison-ready benches.
    match verify_quiescence() {
        Ok(report) => eprintln!("[bench] {}", report.summary()),
        Err(err) => {
            eprintln!("[bench] {err}");
            eprintln!("[bench] {}", err.report.summary());
            panic!("machine not quiescent; set RELON_BENCH_FORCE_RUN=1 to override");
        }
    }

    let mut group = c.benchmark_group("jit_failure_modes");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(Duration::from_secs(6));

    // ----------------- shared tree-walker fixture ---------------------
    let (walker, scope) = build_tree_walker_sum();

    // =================================================================
    // Fixture A: trace abort (BitAnd UnsupportedOp)
    // =================================================================
    //
    // Setup: register the bitand-loop body for FIX_A_FN_ID, kick the
    // recorder once. The recorder will hit `Op::BitAnd(I64)` mid-body
    // and abort with `AbortReason::UnsupportedOp("BitAnd")`; no trace
    // is installed. `invoke_with_fallback` then short-circuits to the
    // cranelift-AOT fallback on every call.
    let aot_a = build_aot_one_arg(bitand_loop_body());
    // Sanity: the AOT path must succeed; the JIT abort is the only
    // failure mode we're trying to surface.
    {
        let v = aot_a
            .run_main(args_n_arg(HOT_LOOP_N as i64))
            .expect("Fixture A AOT run_main must succeed");
        if let Value::Int(got) = v {
            let expected = (1..=HOT_LOOP_N as i64).fold(0i64, |a, i| a.wrapping_add(i));
            assert_eq!(got, expected, "Fixture A AOT result mismatch");
        }
    }
    let a_installed = try_install_recorder_trace(
        FIX_A_FN_ID,
        bitand_loop_body(),
        vec![IrType::I64],
        &[3u64], // small n for the warmup record walk
    );
    assert!(
        !a_installed,
        "Fixture A invariant: BitAnd must abort the recorder (no trace installed)"
    );

    // Row A.1: cranelift_aot — the AOT baseline.
    group.throughput(Throughput::Elements(HOT_LOOP_N));
    let n_full = HOT_LOOP_N as i64;
    group.bench_function(BenchmarkId::new("fixture_a_abort", "cranelift_aot"), |b| {
        b.iter_custom(|iters| {
            let n_in = black_box(n_full);
            timed_with_warmup(iters, || {
                let v = aot_a
                    .run_main(args_n_arg(black_box(n_in)))
                    .expect("Fixture A aot row");
                black_box(v);
            })
        });
    });

    // Row A.2: trace_jit — goes through the install-then-fallback path.
    // Because the recorder aborted at setup, `lookup_trace` returns
    // None every call and `invoke_with_fallback` jumps straight to the
    // closure. The closure re-runs the SAME workload through the AOT
    // evaluator so the only delta vs the AOT row is the trace-state
    // lookup + the closure dispatch overhead.
    let state_a = global_trace_jit_state();
    group.bench_function(BenchmarkId::new("fixture_a_abort", "trace_jit"), |b| {
        b.iter_custom(|iters| {
            let n_in = black_box(n_full);
            let args: [u64; 1] = [n_in as u64];
            timed_with_warmup(iters, || {
                let v = unsafe {
                    state_a.invoke_with_fallback(
                        FIX_A_FN_ID,
                        black_box(args.as_ptr()),
                        64,
                        |_args| {
                            // Fallback: re-run the workload through the
                            // AOT path. Mirrors the production-side
                            // story where the trace-aware host hands
                            // control back to the generic backend on
                            // abort.
                            let v = aot_a
                                .run_main(args_n_arg(n_in))
                                .expect("Fixture A fallback");
                            match v {
                                Value::Int(x) => x as u64,
                                _ => 0,
                            }
                        },
                    )
                };
                black_box(v);
            })
        });
    });

    // Row A.3: tree_walk — the unspecialised path. Uses the
    // `sum_loop_body` source (no BitAnd at the source level since
    // Relon source has no `&` operator) so the per-element cost is
    // representative of the tree-walker's natural overhead. Drops to
    // `TREE_WALK_LOOP_N` because the tree-walker is µs / iter class;
    // throughput adjustment keeps the per-element-cost honest.
    let tw_n = TREE_WALK_LOOP_N as i64;
    group.throughput(Throughput::Elements(TREE_WALK_LOOP_N));
    group.bench_function(BenchmarkId::new("fixture_a_abort", "tree_walk"), |b| {
        b.iter_custom(|iters| {
            let n_in = black_box(tw_n);
            timed_with_warmup(iters, || {
                let v = walker
                    .run_main(&scope, args_n_arg(black_box(n_in)))
                    .expect("Fixture A tree walk row");
                black_box(v);
            })
        });
    });

    // =================================================================
    // Fixture B: high deopt (IsZero guard fires every call)
    // =================================================================
    //
    // Setup: register the toggle-loop body for FIX_B_FN_ID with
    // `n = 2, toggle = 0`. The recorder follows the BrIf
    // fall-through (cond=0) into the loop body and emits an
    // `IsZero(toggle_ssa)` guard so future invocations only stay
    // on-trace while toggle stays zero. At measurement time the
    // bench passes `toggle = 1`, so the guard fires on the very
    // first iteration, `save_deopt` records the snapshot, and the
    // host fallback runs the full loop through cranelift_aot.
    //
    // Cranelift-AOT path: the toggle-checking BrIf is just a normal
    // branch in cranelift — when `toggle != 0` the loop exits on
    // the first iteration with acc = 0 and the call returns. No
    // trap, no guard machinery.
    //
    // Hypothesis: trace_jit pays the trace-entry prologue + guard
    // predicate + save_deopt + fallback dispatch on top of the AOT
    // cost. Expected delta vs `cranelift_aot`: hundreds of ns / call
    // (vs ~5 ns lookup-miss on Fixture A).
    let aot_b = build_aot_two_args(toggle_loop_body());
    // Warmup args: n = 2, toggle = 0. Recorder takes the "stay in
    // loop" fall-through path and emits IsZero(toggle).
    let b_installed = try_install_recorder_trace(
        FIX_B_FN_ID,
        toggle_loop_body(),
        vec![IrType::I64, IrType::I64],
        &[2u64, 0u64],
    );
    // Honest record: if the recorder couldn't install, surface that —
    // Fixture B then degenerates into the same shape as Fixture A.
    // We log and continue rather than panic so the bench file still
    // produces a row even on a regression.
    eprintln!(
        "[bench] fixture_b_deopt: trace {} install",
        if b_installed { "did" } else { "did NOT" }
    );
    // At measurement time, toggle = 1 forces the trace's IsZero
    // guard (recorded at warmup with toggle = 0) to fire on entry.
    // The AOT row sees the BrIf branch out of the loop immediately
    // since `toggle != 0` is true.
    let toggle_runtime: i64 = 1;
    // Sanity: AOT result with `n = HOT_LOOP_N, toggle = 1` must be 0
    // (loop exits on first iter without accumulating). Confirms the
    // trap-free path before timing.
    {
        let v = aot_b
            .run_main(args_n_toggle(HOT_LOOP_N as i64, toggle_runtime))
            .expect("Fixture B AOT row sanity");
        if let Value::Int(got) = v {
            assert_eq!(got, 0, "Fixture B expected acc = 0 at toggle != 0");
        }
    }

    group.throughput(Throughput::Elements(HOT_LOOP_N));
    group.bench_function(BenchmarkId::new("fixture_b_deopt", "cranelift_aot"), |b| {
        b.iter_custom(|iters| {
            let n_in = black_box(HOT_LOOP_N as i64);
            let toggle_in = black_box(toggle_runtime);
            timed_with_warmup(iters, || {
                let v = aot_b
                    .run_main(args_n_toggle(black_box(n_in), black_box(toggle_in)))
                    .expect("Fixture B aot row");
                black_box(v);
            })
        });
    });

    let state_b = global_trace_jit_state();
    group.bench_function(BenchmarkId::new("fixture_b_deopt", "trace_jit"), |b| {
        b.iter_custom(|iters| {
            let n_in = black_box(HOT_LOOP_N as i64);
            let toggle_in = black_box(toggle_runtime);
            let args: [u64; 2] = [n_in as u64, toggle_in as u64];
            timed_with_warmup(iters, || {
                let v = unsafe {
                    state_b.invoke_with_fallback(
                        FIX_B_FN_ID,
                        black_box(args.as_ptr()),
                        64,
                        |_args| {
                            let v = aot_b
                                .run_main(args_n_toggle(n_in, toggle_in))
                                .expect("Fixture B fallback");
                            match v {
                                Value::Int(x) => x as u64,
                                _ => 0,
                            }
                        },
                    )
                };
                black_box(v);
            })
        });
    });

    // Tree-walker row uses the sum-loop source (no `toggle` knob at
    // surface syntax) — the row reports the µs-class unspecialised
    // baseline's per-element cost on the same loop shape. The
    // failure-mode contrast lives between cranelift_aot and trace_jit;
    // the tree-walker row anchors the absolute magnitude.
    group.throughput(Throughput::Elements(TREE_WALK_LOOP_N));
    group.bench_function(BenchmarkId::new("fixture_b_deopt", "tree_walk"), |b| {
        b.iter_custom(|iters| {
            let n_in = black_box(tw_n);
            timed_with_warmup(iters, || {
                let v = walker
                    .run_main(&scope, args_n_arg(black_box(n_in)))
                    .expect("Fixture B tree walk row");
                black_box(v);
            })
        });
    });

    // =================================================================
    // Bytecode-coverage-expansion B-4: deopt-recovery panel
    // =================================================================
    //
    // The two rows below are the deliverable for Phase B-4: side-by-
    // side timing of trace_jit deopt → bytecode_VM resume vs trace_jit
    // deopt → tree_walker resume. Both rows reuse Fixture B's setup
    // (the trace installs with toggle=0 / IsZero(toggle) guard; the
    // cold path passes toggle=1 so the guard fires on entry every
    // call). The only thing that differs is the fallback closure
    // handed to `state_b.invoke_with_fallback`.
    //
    // Acceptance gate from the design doc: bytecode resume should be
    // ≥ 5× faster than the tree-walker resume on the same workload.
    // Both rows share `HOT_LOOP_N` throughput so the per-element-cost
    // surfaces directly in the criterion report.
    let bc_b = build_bytecode_two_args(toggle_loop_body());
    // Sanity: bytecode result matches AOT (acc = 0 when toggle = 1).
    {
        let v = bc_b
            .run_main(args_n_toggle(HOT_LOOP_N as i64, toggle_runtime))
            .expect("Fixture B bytecode row sanity");
        if let Value::Int(got) = v {
            assert_eq!(got, 0, "Fixture B bytecode expected acc = 0 at toggle != 0");
        }
    }
    group.throughput(Throughput::Elements(HOT_LOOP_N));
    group.bench_function(
        BenchmarkId::new("fixture_b_deopt", "relon_deopt_to_bytecode"),
        |b| {
            b.iter_custom(|iters| {
                let n_in = black_box(HOT_LOOP_N as i64);
                let toggle_in = black_box(toggle_runtime);
                let args: [u64; 2] = [n_in as u64, toggle_in as u64];
                timed_with_warmup(iters, || {
                    let v = unsafe {
                        state_b.invoke_with_fallback(
                            FIX_B_FN_ID,
                            black_box(args.as_ptr()),
                            64,
                            |_args| {
                                let v = bc_b
                                    .run_main(args_n_toggle(n_in, toggle_in))
                                    .expect("Fixture B bytecode fallback");
                                match v {
                                    Value::Int(x) => x as u64,
                                    _ => 0,
                                }
                            },
                        )
                    };
                    black_box(v);
                })
            });
        },
    );

    group.bench_function(
        BenchmarkId::new("fixture_b_deopt", "relon_deopt_to_tree_walk"),
        |b| {
            // Tree-walker fallback runs the sum-loop source (the only
            // shape the walker speaks); it stands in for "the
            // walker's per-call cost on a body of comparable
            // arithmetic depth". The walker can't see the `toggle`
            // knob (no surface syntax), so the result diverges from
            // the trace's `acc = 0`; that's intentional — the row
            // measures fallback overhead, not result correctness.
            b.iter_custom(|iters| {
                let n_in = black_box(HOT_LOOP_N as i64);
                let toggle_in = black_box(toggle_runtime);
                let args: [u64; 2] = [n_in as u64, toggle_in as u64];
                // Drop to TREE_WALK_LOOP_N for the tree-walker
                // workload so the µs / iter class fallback doesn't
                // blow the warmup wall-clock cap; throughput is
                // adjusted accordingly via the per-bench `throughput`
                // setting (kept at HOT_LOOP_N so the per-element-cost
                // matches the bytecode row).
                let tw_n = TREE_WALK_LOOP_N as i64;
                timed_with_warmup(iters, || {
                    let v = unsafe {
                        state_b.invoke_with_fallback(
                            FIX_B_FN_ID,
                            black_box(args.as_ptr()),
                            64,
                            |_args| {
                                let v = walker
                                    .run_main(&scope, args_n_arg(tw_n))
                                    .expect("Fixture B tree-walk fallback");
                                match v {
                                    Value::Int(x) => x as u64,
                                    _ => 0,
                                }
                            },
                        )
                    };
                    black_box(v);
                })
            });
        },
    );

    // =================================================================
    // Fixture C: cold workload (n = 50 per call)
    // =================================================================
    //
    // Setup: register + install the sum-loop trace. At measurement
    // time we pass `n = COLD_LOOP_N = 50` per call. Each call ran
    // through the trace finishes in ~50 inner iters; the per-call
    // `TraceContext::with_hooks(64)` + `lookup_trace` + entry function
    // dispatch overhead is amortised across only 50 elements — too
    // few for the trace JIT to recover its fixed costs. The
    // outer Rust loop runs `OUTER_CALLS = HOT_LOOP_N / COLD_LOOP_N =
    // 20_000` calls per bench iteration, so the criterion timer sees
    // 20_000 × per-call cost. Throughput is set to `HOT_LOOP_N` so
    // per-element-cost surfaces directly in the report.
    let aot_c = build_aot_one_arg(sum_loop_body());
    let c_installed =
        try_install_recorder_trace(FIX_C_FN_ID, sum_loop_body(), vec![IrType::I64], &[3u64]);
    assert!(
        c_installed,
        "Fixture C invariant: vanilla sum-loop trace must install"
    );

    let outer_calls = HOT_LOOP_N / COLD_LOOP_N;
    // Sanity: AOT result of 50-iter loop.
    {
        let v = aot_c
            .run_main(args_n_arg(COLD_LOOP_N as i64))
            .expect("Fixture C AOT sanity");
        if let Value::Int(got) = v {
            let n = COLD_LOOP_N as i64;
            let expected = n * (n + 1) / 2;
            assert_eq!(got, expected, "Fixture C AOT analytic mismatch");
        }
    }

    group.throughput(Throughput::Elements(HOT_LOOP_N));
    group.bench_function(BenchmarkId::new("fixture_c_cold", "cranelift_aot"), |b| {
        b.iter_custom(|iters| {
            let n_in = black_box(COLD_LOOP_N as i64);
            timed_with_warmup(iters, || {
                let mut last: i64 = 0;
                for _ in 0..outer_calls {
                    let v = aot_c
                        .run_main(args_n_arg(black_box(n_in)))
                        .expect("Fixture C aot row");
                    if let Value::Int(x) = v {
                        last = x;
                    }
                }
                black_box(last);
            })
        });
    });

    let state_c = global_trace_jit_state();
    group.bench_function(BenchmarkId::new("fixture_c_cold", "trace_jit"), |b| {
        b.iter_custom(|iters| {
            let n_in = black_box(COLD_LOOP_N as i64);
            let args: [u64; 1] = [n_in as u64];
            timed_with_warmup(iters, || {
                let mut last: u64 = 0;
                for _ in 0..outer_calls {
                    let v = unsafe {
                        state_c.invoke_with_fallback(
                            FIX_C_FN_ID,
                            black_box(args.as_ptr()),
                            64,
                            |_args| {
                                let v = aot_c
                                    .run_main(args_n_arg(n_in))
                                    .expect("Fixture C fallback");
                                match v {
                                    Value::Int(x) => x as u64,
                                    _ => 0,
                                }
                            },
                        )
                    };
                    last = v;
                }
                black_box(last);
            })
        });
    });

    // Tree-walker uses TREE_WALK_LOOP_N-ish sizing — for the cold
    // fixture we keep the per-call `n = COLD_LOOP_N` but drop
    // `outer_calls` for tree-walker so wall-clock stays under
    // WARMUP_TIME_CAP_MS. `outer_calls / 100` keeps the row consistent.
    let tw_outer = (outer_calls / 100).max(1);
    let tw_total = tw_outer * COLD_LOOP_N;
    group.throughput(Throughput::Elements(tw_total));
    group.bench_function(BenchmarkId::new("fixture_c_cold", "tree_walk"), |b| {
        b.iter_custom(|iters| {
            let n_in = black_box(COLD_LOOP_N as i64);
            timed_with_warmup(iters, || {
                let mut last = Value::Null;
                for _ in 0..tw_outer {
                    last = walker
                        .run_main(&scope, args_n_arg(black_box(n_in)))
                        .expect("Fixture C tree walk row");
                }
                black_box(last);
            })
        });
    });

    group.finish();

    // Clean up trace state slots to avoid leaking installed traces
    // across runs (matters when the criterion harness drops `c` at
    // process end — invalidation here is defensive).
    state_a.invalidate_trace(FIX_A_FN_ID);
    state_b.invalidate_trace(FIX_B_FN_ID);
    state_c.invalidate_trace(FIX_C_FN_ID);
    let _ = clear_recording(FIX_A_FN_ID);
    let _ = clear_recording(FIX_B_FN_ID);
    let _ = clear_recording(FIX_C_FN_ID);
}

criterion_group!(benches, bench_jit_failure_modes);
criterion_main!(benches);
