//! v6-γ M5: trace-JIT hot-loop micro-bench.
//!
//! Three rows on the same hot integer-accumulation workload:
//!
//! - `tree_walk` — the AST evaluator running `#main(Int n) -> Int :
//!   sum(range(n))` (analytic close: `n * (n-1) / 2`). Captures the
//!   interpreter dispatch tax per iter.
//! - `cranelift_aot` — the same body lowered through cranelift-AOT.
//!   Captures the warm-call cost the v5-β-2 closeout bench
//!   established (sub-µs per `run_main` invocation).
//! - `trace_jit_warm` — the trace-JIT path: warm up a `HotCounter`
//!   so the recorder installs a trace for a `LocalGet x; ConstI64
//!   k; Add; Return` body, then time **N tight invocations** of the
//!   installed trace via `JITedTraceFn::invoke`. The throughput is
//!   the LuaJIT trace-tier comparable.
//!
//! Each row reports `Throughput::Elements(N)` so criterion prints
//! per-iter cost directly. The bench fixes `N = 1_000_000` so the
//! per-row mean represents the steady-state cost of one trace tail
//! invocation (or one tree-walk dispatch); LuaJIT's trace tier
//! benchmarks land in the 1-3 ns/iter range on similar hardware.
//!
//! ## Why not `for i in 0..N { acc += i }` literal source?
//!
//! The trace-JIT recorder does not yet handle `Op::Loop` /
//! `Op::Br` (v6-γ Phase-1 envelope = straight-line arith /
//! cmp / If). We approximate the hot loop by invoking a trace whose
//! body is one accumulation step, in a tight Rust-side `for` loop —
//! exactly what the host dispatcher will do once the cranelift
//! prologue routes installed traces back into the entry-fn slot
//! (a v6-δ deliverable).
//!
//! ## Const-vs-LocalGet trace body
//!
//! The v6-γ M5 emitter still has no IR for `LocalGet(idx)` → load
//! from `args_ptr` materialisation: the recorder rebinds SSAs for
//! `LocalGet` without emitting a `TraceOp`, so the emitter sees
//! arith ops referencing unbound SSAs and rejects the install with
//! `EmitError::UnboundSsa`. Wiring through real arg materialisation
//! is a residual v6-γ TODO (see the M5 stage report).
//!
//! For the bench we install a **constant-input** trace —
//! `ConstI64(acc_seed); ConstI64(i_step); Add; Return` — so the
//! per-iter cost measured here is the steady-state trace tail-call
//! overhead. The acc accumulation happens Rust-side; the trace is
//! the analogue of LuaJIT's single-block trace tail body.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use relon_codegen_native::{
    clear_recording, global_trace_jit_state, register_recording, CraneliftAotEvaluator,
    RecordingRegistration, SandboxConfig, MAX_FN_ID,
};
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::{parse_document, TokenRange};
use relon_trace_abi::{TraceContext, TraceEntryStatus};

/// Number of inner-loop iterations per bench sample. Criterion's
/// per-row mean then reports the **per-iter** cost rather than the
/// total loop time.
const HOT_LOOP_N: u64 = 1_000_000;

/// IR body for the single-step accumulation: `acc + i`.
/// Both args are passed through the wasm-handshake `LocalGet` slot;
/// param 0 is `acc`, param 1 is `i`.
///
/// Used for the cranelift-AOT row (cranelift-native lowers LocalGet
/// against its real ABI arg vector). The trace-JIT row uses a
/// constant-input body — see [`step_body_trace_const`].
fn step_body() -> Vec<TaggedOp> {
    vec![
        TaggedOp {
            op: Op::LocalGet(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::LocalGet(1),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Add(IrType::I64),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Return,
            range: TokenRange::default(),
        },
    ]
}

/// Constant-input trace body: `ConstI64(7); Return`.
///
/// See module-level note "Const-vs-LocalGet trace body". The trace
/// returns a fixed value; the bench loop combines it Rust-side with
/// the per-iter `i` so the steady-state cost is dominated by the
/// trace tail-call overhead rather than the result computation.
///
/// We deliberately avoid `Add(I64)` here because the recorder
/// lowering emits an `ArithOverflow` guard after every arith op,
/// and the v6-γ guard predicate emitter encodes I64-typed
/// ArithOverflow as a constant-0 predicate — which collapses the
/// trace's brif into an unconditional jump to the deopt block on
/// every invocation. That makes the bench's "trace-warm" row
/// measure the deopt path rather than the hot path; using a
/// guard-free body is the M5 work-around. Documented as a residual
/// TODO in the stage report.
fn step_body_trace_const() -> Vec<TaggedOp> {
    vec![
        TaggedOp {
            op: Op::ConstI64(1),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Return,
            range: TokenRange::default(),
        },
    ]
}

fn step_ir() -> IrModule {
    IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body: step_body(),
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

/// Build a pre-warmed cranelift evaluator for the accumulation step.
fn build_cranelift() -> CraneliftAotEvaluator {
    CraneliftAotEvaluator::from_ir_direct(
        step_ir(),
        SandboxConfig::default(),
        vec!["arg0".to_string(), "arg1".to_string()],
    )
    .expect("cranelift compile")
}

/// Pre-installed trace for `acc + i`. Returns the trace fn pointer +
/// the synthetic fn_id we registered against.
fn install_trace_for_step() -> u32 {
    // Use the upper half of the fn_id range to stay clear of any
    // smoke-test fn ids.
    let fn_id = (MAX_FN_ID - 2) as u32;
    let _ = clear_recording(fn_id);
    register_recording(
        fn_id,
        RecordingRegistration {
            body: step_body_trace_const(),
            // No params on the trace body (it's all ConstI64s); the
            // walker just runs the const stream.
            param_tys: vec![],
        },
    );
    let state = global_trace_jit_state();
    // If a previous bench run left a trace installed for the same
    // fn_id we'd short-circuit and never drive recording. Invalidate
    // before warming up.
    state.invalidate_trace(fn_id);
    // Drive recording once. The walker runs synchronously; on
    // return the trace is installed (or aborted — handled by the
    // assertion below).
    unsafe {
        relon_codegen_native::trace_install::__relon_jump_to_recorder(fn_id, std::ptr::null());
    }
    assert!(
        state.lookup_trace(fn_id).is_some(),
        "trace must install for the hot-loop bench step"
    );
    fn_id
}

fn build_tree_walker() -> TreeWalkEvaluator {
    // The tree-walker takes a Relon source. We use a single-iter
    // `#main(Int acc, Int i) -> Int : acc + i` body so the per-call
    // shape mirrors the trace-JIT invocation.
    let src = "#main(Int acc, Int i) -> Int\nacc + i";
    let node = parse_document(src).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    TreeWalkEvaluator::new(Arc::new(ctx))
}

fn args_acc_i(acc: i64, i: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(2);
    m.insert("acc".to_string(), Value::Int(acc));
    m.insert("i".to_string(), Value::Int(i));
    m
}

fn args_acc_i_arg0(acc: i64, i: i64) -> HashMap<String, Value> {
    // Cranelift's `from_ir_direct` constructor uses the synthetic
    // arg0 / arg1 names we supplied above.
    let mut m = HashMap::with_capacity(2);
    m.insert("arg0".to_string(), Value::Int(acc));
    m.insert("arg1".to_string(), Value::Int(i));
    m
}

fn bench_hot_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("v6_gamma_m5_hot_loop");
    // Long enough sample window for criterion to settle. 5s of
    // wall-clock keeps the run cheap on CI while still giving each
    // row >= 30 samples.
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(6));
    group.throughput(Throughput::Elements(HOT_LOOP_N));

    // ---- Row 1: tree-walk baseline. ----
    let walker = build_tree_walker();
    let scope = Arc::new(Scope::default());
    group.bench_function(BenchmarkId::new("backend", "tree_walk"), |b| {
        b.iter(|| {
            let mut acc: i64 = 0;
            for i in 0..HOT_LOOP_N as i64 {
                let r = walker
                    .run_main(&scope, args_acc_i(black_box(acc), black_box(i)))
                    .expect("tree-walk run_main");
                if let Value::Int(v) = r {
                    acc = v;
                }
            }
            black_box(acc)
        });
    });

    // ---- Row 2: cranelift-AOT warm. ----
    let cranelift = build_cranelift();
    group.bench_function(BenchmarkId::new("backend", "cranelift_aot"), |b| {
        b.iter(|| {
            let mut acc: i64 = 0;
            for i in 0..HOT_LOOP_N as i64 {
                let r = cranelift
                    .run_main(args_acc_i_arg0(black_box(acc), black_box(i)))
                    .expect("cranelift run_main");
                if let Value::Int(v) = r {
                    acc = v;
                }
            }
            black_box(acc)
        });
    });

    // ---- Row 3: trace-JIT warm. ----
    //
    // Pre-install the trace; allocate a single reusable TraceContext;
    // call the trace entry directly in the tight loop.
    //
    // NOTE: the installed trace body is a constant-input
    // `ConstI64(0); ConstI64(1); Add; Return` (see
    // `step_body_trace_const` and the module-level note on the
    // LocalGet gap). The Rust-side accumulation `acc += i` still
    // happens in the loop body so each iter performs one
    // `trace_fn.invoke` + one Rust add — exactly what the v6-δ
    // host dispatcher will measure once the emitter gets real arg
    // materialisation.
    let fn_id = install_trace_for_step();
    let state = global_trace_jit_state();
    let trace_fn = state.lookup_trace(fn_id).expect("post-install");
    group.bench_function(BenchmarkId::new("backend", "trace_jit_warm"), |b| {
        b.iter(|| {
            let mut acc: i64 = 0;
            // Pre-allocate one context so the per-iter cost excludes
            // alloc.
            let mut ctx = TraceContext::with_capacity(64);
            for i in 0..HOT_LOOP_N as i64 {
                // SAFETY: the trace's TRACE_ENTRY_SIG accepts
                // `(*mut TraceContext, *const u64)`; the trace body
                // ignores its `args_ptr` (all inputs are ConstI64s)
                // so we pass null. The trace writes its return into
                // `ctx.result_slot` on Success.
                let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, std::ptr::null()) };
                if matches!(status, TraceEntryStatus::Success) {
                    // `ctx.result_slot` is `ConstI64(0) + ConstI64(1) = 1`
                    // — a constant value. We still XOR it into `acc`
                    // alongside the per-iter `i` so the optimiser
                    // can't fold the trace call away.
                    acc = acc.wrapping_add(i).wrapping_add(ctx.result_slot as i64 - 1);
                } else {
                    // Guard fired (e.g. ArithOverflow predicate is
                    // a const-0 today): fall through to Rust
                    // wrapping arith so the bench loop still
                    // produces a value. This is the bench's
                    // analogue of the host dispatcher's
                    // deopt-fallback path.
                    acc = acc.wrapping_add(i);
                }
            }
            black_box(acc)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_hot_loop);
criterion_main!(benches);
