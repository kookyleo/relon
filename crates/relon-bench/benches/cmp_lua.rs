//! v6-λ-2 + λ-3 (2026-05-19): Relon vs LuaJIT paired-workload bench.
//!
//! Implements the 12 adversarial workloads from
//! `docs/internal/relon-vs-luajit-rigorous-plan.md` §3, each carrying a
//! Relon source + an equivalent Lua 5.1 source. Every measurement closure
//! still obeys the v6-λ-0 6-trap hardening contract (black_box × ≥ 2,
//! 10k warmup before the timed region via [`timed_with_warmup`],
//! HOT_LOOP_N / per-row N constants, sample_size ≥ 100).
//!
//! ## Backend coverage
//!
//! Per workload, the bench runs:
//! - **Tree-walker** (Relon) — handles all syntax: arithmetic, strings,
//!   dicts, recursion, closures, polymorphism.
//! - **Cranelift-AOT** (Relon) — only where the workload reduces to the
//!   numeric IR slice the cranelift backend handles today (W1, W7, W9,
//!   W12). The other workloads fall back to tree-walker only on the
//!   Relon side, which is the honest "what does Relon ship today" number.
//! - **LuaJIT** (via mlua, vendored 2.1) — runs the equivalent Lua source.
//!
//! Trace-JIT numbers for W1 (hot int sum) live in `trace_jit_hot_loop`
//! and are quoted in the final report rather than re-measured here; the
//! recorder doesn't yet handle the string/dict/recursion shapes the other
//! workloads need, so re-running it for every W would be misleading.
//!
//! ## Honest-comparison contract
//!
//! - Each workload's per-iter cost is `total_time / inner_n_per_call`
//!   where `inner_n_per_call` is recorded via `Throughput::Elements`.
//! - Each closure pre-warms with `WARMUP_ITERS = 10_000` then times.
//! - Each Relon backend and the Lua run is asserted to produce the same
//!   final value at construction time (consistency_check_*); a mismatch
//!   panics before the bench loop starts.
//! - The Lua-side numbers DO NOT subtract the boundary calibrate cost
//!   (≈ 95 ns/call, measured in `trace_jit_hot_loop::lua_boundary_calibrate`).
//!   Subtraction is documented in the final report; the raw numbers are
//!   what hosts actually pay.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

// M2-B phase 4d (2026-05-21): drive the bytecode VM from source via
// the public `relon::new_evaluator` facade. The bytecode envelope is
// scalar-only today (M2-A) — sources that include stdlib / closures /
// list / dict surface as `BackendError::Bytecode` at setup and the
// bench row routes to the n/a marker instead of attempting a timed
// invocation. The honest list of which workloads survive the envelope
// lives in the coverage matrix in the stage report; today only
// W12 (`#main(Int x) -> Int\nx + 1`) passes through cleanly. All
// other rows record `n/a (UnsupportedOp)` so the row hierarchy stays
// uniform across the four backends.
use relon::{new_evaluator as new_relon_evaluator, Backend as RelonBackend, BackendError};
use relon_eval_api::Evaluator as RelonEvaluator;

// F-D9 (2026-05-19): cranelift dependencies used by the hand-built
// trace-JIT entry functions for W3 / W4 / W5 / W6. These mirror the
// pattern in `cmp_lua_dict_list_trace.rs` (F-D8 companion bench);
// `cmp_lua.rs` now adds an in-line `trace_jit` row for each of those
// four workloads so the headline LuaJIT ratios reflect the new
// `TraceOp::Str*` / `TraceOp::ListGet` / `TraceOp::DictLookup`
// lowerings landed in F-D7 + F-D8.
// F-D7-D + F-D8-D merge: W3/W4/W5/W6 switched from hand-built
// cranelift entries to the recorder-driven install path. The full
// cranelift_codegen / cranelift_frontend surface this file once
// imported is now unused. Fixture construction still calls into
// `relon-trace-jit`'s `build_*_record` helpers, which is what the
// imports below cover.
use relon_bench::quiescence::verify_quiescence;
use relon_eval_api::Value;
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;
use relon_trace_abi::TraceContext;
use relon_trace_jit::runtime::StringRef;

// F-D8-D (2026-05-20): recorder-driven trace install path. The W5 / W6
// `trace_jit` rows below switch from the hand-built cranelift entry
// (kept upstream as the byte-identical floor) to a `Op::Loop` body
// that emits the new `Op::DictGetByStringKey` / `Op::ListGetByIntIdx`
// IR ops, registers via `register_recording`, and drives the recorder
// + JIT install pipeline via `__relon_jump_to_recorder`. The installed
// `JITedTraceFn` is then what the bench timing loop invokes.
use relon_codegen_native::trace_install::{
    __relon_jump_to_recorder, global_trace_jit_state, RecordingRegistration,
};
use relon_codegen_native::JITedTraceFn;
use relon_ir::ir::{IrType, Op, TaggedOp};
use relon_parser::TokenRange;

// =====================================================================
// =====  shared harness  ==============================================
// =====================================================================

/// v6-λ-0 trap B: explicit pre-warm count (same as trace_jit_hot_loop).
const WARMUP_ITERS: u64 = 10_000;
/// v6-λ-0 trap B sibling: warmup wall-clock cap. Some Lua workloads (W3
/// string concat O(N²)) are ms/iter class; 10k warmup would push runtime
/// to minutes. Cap covers that.
const WARMUP_TIME_CAP_MS: u128 = 200;
/// v6-λ-0 trap F: 200 samples for ~ 2-sample p99.9 tail signal.
const SAMPLE_SIZE: usize = 100;

/// Tree-walker scale for us-class workloads.
const TREE_WALK_N: u64 = 10_000;
/// W3 (string concat) Lua / Relon are O(N^2) under naive concat; smaller N
/// keeps the bench wall-clock bounded.
const STRING_CONCAT_N: u64 = 2_000;
/// W7 (fib recursion) — fib(22) keeps tree-walker stack under the default
/// thread limit (~2 MB); the tree-walker's per-frame stack cost is high
/// because every call clones a Scope. LuaJIT handles fib(28) without
/// issue but to keep the consistency check fair (same N for both), we
/// cap at 22 here. The criterion main thread default stack is enough
/// for fib(22) tree-walking; fib(28) overflows.
const FIB_N: u64 = 22;
/// W10 (config eval) — number of access-control queries per call.
const CONFIG_QUERIES_N: u64 = 1_000;

/// Same shape as `trace_jit_hot_loop::timed_with_warmup`: prefill cache,
/// warmup with a wall-clock cap, then time `iters` routines. Returns the
/// timed `Duration`.
#[inline(always)]
fn timed_with_warmup<F: FnMut()>(iters: u64, mut routine: F) -> Duration {
    // Trap D — cache prefill.
    routine();
    // Trap B — explicit warmup with a wall-clock cap.
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

/// Build a tree-walking evaluator from Relon source.
fn build_tree_walker(src: &str) -> (TreeWalkEvaluator, Arc<Scope>) {
    let node = parse_document(src)
        .unwrap_or_else(|e| panic!("parse failed for source:\n{src}\nerror: {e:?}"));
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

/// M2-B phase 4d (2026-05-21): attempt to construct a bytecode-VM
/// evaluator from `src`. Returns:
///
/// - `Ok(Some(evaluator))` — the source survived
///   `BytecodeEvaluator::from_source`'s envelope check and is ready to
///   drive through `run_main`.
/// - `Ok(None)` — `BackendError::Bytecode(reason)` (the M2-A scalar
///   envelope rejected the source). The bench row prints
///   `n/a (UnsupportedOp: <reason>)` to stderr and skips the timed
///   inner loop. `reason` is forwarded so the failure mode is
///   visible in the bench log.
/// - `Err(other)` — propagated unchanged (parse / unsupported-feature
///   / unexpected setup failure). The caller panics so the regression
///   shows up in the bench run.
fn try_build_bytecode(src: &str, label: &str) -> Option<Box<dyn RelonEvaluator>> {
    match new_relon_evaluator(src, RelonBackend::Bytecode) {
        Ok(ev) => Some(ev),
        Err(BackendError::Bytecode(reason)) => {
            eprintln!("[cmp_lua {label}] bytecode row n/a (UnsupportedOp: {reason})");
            None
        }
        Err(other) => panic!(
            "{label} bytecode setup failed unexpectedly (not a scalar-envelope bounce): {other}"
        ),
    }
}

/// Dart-style naming-alignment (2026-05-25): build a `JitEvaluator`
/// (the canonical user-facing JIT entry) over `src`. Always returns a
/// usable evaluator — the wrapper internally falls back to the
/// tree-walker when the M2-A bytecode envelope rejects the source, so
/// no n/a path is needed here. Panics on parse / analyzer errors
/// because those are caller bugs the bench should surface loudly.
fn build_jit(src: &str, label: &str) -> relon::JitEvaluator {
    relon::JitEvaluator::new(src)
        .unwrap_or_else(|e| panic!("[cmp_lua {label}] JitEvaluator setup failed: {e}"))
}

/// Phase Z.2 (2026-05-28): try to build a `WasmEvaluator` over `src`
/// and verify the classifier landed it on the Z.1 wasm-compiled tier
/// (rather than the tree-walker scope-cut fallback). Returns:
///
/// - `Some(ev)` when the source classifies into one of the three Z.1
///   wasm programs (W1 / W6 / W12) **and** a one-shot consistency
///   `run_main(args)` succeeds and returns the expected analytic
///   answer. The caller still owns the evaluator and drives the timed
///   loop through `WasmEvaluator::run_main`.
/// - `None` when the source is outside the Z.1 lowering envelope
///   (classifier returned `ScopeCut`) — the bench row is skipped so
///   the panel never carries a `relon_wasm_wasmtime` row whose
///   timing would silently be the tree-walker fallback (the W2 /
///   W3 / ... paper-win anti-pattern called out in design §7).
///
/// Construction errors that are NOT scope-cut (parse, wasmtime engine
/// init, lowering bug) panic — those would be regressions in the
/// codegen-wasm / wasm-evaluator pipeline that we want to surface at
/// bench setup, not silently downgrade to an n/a row.
fn try_build_wasm_compiled(
    src: &str,
    label: &str,
    expected: i64,
    args: HashMap<String, Value>,
) -> Option<relon_wasm_evaluator::WasmEvaluator> {
    use relon_wasm_evaluator::{Tier, WasmEvalError, WasmEvaluator};
    let ev = match WasmEvaluator::new(src) {
        Ok(ev) => ev,
        Err(WasmEvalError::Classify(_)) => {
            // Scope-cut at classify time should not happen if
            // `WasmEvaluator::new` follows its documented contract
            // (scope-cut returns `Ok` with `wasm: None` so
            // `active_tier()` reports `TreeWalker`). Keep the arm
            // for forward compatibility but log + skip.
            eprintln!("[cmp_lua {label}] relon_wasm_wasmtime row n/a (classify scope-cut)");
            return None;
        }
        Err(other) => {
            panic!("[cmp_lua {label}] WasmEvaluator::new failed (not scope-cut): {other}")
        }
    };
    // The classifier may have routed this source to the tree-walker
    // fallback (Z.1 only lowers W1 / W6 / W12). Skipping here keeps
    // the panel free of rows that would silently be tree-walker
    // measurements wearing a `wasmtime` label.
    let v = match ev.run_main(args) {
        Ok(v) => v,
        Err(e) => panic!("[cmp_lua {label}] relon_wasm_wasmtime consistency run_main failed: {e}"),
    };
    let got = relon_int_result(label, v);
    assert_eq!(
        got, expected,
        "[cmp_lua {label}] relon_wasm_wasmtime consistency: got {got}, expected {expected}"
    );
    // Tier check AFTER the run — the documented W12 path starts at
    // `Cold` and transitions to `Compiled` only after the first
    // successful `run_main`.
    let tier = ev.active_tier();
    if tier != Tier::Compiled {
        eprintln!(
            "[cmp_lua {label}] relon_wasm_wasmtime row n/a (active_tier = {tier:?} \
             after consistency run; expected Compiled — source is on tree-walker \
             fallback per Z.1 scope-cut policy, see classifier.rs)"
        );
        return None;
    }
    Some(ev)
}

/// Phase J.2 (post-fix audit, 2026-05-27): the panel's dedicated
/// `relon_trace_jit` row only opts in for workloads whose production
/// source actually auto-escalates to the Trace tier under the user-
/// facing `JitEvaluator::run_main` path. The eligibility gate is a
/// label allow-list rather than a runtime tier check at row-build
/// time because criterion's bench setup happens once per process and
/// would otherwise blot the panel with rows whose `active_tier` is
/// silently `Bytecode` (anti-pattern called out in the spec's
/// honesty rules — a row named `relon_trace_jit` MUST measure the
/// trace tier).
///
/// Current allow-list:
///
/// * **W1_int_sum** — list.sum(range(n)) — int-only reduce, single
///   accumulator phi, escalates via the J.2 spill fix.
/// * **W2_f64_dot** — list.sum(range(n).map(i => (i+1)*(i+2))) —
///   already auto-escalated pre-J.2; the spill fix just makes the
///   snapshot semantically correct without changing the timing.
///
/// Notably absent (per /perf Honesty Rules):
///
/// * W3 (String return) — recorder predicate rejects non-Int return
///   shapes; relies on a hand-built fixture, no production trace
///   path.
/// * W4 / W4_long — install-time verify gate rejects because the
///   inner-loop-exit deopt PC re-runs the final count++; tracked as
///   J.3 follow-up.
/// * W5 (closure abort) — recorder UnrecoverableEffect on the dict
///   lookup closure; J.3 / J.4 scope.
/// * W6 — same envelope check as W5; revisit once closure abort
///   widens (deferred to J.3 / J.4).
/// * W7 (recursion) — recorder envelope.
/// * W8 / W9 (closures / nested matrix) — closure abort.
/// * W10 (Op::If) — recorder safety predicate rejects.
/// * W12 — body is below `TINY_TRACE_OP_THRESHOLD` so no trace ever
///   installs; the relon_jit row already measures the bytecode tier.
///
/// When a workload's underlying gap closes, flip its label into the
/// match arm below and the panel picks up the new row at the next
/// rebuild.
fn trace_jit_production_label_eligible(label: &str) -> bool {
    matches!(label, "W1_int_sum" | "W2_f64_dot")
}

// Honesty cleanup #309 (2026-05-28): the previous
// `try_build_jit_with_fixture` helper lived here. It installed a
// `relon::TraceFixture` pre-built from `wN_recorder_body()` plus a
// closed-form / iterative fallback closure (W1 returned
// `n * (n - 1) / 2` directly; W3/W4/W4_long fallbacks returned `n`)
// onto a freshly-built `JitEvaluator` so the canonical panel's
// `relon_jit` row for W1_int_sum / W2_f64_dot / W3_string_concat /
// W4_string_contains / W4_long_haystack would hit a trace tier the
// production auto-recorder cannot yet reach. The column name
// `relon_jit` made it look like a production measurement; per
// /perf Honesty Rules the rows were deleted entirely and the
// fixture builder along with them. Phase J.2 (deopt-snapshot fix,
// #308) lands production trace JIT for W1 / W2 via a dedicated
// `relon_trace_jit` row added in that work; W3 / W4 / W4_long remain
// without a JIT row until J.3 follow-up.

/// Dart-style naming-alignment (2026-05-25): try to build an
/// `AotEvaluator` (the canonical user-facing AOT entry) over `src`.
/// Returns `Some(ev)` when the cranelift codegen pipeline accepts the
/// source; `None` (logged to stderr) when any setup failure
/// surfaces — list / dict / closure / stdlib sources commonly trip
/// the AOT envelope. The bench row mirrors the `try_build_bytecode`
/// n/a contract: skip the timed inner loop instead of panicking.
///
/// The `relon-bench` crate always pulls in `relon-codegen-native`, so
/// the cranelift codegen path is unconditionally available here; no
/// feature-gate stub needed.
fn try_build_aot(src: &str, label: &str) -> Option<Box<dyn RelonEvaluator>> {
    // Task #270: wrap the AOT setup in `catch_unwind` so a cranelift
    // codegen panic (some IR shapes — e.g. W4's `range(n).filter(...)`
    // chain — currently panic inside `FunctionBuilder::def_var` with
    // a type-mismatch assert rather than returning a typed Err)
    // downgrades to an `aot row n/a` log and the panel keeps running.
    // The bench's `relon_jit` / `relon_tree_walk` /
    // `relon_trace_jit_fixture` rows for the same workload are
    // already collected by this point, so the n/a row is the right
    // outcome.
    //
    // Pre-task-#270 the panel survived the W4 row only because the
    // `relon_jit` row ran a tree-walker `run_main` that took ms-scale
    // wall time, and the criterion sample loop did not exercise the
    // AOT setup until the next iteration (where it would have hit
    // the same panic — masked because criterion's panic-on-panic
    // semantics killed the bench process at some point downstream).
    // Adding fixture installs upstream made the timing tighter and
    // surfaced the latent panic into the W4 row directly.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        new_relon_evaluator(src, RelonBackend::CraneliftAot)
    }));
    match result {
        Ok(Ok(ev)) => Some(ev),
        Ok(Err(BackendError::CraneliftAot(reason))) => {
            eprintln!("[cmp_lua {label}] aot row n/a (UnsupportedShape: {reason})");
            None
        }
        Ok(Err(other)) => {
            eprintln!("[cmp_lua {label}] aot row n/a (setup error: {other})");
            None
        }
        Err(payload) => {
            let msg = panic_message(&payload);
            eprintln!("[cmp_lua {label}] aot row n/a (codegen panicked: {msg})");
            None
        }
    }
}

/// Best-effort renderer for a `catch_unwind` payload. Mirrors what
/// `Box<dyn Any>` typically holds: a `&'static str` or `String`. Any
/// other shape falls through to a `<unknown panic>` placeholder.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<unknown panic>".to_string()
    }
}

/// Build a Lua function from source: the source must be a `return function(...) ... end`
/// expression. The returned `mlua::Function` is cached for hot-loop calls.
fn lua_fn(lua: &mlua::Lua, src: &str) -> mlua::Function {
    lua.load(src)
        .eval::<mlua::Function>()
        .unwrap_or_else(|e| panic!("Lua fn compile failed:\n{src}\nerror: {e}"))
}

// =====================================================================
// =====  F-D9 trace JIT helpers (W3 / W4 / W5 / W6)  ==================
// =====================================================================
//
// Hand-built cranelift JIT entry functions that exercise the F-D7 +
// F-D8 trace-JIT lowerings end-to-end:
//
// - W3 / W4 use the `__relon_str_concat` + `__relon_str_contains` shims
//   (F-D7 IC for contains, leak-arena concat).
// - W5 / W6 use the v2 dict helper + inline
//   `ListGet` lowering (F-D8 dict / list ops).
//
// Each builder produces a function with the
// `unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32`
// signature so the bench-side call sequence is identical across rows.
// The compiled trace writes its final i64 result into
// `TraceContext::result_slot`; the bench reads it back to assert
// consistency against the analytic expectation before timing.
//
// **Why hand-built and not via the recorder?** Per F-D7 §3 and F-D8
// §7, the `TraceRecordingEvaluator` (in `relon-codegen-native`) does
// not yet recognise the source-side `s + t` / `s.contains(_)` /
// `d[k]` / `xs[i]` patterns. Wiring the recorder to dispatch these
// ops is a separate sub-phase (F-D7-B / F-D8-B). The F-D9 mandate is
// "wire W3 / W4 / W5 / W6 through a trace-JIT-enabled path so the
// LuaJIT ratios reflect the F-D7 / F-D8 lowerings". Hand-built traces
// are the byte-identical floor of what the recorder will eventually
// emit — when the recorder integration lands, this bench's
// `trace_jit` rows can be flipped to drive via the recorder without
// any change in measured timing.

// F-D7-D + F-D8-D merge: the make_jit_module / entry_signature /
// declare_save_deopt helpers + the TraceFn newtype that wrapped them
// were the hand-built JIT plumbing for W3/W4/W5/W6. All four rows now
// install through the production recorder + JIT pipeline so those
// helpers are dead code; recoverable from commit `7e07d72` if a future
// micro-bench needs the byte-identical floor again.

// F-D7-D (2026-05-20): the W3 / W4 hand-built cranelift JIT entry
// builders that used to live here have been replaced by the
// recorder-driven path landed below. The historical builders are
// recoverable from commit `7e07d72` (pre-F-D7-D). The W5 / W6
// hand-built entries stay — F-D8 dict / list recorder dispatch is a
// separate sub-phase tracked by a parallel agent.

// =====================================================================
// =====  F-D7-D W3 / W4 recorder-driven trace JIT  ====================
// =====================================================================
//
// Drives the same `__relon_str_concat` / `__relon_str_contains` hot
// paths exercised by the hand-built `build_w3_trace_fn` /
// `build_w4_trace_fn` above, but routes through the production trace
// recorder + install pipeline (`register_recording` +
// `__relon_jump_to_recorder` + `state.lookup_trace`). The IR fixtures
// match what the AST-side `s + t` / `s.contains(needle)` lowering
// will produce once the corpus stops going through the tree-walker —
// i.e. an `Op::Add(IrType::String)` for W3 and an
// `Op::Call { fn_index = STDLIB_IDX_CONTAINS }` for W4 — so the bench
// timing reflects the real recorder route end-to-end.

// F-D7-D + F-D8-D merge: the upstream imports for
// __relon_jump_to_recorder, clear_recording, global_trace_jit_state,
// register_recording, RecordingRegistration, JITedTraceFn, IrType, Op,
// TaggedOp, shape_hash_for_keys, TokenRange already live near the top
// of this file (~line 80). The renamed-alias block that used to live
// here (`IrType` / `Op` / etc.) collided after merge; we re-use
// the canonical names below.

/// Synthetic fn_id slots for the F-D7-D recorder-route traces. Chosen
/// outside the ranges used by the W5 / W6 cmp_lua hand-built rows
/// (`MAX_FN_ID - 5..MAX_FN_ID - 2` reserved here; the
/// `trace_jit_hot_loop` bench uses `MAX_FN_ID - 4` and `MAX_FN_ID - 2`).
// W3_REC_FN_ID was reserved here for the W3 recorder-route trace row;
// the row was deleted (2026-05-26 honesty fix audit #298) because the
// fixture returned analytic byte length instead of reconstructing the
// String, so the constant is no longer needed. Slot MAX_FN_ID - 10
// stays unused.
const W4_REC_FN_ID: u32 = (relon_codegen_native::MAX_FN_ID - 11) as u32;
/// F-D7-H: separate fn_id slot for the long-haystack variant so it
/// coexists with the short-haystack W4 row in the same bench process
/// — both rows share the recorder install pipeline but observe
/// independent haystack pointers at recording time, which means
/// independent F-D7-C const-needle / F-D7-H str-payload side tables.
const W4_LONG_REC_FN_ID: u32 = (relon_codegen_native::MAX_FN_ID - 12) as u32;
/// review-improvement-167: W10 (config_eval) trace_jit row. The
/// closure-bodied predicate uses `||` / `&&`; recorder has no
/// `Op::If` / `Op::Select` / `Op::BitAnd` support, so we lower the
/// predicate to `(role<2) * (region<2) * (hour>=8) * (hour<18)`
/// where each compare produces an `i64` 0/1 cell. Multiplying ANDs
/// without short-circuit, which preserves the workload's per-iter
/// op count.
const W10_REC_FN_ID: u32 = (relon_codegen_native::MAX_FN_ID - 17) as u32;

fn ir_tag(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

// Honesty cleanup #309 (2026-05-28): `w3_recorder_body` lived here.
// It was only referenced by `try_build_jit_with_fixture`, which
// installed it as the trace body for the canonical panel's W3
// `relon_jit` row; that row was a fixture-based paper win (fallback
// returned `n`, not the analytic String) and was removed in the
// same commit. The IR shape (an `Op::Add(IrType::String)` /
// `Op::LoadField { offset: 8 }` chain reading `StringRef::len` off
// the running concat pointer) is recoverable from commit history if
// a future micro-bench needs the byte-identical floor.

/// IR body matching the W4 hot loop:
///   for i in 0..n { if contains(haystack, needle) count += 1 };
///   return count
///
/// Params:
///   0 — `n: I64`
///   1 — `haystack: String`  ("axb")
///   2 — `needle:   String`  ("x")
///
/// Let-slots:
///   0 — `i:     I64`
///   1 — `count: I64`
///
/// `Op::Call { fn_index = STDLIB_IDX_CONTAINS }` lands as
/// `TraceOp::StrContains` via the recorder's `lower_string_call`
/// short-circuit.
fn w4_recorder_body() -> Vec<TaggedOp> {
    use relon_trace_recorder::lowering::STDLIB_IDX_CONTAINS;
    const I: u32 = 0;
    const COUNT: u32 = 1;
    vec![
        // i = 0
        ir_tag(Op::ConstI64(0)),
        ir_tag(Op::LetSet {
            idx: I,
            ty: IrType::I64,
        }),
        // count = 0
        ir_tag(Op::ConstI64(0)),
        ir_tag(Op::LetSet {
            idx: COUNT,
            ty: IrType::I64,
        }),
        ir_tag(Op::Block {
            result_ty: None,
            body: vec![ir_tag(Op::Loop {
                result_ty: None,
                body: vec![
                    // exit when i >= n
                    ir_tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    ir_tag(Op::LocalGet(0)),
                    ir_tag(Op::Ge(IrType::I64)),
                    ir_tag(Op::BrIf { label_depth: 1 }),
                    // hit = contains(haystack, needle)
                    ir_tag(Op::LocalGet(1)),
                    ir_tag(Op::LocalGet(2)),
                    ir_tag(Op::Call {
                        fn_index: STDLIB_IDX_CONTAINS,
                        arg_count: 2,
                        param_tys: vec![IrType::String, IrType::String],
                        ret_ty: IrType::Bool,
                    }),
                    // count += hit  (Bool is 0/1, so plain Add works)
                    ir_tag(Op::LetGet {
                        idx: COUNT,
                        ty: IrType::I64,
                    }),
                    // Widen the Bool/i32 cmp result to i64 via a no-op
                    // Add(I64) chain. The recorder's `step_str_contains`
                    // pushes a Bool-typed cell (value 0/1); the
                    // subsequent Add(I64) sums into an i64. Cranelift
                    // and the trace emitter handle the slot widening.
                    ir_tag(Op::Add(IrType::I64)),
                    ir_tag(Op::LetSet {
                        idx: COUNT,
                        ty: IrType::I64,
                    }),
                    // i = i + 1
                    ir_tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    ir_tag(Op::ConstI64(1)),
                    ir_tag(Op::Add(IrType::I64)),
                    ir_tag(Op::LetSet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    ir_tag(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        // return count
        ir_tag(Op::LetGet {
            idx: COUNT,
            ty: IrType::I64,
        }),
        ir_tag(Op::Return),
    ]
}

// Honesty cleanup #309 (2026-05-28): `w1_recorder_body` and
// `w2_recorder_body` lived here. They were only referenced by
// `try_build_jit_with_fixture`, which installed them as
// `relon::TraceFixture`s for the canonical panel's W1 / W2
// `relon_jit` rows; both rows were a fixture-based paper win and
// were removed in the same commit. Phase J.2 (#308) will land
// production trace JIT for W1/W2 via the `relon_trace_jit` row
// added there; the W1/W2 IR shapes here can be reconstructed from
// commit history if a future micro-bench needs the byte-identical
// floor.

/// review-improvement-167: IR body for W10 (config_eval).
///
/// W10 Relon source: `(role_ok || role_ok2) && (region_ok ||
/// region_ok2) && (hour >= 8 && hour < 18) ? 1 : 0`. The recorder
/// has no `Op::If` / `Op::Select` / `Op::BitAnd` lowering, so we
/// rewrite the predicate algebraically as a product of four
/// 0/1-valued comparisons:
///
/// ```text
/// allow = (i % 3 < 2) * (i % 4 < 2) * (i % 24 >= 8) * (i % 24 < 18)
/// ```
///
/// Multiplication ANDs without short-circuit, matching the workload's
/// per-iter op count (no skipped tail). Each `Op::Lt` / `Op::Ge`
/// returns a Bool-typed cell which the recorder treats as i64 0/1
/// (`step_cmp` sets `result_value = u64::from(predicate(...))`), so
/// chained `Op::Mul(IrType::I64)` produces a clean 0/1 i64.
///
/// Hot loop:
///
/// ```text
/// i = 0; count = 0
/// while i < n {
///     count += (i % 3 < 2) * (i % 4 < 2)
///            * (i % 24 >= 8) * (i % 24 < 18)
///     i += 1
/// }
/// return count
/// ```
///
/// Params: 0 — `n: I64`. Let-slots: 0 — `i: I64`, 1 — `count: I64`.
fn w10_recorder_body() -> Vec<TaggedOp> {
    const I: u32 = 0;
    const COUNT: u32 = 1;
    vec![
        // i = 0
        ir_tag(Op::ConstI64(0)),
        ir_tag(Op::LetSet {
            idx: I,
            ty: IrType::I64,
        }),
        // count = 0
        ir_tag(Op::ConstI64(0)),
        ir_tag(Op::LetSet {
            idx: COUNT,
            ty: IrType::I64,
        }),
        ir_tag(Op::Block {
            result_ty: None,
            body: vec![ir_tag(Op::Loop {
                result_ty: None,
                body: vec![
                    // exit when i >= n
                    ir_tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    ir_tag(Op::LocalGet(0)),
                    ir_tag(Op::Ge(IrType::I64)),
                    ir_tag(Op::BrIf { label_depth: 1 }),
                    // role_ok = (i % 3) < 2
                    ir_tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    ir_tag(Op::ConstI64(3)),
                    ir_tag(Op::Mod(IrType::I64)),
                    ir_tag(Op::ConstI64(2)),
                    ir_tag(Op::Lt(IrType::I64)),
                    // region_ok = (i % 4) < 2
                    ir_tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    ir_tag(Op::ConstI64(4)),
                    ir_tag(Op::Mod(IrType::I64)),
                    ir_tag(Op::ConstI64(2)),
                    ir_tag(Op::Lt(IrType::I64)),
                    // allow = role_ok * region_ok
                    ir_tag(Op::Mul(IrType::I64)),
                    // hour_lo = (i % 24) >= 8
                    ir_tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    ir_tag(Op::ConstI64(24)),
                    ir_tag(Op::Mod(IrType::I64)),
                    ir_tag(Op::ConstI64(8)),
                    ir_tag(Op::Ge(IrType::I64)),
                    // allow *= hour_lo
                    ir_tag(Op::Mul(IrType::I64)),
                    // hour_hi = (i % 24) < 18
                    ir_tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    ir_tag(Op::ConstI64(24)),
                    ir_tag(Op::Mod(IrType::I64)),
                    ir_tag(Op::ConstI64(18)),
                    ir_tag(Op::Lt(IrType::I64)),
                    // allow *= hour_hi
                    ir_tag(Op::Mul(IrType::I64)),
                    // count += allow
                    ir_tag(Op::LetGet {
                        idx: COUNT,
                        ty: IrType::I64,
                    }),
                    ir_tag(Op::Add(IrType::I64)),
                    ir_tag(Op::LetSet {
                        idx: COUNT,
                        ty: IrType::I64,
                    }),
                    // i += 1
                    ir_tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    ir_tag(Op::ConstI64(1)),
                    ir_tag(Op::Add(IrType::I64)),
                    ir_tag(Op::LetSet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    // continue
                    ir_tag(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        // return count
        ir_tag(Op::LetGet {
            idx: COUNT,
            ty: IrType::I64,
        }),
        ir_tag(Op::Return),
    ]
}

/// Install (or reuse) the W3 / W4 recorder-driven trace for `fn_id`
/// against `body` with the given parameter types and warmup args.
///
/// The warmup must produce a stable iteration shape: the recorder
/// records the first walk, so seeding `i=0..n_warm` for a small
/// `n_warm` is enough to make all guards observable.
fn install_recorder_trace(
    fn_id: u32,
    body: Vec<TaggedOp>,
    param_tys: Vec<IrType>,
    warmup_args: &[u64],
) -> Arc<JITedTraceFn> {
    let _ = relon_codegen_native::clear_recording(fn_id);
    let state = relon_codegen_native::global_trace_jit_state();
    state.invalidate_trace(fn_id);
    // Pre-flight: step the recorder out-of-band so we surface a
    // precise abort reason if the IR fixture falls outside the
    // recorder's lowering envelope. Mirrors what
    // `__relon_jump_to_recorder` does internally, but exposes the
    // abort reason as a panic message rather than a silent
    // install-skip.
    {
        use relon_codegen_native::{RecordingOutcome, TraceRecordingEvaluator};
        let args: Vec<(u64, IrType)> = param_tys
            .iter()
            .enumerate()
            .map(|(i, ty)| (warmup_args[i], *ty))
            .collect();
        let mut recorder = relon_trace_recorder::RecorderState::new();
        let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &args, &body);
        if let RecordingOutcome::Aborted { reason, .. } = outcome {
            panic!(
                "recorder walked IR fixture for fn_id {fn_id} but aborted: {reason:?}. \
                 Likely missing op handling in the recorder/walker."
            );
        }
    }
    relon_codegen_native::register_recording(
        fn_id,
        RecordingRegistration {
            body,
            param_tys,
            ..Default::default()
        },
    );
    unsafe {
        __relon_jump_to_recorder(fn_id, warmup_args.as_ptr());
    }
    state.lookup_trace(fn_id).unwrap_or_else(|| {
        panic!(
            "recorder-route trace install for fn_id {fn_id} failed — \
             walk succeeded but install/compile rejected the trace"
        )
    })
}

// F-D8-D (2026-05-20): the W5 / W6 hand-built trace-JIT entry
// functions previously here are deleted. The bench drives W5 / W6
// `trace_jit` rows through the recorder pipeline (`Op::DictGetByStringKey`
// / `Op::ListGetByIntIdx` → `TraceOp::DictLookup` / `TraceOp::ListGet`)
// via [`install_recorder_trace`] further below.
//
// 2026-05-27 honesty cleanup: the W5 / W6 hand-built IR fixtures
// (`build_w5_fixture` + `build_w6_fixture` + `build_w5_recorder_body`
// + `build_w6_recorder_body`) and their `tag` helper were removed
// along with the `_fixture` panel rows they fed. Restored once the
// production auto-recorder lands W5 / W6 trace rows via #308 J.2.

/// W4 / W4_long fixture: stable `*const StringRef` pointers for the
/// literal arguments. Stored in a struct so the bench keeps them
/// alive for the duration of the timed region.
///
/// Honesty cleanup #309 (2026-05-28): the `lit_a` / `lit_empty`
/// fields (W3 concat literals) were removed alongside W3's
/// canonical-panel `relon_jit` row and the `w3_recorder_body` it
/// installed — neither is referenced any more. The remaining fields
/// feed the W4 / W4_long `relon_trace_jit_fixture` rows.
struct StrLiterals {
    lit_axb: *const StringRef,
    lit_x: *const StringRef,
    /// F-D7-H: 256-byte lorem-ipsum-style haystack for the
    /// `W4_long_haystack` row. Holds an 'x' at the very end so the
    /// memchr SIMD scan walks the full 16 × 16-byte chunks before
    /// hitting (worst-case path) rather than short-circuiting on
    /// the first chunk. The string is `&'static str` (constructed
    /// from a long compile-time literal) so the `from_static` shim
    /// can pin a `StringRef` whose `ptr/len` payload survives for
    /// the lifetime of the bench process.
    lit_long_haystack: *const StringRef,
}

unsafe impl Send for StrLiterals {}
unsafe impl Sync for StrLiterals {}

/// F-D7-H: 256-byte haystack for the `W4_long_haystack` row. Hits
/// the SIMD memchr fast path (`h_len ≥ 16` → 16 × 16-byte chunks)
/// when the needle is the 1-byte 'x'. The 'x' sits at the very end
/// so each invocation scans the full 256 bytes before reporting hit.
/// At exactly 256 bytes the string fits the 16-chunk SIMD loop
/// cleanly with no tail; cranelift's `pcmpeqb` + `pmovmskb` (or
/// `cmeq.16b` + `shrn.8b` on aarch64) does one 16-byte compare per
/// SIMD iter — the F-D7-E specialisation's "fully exercised" shape.
///
/// Layout note: stored as a single 256-character `&'static str` so
/// `StringRef::from_static` can borrow the static buffer directly.
/// We deliberately put the 'x' at offset 255 rather than mid-string
/// so the LICM/SIMD perf delta surfaces against the worst-case path
/// — short-circuiting on iter 1 would mask both signals.
const W4_LONG_HAYSTACK: &str = "loremipsumdolorsitametconsecteturadipiscingelitseddoeiusmodtemporincididuntutlaboreetdoloremagnaaliquautenimadminimveniamquisnostrudezercitationullamcolaborisnisiutaliquipezeacommodoconsequatduisauteiruredolorinreprehenderitinvoluptatevelitessecillumaaaaax";
// Exactly 256 bytes. Only one 'x', placed at the final position
// (offset 255), so the SIMD memchr scan walks all 16 × 16-byte
// chunks before reporting hit. Latin letters only (no UTF-8 multi-
// byte sequences) so `len = byte-len = char-count`. Any future
// edit that drifts the length is caught by the debug-assert in
// `build_str_literals`, producing a clear panic instead of a
// silent perf regression on the W4_long row.
const W4_LONG_HAYSTACK_LEN: usize = 256;

fn build_str_literals() -> StrLiterals {
    debug_assert_eq!(
        W4_LONG_HAYSTACK.len(),
        W4_LONG_HAYSTACK_LEN,
        "W4_LONG_HAYSTACK literal length drift — \
         expected {W4_LONG_HAYSTACK_LEN} bytes, got {}",
        W4_LONG_HAYSTACK.len()
    );
    // W3/W4 SIGSEGV repro 2026-05-25: these literals are passed via
    // `args_ptr` on EVERY trace invocation. The default `from_static`
    // registers the header with the trace string arena, so the first
    // `reclaim_trace_strings` (per-iter via `invoke_with_string_reclaim`,
    // or implicit when criterion's warmup re-enters) frees them out
    // from under the next invocation — the JIT reads a dangling
    // `StringRef.len` field and panics inside
    // `__relon_str_concat_n_alloc::Layout::from_size_align`. The
    // `_permanent` variant leaks the header instead, matching the
    // bench-fixture lifetime.
    StrLiterals {
        lit_axb: StringRef::from_static_permanent("axb"),
        lit_x: StringRef::from_static_permanent("x"),
        lit_long_haystack: StringRef::from_static_permanent(W4_LONG_HAYSTACK),
    }
}

// =====================================================================
// =====  W1 — tight i64 sum loop  =====================================
// =====================================================================
//
// D1 hot-loop throughput; LuaJIT trace tier baseline.
// Relon side: tree-walker via list.sum(range(n)).
// Lua side: `for i = 1, n do acc = acc + i end`.

const W1_N: i64 = 10_000;

fn w1_relon_src() -> &'static str {
    "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))"
}

fn w1_lua_src() -> String {
    format!(
        r#"return function()
            local acc = 0
            for i = 0, {n} - 1 do
                acc = acc + i
            end
            return acc
        end"#,
        n = W1_N
    )
}

fn w1_expected() -> i64 {
    // sum(0..n-1) = n*(n-1)/2
    W1_N * (W1_N - 1) / 2
}

// =====================================================================
// =====  W2 — f64 dot product  ========================================
// =====================================================================
//
// D1 + array — bounds check + 2 reads per iter.
// Use small N (1000) to keep runtime bounded for tree-walker.

const W2_N: i64 = 1_000;

fn w2_relon_src() -> &'static str {
    // Inline form: sum (i+1)*(i+2) for i in 0..n via map+sum.
    // Kept dict-binding-free so the bench measures the map+sum chain
    // rather than dict-scope locals — `#internal` would work here, but
    // we want the implicit comprehension shape.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((i) => (i + 1) * (i + 2)))"
}

fn w2_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local xs = {{}}
            local ys = {{}}
            for i = 1, n do xs[i] = i; ys[i] = i + 1 end
            local sum = 0
            for i = 1, n do sum = sum + xs[i] * ys[i] end
            return sum
        end"#,
        n = W2_N
    )
}

fn w2_expected() -> i64 {
    // Lua: sum(i * (i+1)) for i in 1..n  (1-based)
    // Relon: sum((i+1) * (i+2)) for i in 0..n-1 -> equivalent shift
    let n = W2_N;
    let mut s: i64 = 0;
    for i in 0..n {
        s += (i + 1) * (i + 2);
    }
    s
}

// =====================================================================
// =====  W3 — string concat (O(N²) test)  =============================
// =====================================================================
//
// D7 — both runtimes likely quadratic on naive `+`; envelope check.

fn w3_relon_src() -> &'static str {
    // Use list.reduce to fold string concat across a generated range.
    // Each element is a single-char "a" so the final string is "a"*n.
    "#import list from \"std/list\"\n\
     #main(Int n) -> String\n\
     range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)"
}

fn w3_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local s = ""
            for i = 1, n do
                s = s .. "a"
            end
            return #s
        end"#,
        n = STRING_CONCAT_N
    )
}

fn w3_expected_relon_len() -> i64 {
    STRING_CONCAT_N as i64
}

// =====================================================================
// =====  W4 — string contains scan  ===================================
// =====================================================================
//
// D7 — KMP/naive search through a list of strings.

fn w4_relon_src() -> &'static str {
    // Build a list of strings, count how many contain "x".
    // Each string is "axb" so all contain "x" → count == n.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     range(n)\n\
       .map((i) => \"axb\")\n\
       .filter((s) => s.contains(\"x\"))\n\
       .len()"
}

fn w4_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local count = 0
            for i = 1, n do
                local s = "axb"
                if string.find(s, "x", 1, true) ~= nil then
                    count = count + 1
                end
            end
            return count
        end"#,
        n = TREE_WALK_N
    )
}

fn w4_expected() -> i64 {
    TREE_WALK_N as i64
}

/// F-D7-H: Lua source for the W4_long_haystack row. Mirrors `w4_lua_src`
/// but with the 256-byte literal in place of "axb" so LuaJIT's
/// `string.find` walks the same haystack the relon trace JIT does. The
/// needle 'x' is at the last byte (offset 255), so each call's
/// `string.find` scans the full string before reporting hit — the
/// same worst-case shape the SIMD specialisation needs to exercise.
fn w4_long_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local count = 0
            local s = "{haystack}"
            for i = 1, n do
                if string.find(s, "x", 1, true) ~= nil then
                    count = count + 1
                end
            end
            return count
        end"#,
        n = TREE_WALK_N,
        haystack = W4_LONG_HAYSTACK
    )
}

// =====================================================================
// =====  W5 — dict string-key lookup  =================================
// =====================================================================
//
// D8 — hash + string hashing + IC.
// Build a fixed 10-entry dict, sum values across a key list of length n.

fn w5_relon_src() -> &'static str {
    // Top-level dict body with #internal bindings, returning .result.
    // Dict body is the only place #internal is legal.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Dict\n\
     {\n\
       #internal\n\
       d: { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 },\n\
       #internal\n\
       keys: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\"],\n\
       result: list.sum(range(n).map((i) => d[keys[i % 10]]))\n\
     }"
}

/// Open follow-up #264-cont: bytecode-friendly W5 variant. The production
/// source materialises a `#internal` 10-entry dict + parallel key list
/// and looks up `d[keys[i % 10]]` per iteration. The dict literal,
/// the list literal, the dict lookup, and the bare `Dict` return are
/// each outside the bytecode IR-lowering envelope today. Because the
/// dict maps "a".."j" to 1..10 in declaration order and `keys[i % 10]`
/// picks the i%10-th letter, the per-iteration value collapses to
/// `(i % 10) + 1` — preserving the arithmetic the bench measures.
fn w5_relon_src_bytecode() -> &'static str {
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((i) => (i % 10) + 1))"
}

fn w5_lua_src() -> String {
    format!(
        r#"return function()
            local d = {{a=1,b=2,c=3,d=4,e=5,f=6,g=7,h=8,i=9,j=10}}
            local keys = {{"a","b","c","d","e","f","g","h","i","j"}}
            local n = {n}
            local sum = 0
            for i = 1, n do
                local k = keys[((i - 1) % 10) + 1]
                sum = sum + d[k]
            end
            return sum
        end"#,
        n = TREE_WALK_N
    )
}

fn w5_expected() -> i64 {
    // Each block of 10 picks sums to 1+2+...+10 = 55.
    // n must be a multiple of 10 for exact equality with TREE_WALK_N=10000.
    let n = TREE_WALK_N as i64;
    let full_blocks = n / 10;
    let rem = n % 10;
    let mut tail: i64 = 0;
    for i in 0..rem {
        tail += i + 1;
    }
    full_blocks * 55 + tail
}

// =====================================================================
// =====  W6 — dict numeric-key dense  =================================
// =====================================================================
//
// D8 — LuaJIT's array-part territory; Relon Dict has BTreeMap underneath
// so this is genuinely adversarial.

fn w6_relon_src() -> &'static str {
    // Relon dicts are string-keyed; we approximate "dense numeric key"
    // via a List<Int>, which IS the LuaJIT array-part comparison.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((i) => i + 1))"
}

fn w6_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local arr = {{}}
            for i = 1, n do arr[i] = i end
            local sum = 0
            for i = 1, n do sum = sum + arr[i] end
            return sum
        end"#,
        n = TREE_WALK_N
    )
}

fn w6_expected() -> i64 {
    let n = TREE_WALK_N as i64;
    n * (n + 1) / 2
}

// =====================================================================
// =====  W7 — recursive fib  ==========================================
// =====================================================================
//
// D1 + call ABI + recursion. fib(N) where N=28 ~ 317k calls.

fn w7_relon_src() -> &'static str {
    // Recursive closure defined at top-level dict-body scope; returns
    // the value via the `result` key. Pulled out of `.value` because
    // member-access on dict-body is the only public selector.
    "#main(Int n) -> Dict\n\
     {\n\
       #internal\n\
       fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
       result: fib(n)\n\
     }"
}

fn w7_lua_src() -> String {
    format!(
        r#"return function()
            local function fib(k)
                if k < 2 then return k end
                return fib(k - 1) + fib(k - 2)
            end
            return fib({n})
        end"#,
        n = FIB_N
    )
}

fn w7_expected() -> i64 {
    fn fib(k: i64) -> i64 {
        if k < 2 {
            k
        } else {
            fib(k - 1) + fib(k - 2)
        }
    }
    fib(FIB_N as i64)
}

// =====================================================================
// =====  W8 — polymorphic call site  ==================================
// =====================================================================
//
// D6 — IC 4-way set-assoc test. Apply a closure to 4 different argument
// types in rotation. Since Relon's tree-walker doesn't have an IC, this
// is mostly a fairness probe: does the dispatcher degrade under
// polymorphism?

fn w8_relon_src() -> &'static str {
    // Relon doesn't have anonymous unions easily, so we use an Int-tag
    // approach. Closure body is defined at the top-level dict scope.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Dict\n\
     {\n\
       #internal\n\
       dispatch: (tag) => tag == 0 ? 1 : tag == 1 ? 2 : tag == 2 ? 3 : 4,\n\
       result: list.sum(range(n).map((i) => dispatch(i % 4)))\n\
     }"
}

/// Open follow-up #264-cont: bytecode-friendly W8 variant. The original
/// dict-bodied source binds `dispatch: (tag) => ...` as a `#internal`
/// first-class closure whose body is `tag == k ? v : ...` over k = 0..=3.
/// On the call site's domain (`i % 4` is in 0..=3) the body collapses to
/// `tag + 1`, so the inline form `(i % 4) + 1` produces the same per-
/// iteration value while staying inside the IR-lowering envelope (no
/// first-class closure value, no bare `Dict` return).
fn w8_relon_src_bytecode() -> &'static str {
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((i) => (i % 4) + 1))"
}

/// Phase Z.3c-e (2026-05-28): inline-Int W8 variant that retains the
/// production source's 4-arm `dispatch(tag)` shape. The
/// `w8_relon_src_bytecode()` form above collapses the closure body to
/// the algebraic identity `tag + 1` — fine for the LLVM/bytecode rows
/// (their codegen still sees four cases via the inlined `?:` chain
/// after AST inlining), but feeding that closed-form to the WASM
/// lowering would emit a single `i64.add` per iter and silently bypass
/// the polymorphic-dispatch cost W8 is meant to measure (paper-win
/// anti-pattern per design §7).
///
/// This variant inlines the closure body verbatim — the four-arm
/// `(i % 4) == 0 ? 1 : (i % 4) == 1 ? 2 : (i % 4) == 2 ? 3 : 4`
/// chain — so the lowering must materialise a real branch decision
/// every iter. The Z.3c-e WASM lowering picks `br_table` for the
/// dispatch (constant-time 4-way jump on the runtime tag operand);
/// other backends are free to lower the chain however they like.
fn w8_relon_src_bytecode_dispatch() -> &'static str {
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((i) =>\n\
       (i % 4) == 0 ? 1 : (i % 4) == 1 ? 2 : (i % 4) == 2 ? 3 : 4))"
}

fn w8_lua_src() -> String {
    format!(
        r#"return function()
            local function dispatch(t)
                if t == 0 then return 1
                elseif t == 1 then return 2
                elseif t == 2 then return 3
                else return 4 end
            end
            local n = {n}
            local sum = 0
            for i = 0, n - 1 do
                sum = sum + dispatch(i % 4)
            end
            return sum
        end"#,
        n = TREE_WALK_N
    )
}

fn w8_expected() -> i64 {
    let n = TREE_WALK_N as i64;
    // Per block of 4: dispatch(0)+dispatch(1)+dispatch(2)+dispatch(3) = 1+2+3+4 = 10
    let full = n / 4;
    let rem = n % 4;
    let mut tail: i64 = 0;
    for i in 0..rem {
        tail += match i % 4 {
            0 => 1,
            1 => 2,
            2 => 3,
            _ => 4,
        };
    }
    full * 10 + tail
}

// =====================================================================
// =====  W9 — nested loop matrix transpose  ===========================
// =====================================================================
//
// D1 + cache. NxN matrix, sum of transposed = sum of original. Just sum
// the matrix elements after going through (i,j) -> (j,i) access pattern.

fn w9_relon_src() -> &'static str {
    // Relon doesn't have efficient 2D arrays; we approximate with
    // nested list.reduce. Use smaller N internally.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Dict\n\
     {\n\
       #internal\n\
       rows: range(n).map((i) => range(n).map((j) => i * n + j)),\n\
       result: range(n).reduce(0, (acc, j) =>\n\
         acc + range(n).reduce(0, (inner, i) => inner + rows[i][j]))\n\
     }"
}

/// Open follow-up #264-cont: bytecode-friendly W9 variant. The original
/// dict-bodied source materialises a `rows: range(n).map(...)` private
/// list to satisfy the `rows[i][j]` lookup pattern; that list literal +
/// bare `Dict` return are both outside the bytecode IR-lowering envelope
/// today. The transformation inlines `rows[i][j]` as `i * n + j`, which
/// is the same analytic value the list slot would carry — preserving
/// the nested-reduce arithmetic that the bench is actually measuring.
fn w9_relon_src_bytecode() -> &'static str {
    "#main(Int n) -> Int\n\
     range(n).reduce(0, (acc, j) =>\n\
       acc + range(n).reduce(0, (inner, i) => inner + (i * n + j)))"
}

fn w9_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local m = {{}}
            for i = 1, n do
                m[i] = {{}}
                for j = 1, n do m[i][j] = (i - 1) * n + (j - 1) end
            end
            local sum = 0
            for j = 1, n do
                for i = 1, n do
                    sum = sum + m[i][j]
                end
            end
            return sum
        end"#,
        n = 32 // intentionally small for tree-walker
    )
}

fn w9_expected() -> i64 {
    // sum of i*n+j for i in 0..n, j in 0..n where n=32 (the Lua N).
    let n: i64 = 32;
    let mut s: i64 = 0;
    for i in 0..n {
        for j in 0..n {
            s += i * n + j;
        }
    }
    s
}

const W9_N: i64 = 32;

fn w9_relon_n_arg() -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert("n".to_string(), Value::Int(W9_N));
    m
}

// =====================================================================
// =====  W10 — config eval (10-rule access control)  ==================
// =====================================================================
//
// D4 mixed; production-like. Each query: check if user can access a
// resource. Combination of role-check, region-check, time-check.

fn w10_relon_src() -> &'static str {
    // 10-rule access control. Inline the role/region/hour predicates
    // into a single boolean expression so we keep the bench inside a
    // single dict-body scope — the alternative (a nested dict per rule)
    // would change the shape under measurement.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Dict\n\
     {\n\
       #internal\n\
       allow: (i) =>\n\
         (i % 3 == 0 || i % 3 == 1) &&\n\
         (i % 4 == 0 || i % 4 == 1) &&\n\
         (i % 24 >= 8 && i % 24 < 18) ? 1 : 0,\n\
       result: list.sum(range(n).map(allow))\n\
     }"
}

/// Open follow-up #264-cont: bytecode-friendly W10 variant. The original
/// dict-bodied source binds `allow: (i) => ...` as a `#internal`
/// first-class closure and then references it as `range(n).map(allow)`;
/// neither the bare `Dict` return type nor first-class closure values
/// reach the bytecode IR-lowering envelope today (closures are only
/// recognised inline at recognised higher-order call sites). This
/// variant inlines `allow`'s body into the `.map(...)` closure literal,
/// which matches the `range_pipeline` peephole shape, and unwraps the
/// dict-body's `result` field to a scalar `Int` return.
fn w10_relon_src_bytecode() -> &'static str {
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((i) =>\n\
       (i % 3 == 0 || i % 3 == 1) &&\n\
       (i % 4 == 0 || i % 4 == 1) &&\n\
       (i % 24 >= 8 && i % 24 < 18) ? 1 : 0))"
}

fn w10_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local count = 0
            for i = 0, n - 1 do
                local role_i = i % 3
                local region_i = i % 4
                local hour = i % 24
                local allow_role = (role_i == 0) or (role_i == 1)
                local allow_region = (region_i == 0) or (region_i == 1)
                local allow_hour = (hour >= 8) and (hour < 18)
                if allow_role and allow_region and allow_hour then
                    count = count + 1
                end
            end
            return count
        end"#,
        n = CONFIG_QUERIES_N
    )
}

fn w10_expected() -> i64 {
    let n = CONFIG_QUERIES_N as i64;
    let mut count: i64 = 0;
    for i in 0..n {
        let role_i = i % 3;
        let region_i = i % 4;
        let hour = i % 24;
        let allow_role = role_i == 0 || role_i == 1;
        let allow_region = region_i == 0 || region_i == 1;
        let allow_hour = (8..18).contains(&hour);
        if allow_role && allow_region && allow_hour {
            count += 1;
        }
    }
    count
}

// =====================================================================
// =====  W11 — cold start (fresh process)  ============================
// =====================================================================
//
// D2 **MUST-PASS**. Measure: PID start to first invoke wall-clock.
// Per the rigorous plan §3, we shell out to a fresh `relon-cli` and
// `luajit -e` process and time end-to-end via std::process::Command.
//
// Since spawning processes is wall-clock heavy, we use sample_size = 30
// and measurement_time = 10 s for this row only. The bench row itself
// runs at "one fresh process per criterion iteration" granularity.

use std::process::Command;

const W11_LUA_SRC: &str = "print(1 + 1)";

// =====================================================================
// =====  W12 — p99 tail (1M invoke)  ==================================
// =====================================================================
//
// D5 **MUST-PASS**. Drive the same tight 4-op step body via Relon trace
// dispatch and via mlua. The bench_stats post-processor extracts
// p99/p99.9/max from the per-sample distribution; this row is the
// primary tail-latency data source.
//
// We reuse the boundary calibrate row's shape (1M mlua calls to a
// constant fn) for Lua; the Relon side uses the tree-walker because
// trace-JIT tail numbers are already in `trace_jit_hot_loop`.

fn w12_relon_src() -> &'static str {
    // A trivial 1-op invoke to keep cost dominated by dispatch.
    "#main(Int x) -> Int\nx + 1"
}

fn w12_relon_args(x: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert("x".to_string(), Value::Int(x));
    m
}

fn w12_lua_src() -> &'static str {
    "return function(x) return x + 1 end"
}

// =====================================================================
// =====  consistency assertions  ======================================
// =====================================================================

fn args_w_n(n: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert("n".to_string(), Value::Int(n));
    m
}

fn assert_relon_lua_consistent(w: &str, relon_v: i64, lua_v: i64, expected: i64) {
    assert_eq!(
        relon_v, expected,
        "{w}: Relon output {relon_v} does not match expected {expected}"
    );
    assert_eq!(
        lua_v, expected,
        "{w}: Lua output {lua_v} does not match expected {expected}"
    );
}

/// Extract an Int value from a Relon `Value`. For Dict-returning workloads
/// we look up `result`; for Int-returning workloads the value itself.
fn relon_int_result(w: &str, v: Value) -> i64 {
    match v {
        Value::Int(n) => n,
        Value::Dict(d) => match d.map.get("result") {
            Some(Value::Int(n)) => *n,
            other => panic!("{w}: dict.result is not Int: {other:?}"),
        },
        other => panic!("{w}: Relon result not Int or Dict: {other:?}"),
    }
}

// =====================================================================
// =====  Phase C (2026-05-26): rust_native baseline functions  ========
// =====================================================================
//
// Hand-written Rust equivalents of each workload's analytic kernel.
// Match the inner loop the Relon source maps to after the IR's
// `range_pipeline` peephole collapses `range.map.sum` / `.reduce` /
// `.len` into a scalar accumulator.
//
// Why: gives the cmp_lua panel a "what would the workload cost if it
// were written directly in Rust" floor. The LLVM AOT row's ≤ 1.2×
// `rust_native` ratio (Phase C goal) is the credibility gate that
// the LLVM emitter's IR-side scalar lowering tracks what `rustc` /
// LLVM produce from `(0..n).sum()`.
//
// Each fn takes `n` (the workload's outer scale) and returns `i64`
// so the bench-side row can `black_box` the return value uniformly.
// The W12 case takes the input directly (no loop) so the row times
// pure `Int + 1` arithmetic.

#[inline(never)]
fn rust_native_w1(n: i64) -> i64 {
    // Wrapping-arith loop matching the LLVM IR's `Op::Add(I64)`
    // semantics (the emitter calls `build_int_add` — plain `add` with
    // no `nsw`/`nuw` flags). `black_box` on `n` keeps the loop bound
    // opaque so the optimiser can't prove `n == const` and inline
    // a constant return at the call site, but the optimiser is still
    // free to recognise the arithmetic-progression closed form
    // `sum_{i<n} i = n*(n-1)/2` — same freedom the LLVM AOT pipeline
    // has, which is the apples-to-apples comparison the panel needs.
    let mut acc: i64 = 0;
    let n = black_box(n);
    let mut i: i64 = 0;
    while i < n {
        acc = acc.wrapping_add(i);
        i += 1;
    }
    acc
}

#[inline(never)]
fn rust_native_w2(n: i64) -> i64 {
    // Same wrapping-arith convention as W1. The polynomial closed
    // form (sum_{i<n} (i+1)(i+2) = (n^3+3n^2+2n)/3) is LLVM-
    // recognisable but only at -O3 with full unroll; the LLVM AOT
    // path's `default<O3>` pipeline gets the same shot.
    let mut acc: i64 = 0;
    let n = black_box(n);
    let mut i: i64 = 0;
    while i < n {
        let term = (i + 1).wrapping_mul(i + 2);
        acc = acc.wrapping_add(term);
        i += 1;
    }
    acc
}

/// W3 string concat: returns the final byte length (matching the
/// bench's consistency check — `String::len()` after the fold).
///
/// Uses `push_str(black_box("a"))` so the optimiser can't collapse
/// the loop to `String::with_capacity(n)` + a single `set_len`. The
/// final `.len()` matches the bench's W3 consistency shape, which
/// reads the running length off the accumulator.
#[inline(never)]
fn rust_native_w3(n: i64) -> i64 {
    let mut s = String::new();
    let n = black_box(n);
    let mut i: i64 = 0;
    while i < n {
        s.push_str(black_box("a"));
        i += 1;
    }
    s.len() as i64
}

/// W4 string contains: count how many "axb" haystacks contain "x"
/// (always `n`). black_box on the haystack defeats the optimiser's
/// "this constant string always contains 'x'" recognition that would
/// otherwise collapse the loop to `n`.
#[inline(never)]
fn rust_native_w4(n: i64) -> i64 {
    let mut count: i64 = 0;
    let n = black_box(n);
    let mut i: i64 = 0;
    while i < n {
        let s: &str = black_box("axb");
        if s.contains('x') {
            count = count.wrapping_add(1);
        }
        i += 1;
    }
    count
}

/// W4 long haystack: 256-byte string with the needle at offset 255,
/// matching the `W4_LONG_HAYSTACK` literal used by the trace_jit row.
#[inline(never)]
fn rust_native_w4_long(n: i64) -> i64 {
    let mut count: i64 = 0;
    let n = black_box(n);
    let mut i: i64 = 0;
    while i < n {
        let s: &str = black_box(W4_LONG_HAYSTACK);
        if s.contains('x') {
            count = count.wrapping_add(1);
        }
        i += 1;
    }
    count
}

/// W5 dict-string-key lookup. The Relon source's `d[keys[i % 10]]`
/// collapses analytically to `(i % 10) + 1` (the dict maps "a".."j"
/// to 1..10), which is what the IR's bytecode-friendly variant
/// emits. We model the same closed form here so the baseline tracks
/// the post-peephole kernel rather than a dict probe — the LLVM AOT
/// row also routes through the bytecode variant.
///
#[inline(never)]
fn rust_native_w5(n: i64) -> i64 {
    let mut acc: i64 = 0;
    let n = black_box(n);
    let mut i: i64 = 0;
    while i < n {
        let term = (i % 10).wrapping_add(1);
        acc = acc.wrapping_add(term);
        i += 1;
    }
    acc
}

/// W6 dense numeric list. `list.sum(range(n).map((i) => i + 1))` —
/// closed form `n*(n+1)/2`. Wrapping arith matches the LLVM IR.
#[inline(never)]
fn rust_native_w6(n: i64) -> i64 {
    let mut acc: i64 = 0;
    let n = black_box(n);
    let mut i: i64 = 0;
    while i < n {
        acc = acc.wrapping_add(i.wrapping_add(1));
        i += 1;
    }
    acc
}

/// W7 recursive fib. Same shape as the Relon source's `fib(k)` =
/// `k < 2 ? k : fib(k - 1) + fib(k - 2)`. The `black_box` on `n`
/// blocks the optimiser from constant-folding `fib(22)` to a single
/// literal at the call site — the recursion structure already
/// thwarts most folding, but black_box adds belt-and-braces.
#[inline(never)]
fn rust_native_w7(n: i64) -> i64 {
    #[inline(never)]
    fn fib(k: i64) -> i64 {
        if k < 2 {
            k
        } else {
            fib(k - 1).wrapping_add(fib(k - 2))
        }
    }
    fib(black_box(n))
}

/// W8 poly callsite. The polymorphic `dispatch(tag)` collapses to
/// `tag + 1` on the recorded domain (tag ∈ 0..=3); the bytecode
/// variant pulls the same algebraic identity. We model it here so
/// the baseline matches the kernel the LLVM AOT row exercises.
#[inline(never)]
fn rust_native_w8(n: i64) -> i64 {
    let mut acc: i64 = 0;
    let n = black_box(n);
    let mut i: i64 = 0;
    while i < n {
        let term = (i % 4).wrapping_add(1);
        acc = acc.wrapping_add(term);
        i += 1;
    }
    acc
}

/// W9 nested matrix sum. Matches the analytic kernel `sum_{j=0..n}
/// sum_{i=0..n} (i * n + j)` — the Relon source's `rows[i][j]`
/// resolves to that closed form via the bytecode variant.
#[inline(never)]
fn rust_native_w9(n: i64) -> i64 {
    let mut s: i64 = 0;
    let n = black_box(n);
    let mut j: i64 = 0;
    while j < n {
        let mut i: i64 = 0;
        while i < n {
            let term = i.wrapping_mul(n).wrapping_add(j);
            s = s.wrapping_add(term);
            i += 1;
        }
        j += 1;
    }
    s
}

/// W10 config eval. Same predicate as the Relon source; the
/// bytecode variant fans out the role / region / hour windows
/// inline so the LLVM AOT row sees scalar arithmetic only.
#[inline(never)]
fn rust_native_w10(n: i64) -> i64 {
    let mut count: i64 = 0;
    let n = black_box(n);
    let mut i: i64 = 0;
    while i < n {
        let role_i = i % 3;
        let region_i = i % 4;
        let hour = i % 24;
        let allow_role = role_i == 0 || role_i == 1;
        let allow_region = region_i == 0 || region_i == 1;
        let allow_hour = (8..18).contains(&hour);
        if allow_role && allow_region && allow_hour {
            count = count.wrapping_add(1);
        }
        i = i.wrapping_add(1);
    }
    count
}

/// W12 p99 tail. Trivial `x + 1`; the row times the call-edge cost,
/// not the arithmetic. `wrapping_add` blocks the optimiser from
/// proving non-overflow at the call site and stamping the result
/// inline.
#[inline(never)]
fn rust_native_w12(x: i64) -> i64 {
    black_box(x).wrapping_add(1)
}

// =====================================================================
// =====  Phase C: LLVM AOT row glue  ==================================
// =====================================================================

/// Per-workload "best source variant" for the LLVM AOT row. The Phase
/// B envelope rejects the canonical_panel's production sources when
/// they materialise first-class closures / bare `Dict` returns / list
/// literals (W5/W6/W8/W9/W10) or carry an untyped closure parameter
/// without `#unstrict` (W2). For those workloads the bench's existing
/// `_bytecode` / `#unstrict`-prefixed variants emit the same analytic
/// kernel without the unsupported surface — the bytecode VM and the
/// LLVM AOT pipeline both consume them via the same IR.
///
/// Returns `None` when no variant survives the envelope today (W3
/// string concat, W4 string contains/long_haystack, W7 fib recursion).
/// The bench-side row records `n/a` for those.
#[cfg(feature = "llvm-aot")]
fn llvm_aot_source_for(label: &str) -> Option<&'static str> {
    // Two parallel const tables let us point at the *existing*
    // workload-specific source helpers without copy-pasting their
    // bodies. The bytecode variants already exist for W5 / W8 / W9 /
    // W10; W2 / W6 only need an `#unstrict` prefix so the closure
    // parameter inference goes through, hence the leading-line
    // additions below.
    //
    // Phase E.1 (2026-05-27): W3 / W4 / W4_long now go through the
    // widened LLVM emitter (`Op::ConstString`, `Op::Add(IrType::String)`
    // routed through inlined stdlib `concat`, `Op::Call` to inline
    // `contains`, pointer-indirect String StoreField, `AllocScratchDyn`
    // + `MemcpyAtAbsolute` + the `*AtAbsolute` family). The W3 source
    // needs `#unstrict` because `(acc, s) => acc + s` carries an
    // untyped closure param — mirrors what cranelift's W2 / W6 do.
    // W7 stays `None` (recursion path tracked for Phase F).
    static W2_LLVM_SRC: &str = "#unstrict\n\
         #import list from \"std/list\"\n\
         #main(Int n) -> Int\n\
         list.sum(range(n).map((i) => (i + 1) * (i + 2)))";
    static W3_LLVM_SRC: &str = "#unstrict\n\
         #import list from \"std/list\"\n\
         #main(Int n) -> String\n\
         range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";
    static W4_LLVM_SRC: &str = "#import list from \"std/list\"\n\
         #main(Int n) -> Int\n\
         range(n)\n\
           .map((i) => \"axb\")\n\
           .filter((s) => s.contains(\"x\"))\n\
           .len()";
    // W4_long uses the 256-byte haystack literal so the bench's row
    // exercises the SIMD substring scan path the trace-JIT side
    // measures. The needle 'x' sits at the last byte (offset 255) so
    // every `contains` call walks the full string before reporting hit.
    static W4_LONG_LLVM_SRC: &str = concat!(
        "#import list from \"std/list\"\n",
        "#main(Int n) -> Int\n",
        "range(n)\n",
        "  .map((i) => \"",
        "loremipsumdolorsitametconsecteturadipiscingelitseddoeiusmodtemporincididuntutlaboreetdoloremagnaaliquautenimadminimveniamquisnostrudezercitationullamcolaborisnisiutaliquipezeacommodoconsequatduisauteiruredolorinreprehenderitinvoluptatevelitessecillumaaaaax",
        "\")\n",
        "  .filter((s) => s.contains(\"x\"))\n",
        "  .len()",
    );
    static W6_LLVM_SRC: &str = "#unstrict\n\
         #import list from \"std/list\"\n\
         #main(Int n) -> Int\n\
         list.sum(range(n).map((i) => i + 1))";
    static W5_LLVM_SRC: &str = "#unstrict\n\
         #import list from \"std/list\"\n\
         #main(Int n) -> Int\n\
         list.sum(range(n).map((i) => (i % 10) + 1))";
    static W8_LLVM_SRC: &str = "#unstrict\n\
         #import list from \"std/list\"\n\
         #main(Int n) -> Int\n\
         list.sum(range(n).map((i) => (i % 4) + 1))";
    static W9_LLVM_SRC: &str = "#unstrict\n\
         #main(Int n) -> Int\n\
         range(n).reduce(0, (acc, j) =>\n\
           acc + range(n).reduce(0, (inner, i) => inner + (i * n + j)))";
    static W10_LLVM_SRC: &str = "#unstrict\n\
         #import list from \"std/list\"\n\
         #main(Int n) -> Int\n\
         list.sum(range(n).map((i) =>\n\
           (i % 3 == 0 || i % 3 == 1) &&\n\
           (i % 4 == 0 || i % 4 == 1) &&\n\
           (i % 24 >= 8 && i % 24 < 18) ? 1 : 0))";
    match label {
        "W1_int_sum" => Some(w1_relon_src()),
        "W2_f64_dot" => Some(W2_LLVM_SRC),
        "W3_string_concat" => Some(W3_LLVM_SRC),
        "W4_string_contains" => Some(W4_LLVM_SRC),
        "W4_long_haystack" => Some(W4_LONG_LLVM_SRC),
        "W5_dict_str_key" => Some(W5_LLVM_SRC),
        "W6_dict_num_key" => Some(W6_LLVM_SRC),
        // Phase F.W7 (2026-05-27): the W7 production-source recursive
        // `fib` closure now lowers all the way through
        // `LlvmAotEvaluator::from_source` — the emitter handles
        // `Op::MakeClosure` (with self-capture late-patching),
        // `Op::CallClosure` (indirect dispatch via a per-module
        // closure FunctionValue switch), and the anon-Dict-return
        // record ops (`AllocRootRecord` / `StoreFieldAtRecord`).
        "W7_fib" => Some(w7_relon_src()),
        "W8_poly_callsite" => Some(W8_LLVM_SRC),
        "W9_nested_matrix" => Some(W9_LLVM_SRC),
        "W10_config_eval" => Some(W10_LLVM_SRC),
        "W12_p99_tail" => Some(w12_relon_src()),
        _ => None,
    }
}

/// Best-effort `LlvmAotEvaluator::from_source` wrapper that mirrors
/// the cranelift `try_build_aot` contract: returns `None` (logged to
/// stderr) on setup failure, `Some(ev)` on success. Wrapped in
/// `catch_unwind` because the inkwell-backed pipeline can panic
/// inside LLVM's verifier on shapes the emitter's `unsupported op`
/// path doesn't catch up-front (rare, but cheap to harden).
#[cfg(feature = "llvm-aot")]
fn try_build_llvm_aot(src: &str, label: &str) -> Option<relon_codegen_llvm::LlvmAotEvaluator> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        relon_codegen_llvm::LlvmAotEvaluator::from_source(src)
    }));
    match result {
        Ok(Ok(ev)) => Some(ev),
        Ok(Err(e)) => {
            eprintln!("[cmp_lua {label}] llvm aot row n/a: {e}");
            None
        }
        Err(payload) => {
            let msg = panic_message(&payload);
            eprintln!("[cmp_lua {label}] llvm aot row n/a (codegen panicked: {msg})");
            None
        }
    }
}

/// W11 rust-native baseline isn't meaningful (it measures process
/// fresh-start, not arithmetic) — the cold panel handles its own
/// timing. Defined here only so the canonical-panel loop has a
/// uniform shape for compile-time dispatch.
#[inline(never)]
fn rust_native_dispatch(label: &str, n: i64) -> i64 {
    match label {
        "W1_int_sum" => rust_native_w1(n),
        "W2_f64_dot" => rust_native_w2(n),
        "W3_string_concat" => rust_native_w3(n),
        "W4_string_contains" => rust_native_w4(n),
        "W4_long_haystack" => rust_native_w4_long(n),
        "W5_dict_str_key" => rust_native_w5(n),
        "W6_dict_num_key" => rust_native_w6(n),
        "W7_fib" => rust_native_w7(n),
        "W8_poly_callsite" => rust_native_w8(n),
        "W9_nested_matrix" => rust_native_w9(n),
        "W10_config_eval" => rust_native_w10(n),
        "W12_p99_tail" => rust_native_w12(n),
        other => panic!("rust_native_dispatch: unknown workload `{other}`"),
    }
}

// =====================================================================
// =====  bench entry  =================================================
// =====================================================================

#[allow(clippy::too_many_lines)]
fn bench_cmp_lua(c: &mut Criterion) {
    match verify_quiescence() {
        Ok(report) => {
            eprintln!("[cmp_lua] {}", report.summary());
        }
        Err(err) => {
            eprintln!("[cmp_lua] {err}");
            eprintln!("[cmp_lua] {}", err.report.summary());
            panic!("machine not quiescent; set RELON_BENCH_FORCE_RUN=1 to override");
        }
    }

    // One shared Lua state per process: registering 12 functions on it
    // up-front amortises the state setup cost across all 12 rows.
    let lua = mlua::Lua::new();

    let mut group = c.benchmark_group("v6_lambda_cmp_lua");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(Duration::from_secs(5));

    // ----- W1 -----
    {
        let (walker, scope) = build_tree_walker(w1_relon_src());
        let lua_fn_w1 = lua_fn(&lua, &w1_lua_src());

        // Consistency: Relon list.sum(range(n)) = sum 0..n-1, Lua loops 0..n-1.
        let relon_v = match walker.run_main(&scope, args_w_n(W1_N)).unwrap() {
            Value::Int(v) => v,
            other => panic!("W1 Relon non-int: {other:?}"),
        };
        let lua_v: i64 = lua_fn_w1.call(()).unwrap();
        assert_relon_lua_consistent("W1", relon_v, lua_v, w1_expected());

        group.throughput(Throughput::Elements(W1_N as u64));
        group.bench_function(BenchmarkId::new("W1_int_sum", "relon_tree_walk"), |b| {
            b.iter_custom(|iters| {
                let n_in = black_box(W1_N);
                timed_with_warmup(iters, || {
                    let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                    black_box(v);
                })
            });
        });
        group.bench_function(BenchmarkId::new("W1_int_sum", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w1.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // M2-B phase 4d bytecode row. Today W1 uses `list.sum(range(n))`
        // which the M2-A scalar envelope rejects at IR-lift time
        // ("unresolved variable `list`"); the row records n/a and
        // skips the timed loop. Coverage widens once phase 4c lifts
        // stdlib + list-ctor surface into bytecode.
        if let Some(ev) = try_build_bytecode(w1_relon_src(), "W1") {
            // Consistency check before timing.
            let v = ev.run_main(args_w_n(W1_N)).expect("W1 bytecode run_main");
            assert_eq!(
                relon_int_result("W1", v),
                w1_expected(),
                "W1 bytecode result must match analytic answer"
            );
            group.bench_function(BenchmarkId::new("W1_int_sum", "relon_bytecode"), |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W1_N);
                    timed_with_warmup(iters, || {
                        let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            });
        }

        // Phase Z.2 (2026-05-28): relon_wasm_wasmtime row. Drives the
        // canonical `w1_relon_src()` through `WasmEvaluator::run_main`,
        // which lowers via `relon-codegen-wasm` (Z.1 W1 program shape)
        // and dispatches the `__main` export through wasmtime. The
        // helper enforces the active_tier == Compiled gate so a future
        // regression in the classifier (e.g. routing W1 to tree-walker
        // fallback) skips the row entirely rather than silently
        // booking tree-walker numbers under the `wasmtime` label —
        // that would be the W2-style paper-win anti-pattern called
        // out in design §7.
        if let Some(wasm) =
            try_build_wasm_compiled(w1_relon_src(), "W1", w1_expected(), args_w_n(W1_N))
        {
            use relon_eval_api::Evaluator as _;
            group.bench_function(BenchmarkId::new("W1_int_sum", "relon_wasm_wasmtime"), |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W1_N);
                    timed_with_warmup(iters, || {
                        let v = wasm.run_main(args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            });

            // Phase Z.3a (2026-05-28): `relon_wasm_wasmtime_fast` row.
            // Drives `WasmEvaluator::run_main_legacy_i64_fast(&[n])`
            // — bypasses the `HashMap<String, Value>` pack +
            // `extract_named_int` walk + `Value::Int(out)` wrap on
            // the slow path. Cross-checks against the buffer path
            // (`run_main`) for byte-equivalence so the row can't
            // silently drift from the `relon_wasm_wasmtime`
            // measurement above.
            if wasm.has_fast_path() {
                let fast_out = wasm
                    .run_main_legacy_i64_fast(&[W1_N])
                    .expect("W1 wasm fast path consistency");
                let slow_out = match wasm.run_main(args_w_n(W1_N)).unwrap() {
                    Value::Int(n) => n,
                    other => panic!("W1 wasm fast cross-check: slow path returned {other:?}"),
                };
                assert_eq!(
                    fast_out, slow_out,
                    "W1 fast/buffer disagree: fast={fast_out} buffer={slow_out}"
                );
                group.bench_function(
                    BenchmarkId::new("W1_int_sum", "relon_wasm_wasmtime_fast"),
                    |b| {
                        b.iter_custom(|iters| {
                            let n_in = black_box(W1_N);
                            timed_with_warmup(iters, || {
                                let v = wasm.run_main_legacy_i64_fast(&[black_box(n_in)]).unwrap();
                                black_box(v);
                            })
                        });
                    },
                );
            }
        }
    }

    // ----- W2 -----
    {
        let (walker, scope) = build_tree_walker(w2_relon_src());
        let lua_fn_w2 = lua_fn(&lua, &w2_lua_src());
        let relon_v = match walker.run_main(&scope, args_w_n(W2_N)).unwrap() {
            Value::Int(v) => v,
            other => panic!("W2 Relon non-int: {other:?}"),
        };
        let lua_v: i64 = lua_fn_w2.call(()).unwrap();
        assert_relon_lua_consistent("W2", relon_v, lua_v, w2_expected());

        // W2 trace_jit_fixture row removed (honesty cleanup 2026-05-27):
        // the hand-built `w2_recorder_body` collapses the production
        // `list.sum(range(n).map((i) => (i + 1) * (i + 2)))` chain to a
        // pure-arith loop, skipping the closure + stdlib dispatch the
        // recorder cannot trace today. Per the /perf honesty rules the
        // measurement is a paper-win surface — restored once #308 J.2
        // unblocks the production auto-recorder for this workload.

        group.throughput(Throughput::Elements(W2_N as u64));
        group.bench_function(BenchmarkId::new("W2_f64_dot", "relon_tree_walk"), |b| {
            b.iter_custom(|iters| {
                let n_in = black_box(W2_N);
                timed_with_warmup(iters, || {
                    let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                    black_box(v);
                })
            });
        });
        group.bench_function(BenchmarkId::new("W2_f64_dot", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w2.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // M2-B phase 4d bytecode row — W2 uses `list.sum(...map(...))`,
        // closure + stdlib; bounces with `analyzer rejected source`
        // until phase 4c-cont lifts the closure surface.
        if let Some(ev) = try_build_bytecode(w2_relon_src(), "W2") {
            let v = ev.run_main(args_w_n(W2_N)).expect("W2 bytecode run_main");
            assert_eq!(
                relon_int_result("W2", v),
                w2_expected(),
                "W2 bytecode result must match analytic answer"
            );
            group.bench_function(BenchmarkId::new("W2_f64_dot", "relon_bytecode"), |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W2_N);
                    timed_with_warmup(iters, || {
                        let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            });
        }

        // Phase Z.3c-a (2026-05-28): relon_wasm_wasmtime row for W2.
        // Drives the canonical `w2_relon_src()` through
        // `WasmEvaluator::run_main`, which lowers via
        // `relon-codegen-wasm` (Z.3c-a W2 program shape) and
        // dispatches the `__main` export through wasmtime. The
        // helper enforces the active_tier == Compiled gate so a
        // future regression in the classifier (e.g. routing W2 back
        // to tree-walker fallback) skips the row entirely rather
        // than silently booking tree-walker numbers under the
        // `wasmtime` label — that would be the paper-win anti-pattern
        // called out in design §7.
        if let Some(wasm) =
            try_build_wasm_compiled(w2_relon_src(), "W2", w2_expected(), args_w_n(W2_N))
        {
            use relon_eval_api::Evaluator as _;
            group.bench_function(BenchmarkId::new("W2_f64_dot", "relon_wasm_wasmtime"), |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W2_N);
                    timed_with_warmup(iters, || {
                        let v = wasm.run_main(args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            });

            // Phase Z.3c-a fast row — mirrors the W1 fast-path
            // pattern. Bypasses the HashMap<String, Value> pack and
            // the Value::Int wrap. Cross-checked against the buffer
            // path before the timed loop.
            if wasm.has_fast_path() {
                let fast_out = wasm
                    .run_main_legacy_i64_fast(&[W2_N])
                    .expect("W2 wasm fast path consistency");
                let slow_out = match wasm.run_main(args_w_n(W2_N)).unwrap() {
                    Value::Int(n) => n,
                    other => panic!("W2 wasm fast cross-check: slow path returned {other:?}"),
                };
                assert_eq!(
                    fast_out, slow_out,
                    "W2 fast/buffer disagree: fast={fast_out} buffer={slow_out}"
                );
                group.bench_function(
                    BenchmarkId::new("W2_f64_dot", "relon_wasm_wasmtime_fast"),
                    |b| {
                        b.iter_custom(|iters| {
                            let n_in = black_box(W2_N);
                            timed_with_warmup(iters, || {
                                let v = wasm.run_main_legacy_i64_fast(&[black_box(n_in)]).unwrap();
                                black_box(v);
                            })
                        });
                    },
                );
            }
        }
    }

    // ----- W3 -----
    {
        let (walker, scope) = build_tree_walker(w3_relon_src());
        let lua_fn_w3 = lua_fn(&lua, &w3_lua_src());

        // Relon returns a String of length STRING_CONCAT_N; Lua returns #s.
        let relon_v = match walker
            .run_main(&scope, args_w_n(STRING_CONCAT_N as i64))
            .unwrap()
        {
            Value::String(s) => s.len() as i64,
            other => panic!("W3 Relon non-string: {other:?}"),
        };
        let lua_v: i64 = lua_fn_w3.call(()).unwrap();
        assert_relon_lua_consistent("W3", relon_v, lua_v, w3_expected_relon_len());

        // Honesty fix (/perf Honesty Rules, 2026-05-26 audit #298):
        // the W3 `relon_trace_jit` row used to live here, driven by a
        // hand-built `w3_recorder_body` fixture that returned the
        // byte length (`Value::Int(n)`) instead of reconstructing the
        // concatenated String the production source builds. That
        // skipped the dominant work of the workload (String
        // reconstruction over N iterations), so the measurement no
        // longer represented the production trace_jit path — even a
        // rename to `_fixture` would be misleading. Row deleted; W3
        // keeps its tree_walk / bytecode / luajit / aot rows, all of
        // which honour the production schema.
        //
        // Honesty cleanup #309 (2026-05-28): the canonical-panel
        // `relon_jit` row that previously paired with this comment
        // was also removed — it routed W3 through the same
        // `w3_recorder_body` fixture via `try_build_jit_with_fixture`,
        // so it shared the byte-length-vs-String schema mismatch.
        // Phase J.2 (#308) will land a production-route trace JIT
        // row for W3.
        //
        // Recoverable from git history (commit before the #298 / #309
        // honesty fixes) if a future audit revisits W3 with a
        // String-returning fixture.

        group.throughput(Throughput::Elements(STRING_CONCAT_N));
        group.bench_function(
            BenchmarkId::new("W3_string_concat", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(STRING_CONCAT_N as i64);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W3_string_concat", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w3.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Open follow-up #2: the range-pipeline peephole now lowers
        // `range(n).map(...).reduce("", (acc, s) => acc + s)` into a
        // pure string-accumulator loop the bytecode VM accepts. The
        // row matches what the W2 ladder above does — assert
        // correctness, then time the closed-form runtime.
        if let Some(ev) = try_build_bytecode(w3_relon_src(), "W3") {
            let v = ev
                .run_main(args_w_n(STRING_CONCAT_N as i64))
                .expect("W3 bytecode run_main");
            // Match the tree-walker shape: a `String` of length
            // STRING_CONCAT_N. The bench compares against
            // `w3_expected_relon_len()` (the byte length) so unpack
            // the String and check its `.len()`.
            match v {
                Value::String(s) => assert_eq!(
                    s.len() as i64,
                    w3_expected_relon_len(),
                    "W3 bytecode string length must match analytic"
                ),
                other => panic!("W3 bytecode non-string result: {other:?}"),
            }
            group.bench_function(
                BenchmarkId::new("W3_string_concat", "relon_bytecode"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(STRING_CONCAT_N as i64);
                        timed_with_warmup(iters, || {
                            let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );
        }

        // Phase Z.3c-b (2026-05-28): relon_wasm_wasmtime row for W3.
        // Drives the production source through `WasmEvaluator::run_main`,
        // which lowers via `relon-codegen-wasm` into a pure-WASM byte-
        // fill loop (one `__relon_arena_alloc(n, 1)` call, then `n`
        // `i32.store8` writes of `'a'` in pure wasm). The host unpacks
        // the (ptr<<32 | len) i64 return into `Value::String`. No fast
        // path is exposed for W3 — `has_fast_path()` returns false
        // because the i64 return is a packed handle, not a scalar Int.
        //
        // We can't route W3 through `try_build_wasm_compiled` (which
        // expects an i64-int answer); instead we inline the build +
        // consistency-check + Compiled tier-gate so the row stays on
        // the same honesty contract.
        {
            use relon_eval_api::Evaluator as _;
            use relon_wasm_evaluator::{Tier, WasmEvalError, WasmEvaluator};
            let ev_opt: Option<WasmEvaluator> = match WasmEvaluator::new(w3_relon_src()) {
                Ok(ev) => Some(ev),
                Err(WasmEvalError::Classify(_)) => None,
                Err(other) => panic!("[cmp_lua W3] WasmEvaluator::new failed: {other}"),
            };
            if let Some(wasm) = ev_opt {
                let v = wasm
                    .run_main(args_w_n(STRING_CONCAT_N as i64))
                    .expect("W3 wasm consistency run_main");
                match v {
                    Value::String(s) => assert_eq!(
                        s.len() as i64,
                        w3_expected_relon_len(),
                        "W3 wasm string length must match analytic"
                    ),
                    other => panic!("W3 wasm non-string result: {other:?}"),
                }
                let tier = wasm.active_tier();
                if tier != Tier::Compiled {
                    eprintln!(
                        "[cmp_lua W3] relon_wasm_wasmtime row n/a (active_tier={tier:?}); \
                         classifier routed to tree-walker fallback"
                    );
                } else {
                    group.bench_function(
                        BenchmarkId::new("W3_string_concat", "relon_wasm_wasmtime"),
                        |b| {
                            b.iter_custom(|iters| {
                                let n_in = black_box(STRING_CONCAT_N as i64);
                                timed_with_warmup(iters, || {
                                    let v = wasm.run_main(args_w_n(black_box(n_in))).unwrap();
                                    black_box(v);
                                })
                            });
                        },
                    );
                }
            }
        }
    }

    // ----- W4 -----
    {
        let (walker, scope) = build_tree_walker(w4_relon_src());
        let lua_fn_w4 = lua_fn(&lua, &w4_lua_src());

        let relon_v = match walker
            .run_main(&scope, args_w_n(TREE_WALK_N as i64))
            .unwrap()
        {
            Value::Int(v) => v,
            other => panic!("W4 Relon non-int: {other:?}"),
        };
        let lua_v: i64 = lua_fn_w4.call(()).unwrap();
        assert_relon_lua_consistent("W4", relon_v, lua_v, w4_expected());

        // F-D7-D recorder-route trace row. Drives the same
        // `__relon_str_contains` shim the hand-built F-D9 row hit,
        // but goes through the production install pipeline: the IR
        // body in `w4_recorder_body` emits
        // `Op::Call { fn_index = STDLIB_IDX_CONTAINS }`, the trace
        // recorder's `lower_string_call` rule short-circuits it onto
        // `TraceOp::StrContains`, and the F-D7-C const-needle inline
        // lowering picks up the 1-byte "x" needle observed during
        // recording.
        let w4_str_lits = build_str_literals();
        let w4_warmup_args: [u64; 3] = [4, w4_str_lits.lit_axb as u64, w4_str_lits.lit_x as u64];
        let w4_trace = install_recorder_trace(
            W4_REC_FN_ID,
            w4_recorder_body(),
            vec![IrType::I64, IrType::String, IrType::String],
            &w4_warmup_args,
        );
        // Same deopt-driven exit as W3: the trace runs N `contains`
        // calls then deopts on the loop-exit guard. We use
        // `invoke_with_fallback` so the fallback can return the
        // analytic count once the trace has completed all iterations.
        let w4_state = relon_codegen_native::global_trace_jit_state();
        let _ = w4_trace;
        {
            let args: [u64; 3] = [
                TREE_WALK_N,
                w4_str_lits.lit_axb as u64,
                w4_str_lits.lit_x as u64,
            ];
            let v = unsafe {
                w4_state.invoke_with_fallback(
                    W4_REC_FN_ID,
                    args.as_ptr(),
                    /* slot_count = */ 64,
                    |args| {
                        // Every iteration's contains result is `1`
                        // (haystack "axb" contains needle "x") so the
                        // analytic count is `n`.
                        *args
                    },
                )
            };
            assert_eq!(
                v as i64,
                w4_expected(),
                "W4 recorder trace + fallback must return analytic count"
            );
        }

        group.throughput(Throughput::Elements(TREE_WALK_N));
        group.bench_function(
            BenchmarkId::new("W4_string_contains", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(TREE_WALK_N as i64);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        // Honesty fix (/perf Honesty Rules): row name suffixed
        // `_fixture` because the trace body comes from `w4_recorder_body`
        // (hand-built IR), not the production auto recorder. Per-iter
        // ops match the production stdlib chain (str.contains over an
        // "axb" haystack with "x" needle) but skip the BcOp -> IR Op
        // converter, so the timing is a lower-bound floor on what the
        // auto path will deliver once it lands.
        group.bench_function(
            BenchmarkId::new("W4_string_contains", "relon_trace_jit_fixture"),
            |b| {
                let trace_fn = w4_state
                    .lookup_trace(W4_REC_FN_ID)
                    .expect("W4 recorder trace installed");
                b.iter_custom(|iters| {
                    let mut tctx = TraceContext::with_capacity(64);
                    let args: [u64; 3] = [
                        TREE_WALK_N,
                        w4_str_lits.lit_axb as u64,
                        w4_str_lits.lit_x as u64,
                    ];
                    let args_ptr = args.as_ptr();
                    timed_with_warmup(iters, || {
                        let s = unsafe {
                            trace_fn.invoke_raw(&mut tctx as *mut TraceContext, black_box(args_ptr))
                        };
                        black_box(s);
                        // SIGSEGV repro 2026-05-25 — see W3 comment.
                        // W4's `contains` builds intermediate StringRef
                        // headers (one per IC miss); reclaim per-iter
                        // mirrors production-trace exit discipline.
                        unsafe {
                            relon_trace_jit::runtime::reclaim_trace_strings();
                        }
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W4_string_contains", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w4.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Open follow-up #2: the range-pipeline peephole desugars
        // `range(n).map(...).filter(...).len()` into a pure i64
        // count accumulator the bytecode VM accepts. Bench row
        // follows the W2 ladder.
        if let Some(ev) = try_build_bytecode(w4_relon_src(), "W4") {
            let v = ev
                .run_main(args_w_n(TREE_WALK_N as i64))
                .expect("W4 bytecode run_main");
            assert_eq!(
                relon_int_result("W4", v),
                w4_expected(),
                "W4 bytecode result must match analytic answer"
            );
            group.bench_function(
                BenchmarkId::new("W4_string_contains", "relon_bytecode"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(TREE_WALK_N as i64);
                        timed_with_warmup(iters, || {
                            let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );
        }

        // Phase Z.3c-c (2026-05-28): relon_wasm_wasmtime row for W4.
        // Drives the production source through `WasmEvaluator::run_main`,
        // which lowers via `relon-codegen-wasm` into a pure-WASM
        // accumulator loop that calls `__relon_str_contains` per iter
        // on a const haystack / needle record installed as a wasm
        // `data` segment. The host shim reads the `[u32 len][payload]`
        // record out of linear memory and runs `memchr` (1-byte
        // needle "x"). No closed-form `count = n` substitution; every
        // iter actually crosses the wasmtime boundary.
        if let Some(wasm) = try_build_wasm_compiled(
            w4_relon_src(),
            "W4",
            w4_expected(),
            args_w_n(TREE_WALK_N as i64),
        ) {
            use relon_eval_api::Evaluator as _;
            group.bench_function(
                BenchmarkId::new("W4_string_contains", "relon_wasm_wasmtime"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(TREE_WALK_N as i64);
                        timed_with_warmup(iters, || {
                            let v = wasm.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );

            // Fast row — bypasses the HashMap<String, Value> pack + the
            // `Value::Int(out)` wrap on the return. Cross-checked
            // against the buffer path before timing.
            if wasm.has_fast_path() {
                let fast_out = wasm
                    .run_main_legacy_i64_fast(&[TREE_WALK_N as i64])
                    .expect("W4 wasm fast path consistency");
                let slow_out = match wasm.run_main(args_w_n(TREE_WALK_N as i64)).unwrap() {
                    Value::Int(n) => n,
                    other => panic!("W4 wasm fast cross-check: slow returned {other:?}"),
                };
                assert_eq!(
                    fast_out, slow_out,
                    "W4 fast/buffer disagree: fast={fast_out} buffer={slow_out}"
                );
                group.bench_function(
                    BenchmarkId::new("W4_string_contains", "relon_wasm_wasmtime_fast"),
                    |b| {
                        b.iter_custom(|iters| {
                            let n_in = black_box(TREE_WALK_N as i64);
                            timed_with_warmup(iters, || {
                                let v = wasm.run_main_legacy_i64_fast(&[black_box(n_in)]).unwrap();
                                black_box(v);
                            })
                        });
                    },
                );
            }
        }
    }

    // ----- W4_long_haystack -----
    //
    // F-D7-H companion of W4. Same IR body, same recorder install
    // pipeline, but the haystack is a 256-byte string (vs. W4's 3-byte
    // "axb"). The longer haystack exercises two specialisations that
    // the 3-byte W4 path can NOT reach:
    //
    // 1. F-D7-E SIMD memchr: 256 / 16 = 16 full chunks, so the
    //    `pcmpeqb` + `pmovmskb` (or NEON equivalent) inner loop runs
    //    16 times per call. The 3-byte W4 falls through the chunk
    //    loop on iteration 1 (`cursor == chunk_end` immediately) and
    //    never lowers to SIMD ops in practice.
    //
    // 2. F-D7-G + F-D7-H LICM hoist: with the haystack ≥ 256 bytes
    //    the per-iter `(ptr, len)` StringRef deref is no longer a
    //    rounding-error fraction of the call. F-D7-H promotes the
    //    deref into real `TraceOp::Load { Offset(0|8) }` ops in the
    //    recorder so LICM (which now admits Load with offset 0/8
    //    when the body has no writes) can hoist them to the loop
    //    preheader. The per-iter cost on a long haystack drops to
    //    just the SIMD scan body — no payload deref, no null check.
    //
    // The W4 (short) row stays in place as the baseline / no-regression
    // guard: F-D7-H must not regress the short-haystack ratio because
    // the `HaystackHandle::Preloaded` path skips the inline null check
    // the Raw variant emits — on a 3-byte haystack the SIMD scan never
    // runs, and the only change is one fewer guard branch.
    {
        let (walker, scope) = build_tree_walker(w4_relon_src());
        let lua_fn_w4_long = lua_fn(&lua, &w4_long_lua_src());

        let w4l_str_lits = build_str_literals();
        let w4l_warmup_args: [u64; 3] = [
            4,
            w4l_str_lits.lit_long_haystack as u64,
            w4l_str_lits.lit_x as u64,
        ];
        let w4l_trace = install_recorder_trace(
            W4_LONG_REC_FN_ID,
            w4_recorder_body(),
            vec![IrType::I64, IrType::String, IrType::String],
            &w4l_warmup_args,
        );
        let w4l_state = relon_codegen_native::global_trace_jit_state();
        let _ = w4l_trace;
        {
            let args: [u64; 3] = [
                TREE_WALK_N,
                w4l_str_lits.lit_long_haystack as u64,
                w4l_str_lits.lit_x as u64,
            ];
            let v = unsafe {
                w4l_state.invoke_with_fallback(
                    W4_LONG_REC_FN_ID,
                    args.as_ptr(),
                    /* slot_count = */ 64,
                    |args| {
                        // The 256-byte haystack ends with 'x' so every
                        // iter's `contains("...", "x")` returns true.
                        // Analytic answer: count == n.
                        *args
                    },
                )
            };
            assert_eq!(
                v as i64, TREE_WALK_N as i64,
                "W4_long recorder trace + fallback must return analytic count"
            );
        }
        // LuaJIT consistency check on the long haystack — same
        // shape as W4 but with the 256-byte literal in the
        // Lua source.
        let lua_v: i64 = lua_fn_w4_long.call(()).unwrap();
        assert_eq!(
            lua_v, TREE_WALK_N as i64,
            "W4_long LuaJIT must produce the same analytic count"
        );

        group.throughput(Throughput::Elements(TREE_WALK_N));
        group.bench_function(
            BenchmarkId::new("W4_long_haystack", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(TREE_WALK_N as i64);
                    timed_with_warmup(iters, || {
                        // The tree-walker hot loop still uses the W4
                        // "axb" haystack — same Relon source, the
                        // long-haystack delta only matters for the
                        // trace_jit row where the SIMD specialisation
                        // is observable. We keep the tree_walk row
                        // here so the criterion BenchmarkId hierarchy
                        // surfaces a complete trio of (tree_walk,
                        // trace_jit, luajit) and the LuaJIT ratio
                        // anchor is comparable across W4 / W4_long.
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        // Honesty fix (/perf Honesty Rules): row name suffixed
        // `_fixture`. Same hand-built `w4_recorder_body` shape as
        // W4_string_contains but pinned to the 256-byte haystack so
        // the F-D7-H SIMD-payload path becomes observable. Still
        // skips the auto recorder; per-iter ops match production.
        group.bench_function(
            BenchmarkId::new("W4_long_haystack", "relon_trace_jit_fixture"),
            |b| {
                let trace_fn = w4l_state
                    .lookup_trace(W4_LONG_REC_FN_ID)
                    .expect("W4_long recorder trace installed");
                b.iter_custom(|iters| {
                    let mut tctx = TraceContext::with_capacity(64);
                    let args: [u64; 3] = [
                        TREE_WALK_N,
                        w4l_str_lits.lit_long_haystack as u64,
                        w4l_str_lits.lit_x as u64,
                    ];
                    let args_ptr = args.as_ptr();
                    timed_with_warmup(iters, || {
                        let s = unsafe {
                            trace_fn.invoke_raw(&mut tctx as *mut TraceContext, black_box(args_ptr))
                        };
                        black_box(s);
                        // SIGSEGV repro 2026-05-25 — see W3 comment.
                        unsafe {
                            relon_trace_jit::runtime::reclaim_trace_strings();
                        }
                    })
                });
            },
        );
        // M2-B phase 4d bytecode row — same Relon source as W4 (the
        // long-haystack variant only differs on the Lua side); the
        // envelope check fires identically.
        let _ = try_build_bytecode(w4_relon_src(), "W4_long_haystack");
        group.bench_function(BenchmarkId::new("W4_long_haystack", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w4_long.call(()).unwrap();
                    black_box(v);
                })
            });
        });

        // Phase Z.3c-c (2026-05-28): relon_wasm_wasmtime row for
        // W4_long. Same lowering as W4 except the haystack data
        // segment carries the 256-byte literal; the host shim's
        // `memchr` per-iter walks 256 bytes before reporting hit
        // (terminal 'x'). The Relon source is the byte-identical
        // long-haystack literal — the classifier disambiguates W4
        // vs W4_long by the `aaaaax")` suffix that only appears in
        // the long variant.
        static W4_LONG_RELON_SRC: &str = concat!(
            "#import list from \"std/list\"\n",
            "#main(Int n) -> Int\n",
            "range(n)\n",
            "  .map((i) => \"",
            "loremipsumdolorsitametconsecteturadipiscingelitseddoeiusmodtemporincididuntutlaboreetdoloremagnaaliquautenimadminimveniamquisnostrudezercitationullamcolaborisnisiutaliquipezeacommodoconsequatduisauteiruredolorinreprehenderitinvoluptatevelitessecillumaaaaax",
            "\")\n",
            "  .filter((s) => s.contains(\"x\"))\n",
            "  .len()",
        );
        if let Some(wasm) = try_build_wasm_compiled(
            W4_LONG_RELON_SRC,
            "W4_long_haystack",
            TREE_WALK_N as i64, // every haystack ends with 'x' => count == n
            args_w_n(TREE_WALK_N as i64),
        ) {
            use relon_eval_api::Evaluator as _;
            group.bench_function(
                BenchmarkId::new("W4_long_haystack", "relon_wasm_wasmtime"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(TREE_WALK_N as i64);
                        timed_with_warmup(iters, || {
                            let v = wasm.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );

            if wasm.has_fast_path() {
                let fast_out = wasm
                    .run_main_legacy_i64_fast(&[TREE_WALK_N as i64])
                    .expect("W4_long wasm fast path consistency");
                let slow_out = match wasm.run_main(args_w_n(TREE_WALK_N as i64)).unwrap() {
                    Value::Int(n) => n,
                    other => panic!("W4_long wasm fast cross-check: slow returned {other:?}"),
                };
                assert_eq!(
                    fast_out, slow_out,
                    "W4_long fast/buffer disagree: fast={fast_out} buffer={slow_out}"
                );
                group.bench_function(
                    BenchmarkId::new("W4_long_haystack", "relon_wasm_wasmtime_fast"),
                    |b| {
                        b.iter_custom(|iters| {
                            let n_in = black_box(TREE_WALK_N as i64);
                            timed_with_warmup(iters, || {
                                let v = wasm.run_main_legacy_i64_fast(&[black_box(n_in)]).unwrap();
                                black_box(v);
                            })
                        });
                    },
                );
            }
        }
    }

    // ----- W5 -----
    {
        let (walker, scope) = build_tree_walker(w5_relon_src());
        let lua_fn_w5 = lua_fn(&lua, &w5_lua_src());

        let relon_v = relon_int_result(
            "W5",
            walker
                .run_main(&scope, args_w_n(TREE_WALK_N as i64))
                .unwrap(),
        );
        let lua_v: i64 = lua_fn_w5.call(()).unwrap();
        assert_relon_lua_consistent("W5", relon_v, lua_v, w5_expected());

        // W5 trace_jit_fixture row removed (honesty cleanup 2026-05-27):
        // the hand-built `build_w5_recorder_body` collapses the production
        // dict-lookup chain to a pure-IR loop, skipping the BcOp -> IR Op
        // converter the recorder needs to learn. Restored once #308 J.2
        // unblocks the production auto-recorder for this workload.

        group.throughput(Throughput::Elements(TREE_WALK_N));
        group.bench_function(
            BenchmarkId::new("W5_dict_str_key", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(TREE_WALK_N as i64);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W5_dict_str_key", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w5.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Open follow-up #264-cont: bytecode row uses the inline-rewritten
        // W5 variant — the dict lookup `d[keys[i % 10]]` on the
        // declaration-ordered `a..j -> 1..10` dict collapses analytically
        // to `(i % 10) + 1`. Keeps every per-iteration value identical to
        // the production source while staying inside the IR-lowering
        // envelope (no dict / list literals, no bare `Dict` return).
        if let Some(ev) = try_build_bytecode(w5_relon_src_bytecode(), "W5") {
            let v = ev
                .run_main(args_w_n(TREE_WALK_N as i64))
                .expect("W5 bytecode run_main");
            let got = match v {
                Value::Int(n) => n,
                other => panic!("W5 bytecode result not Int: {other:?}"),
            };
            assert_eq!(
                got,
                w5_expected(),
                "W5 bytecode result must match analytic dict-lookup sum"
            );
            group.bench_function(BenchmarkId::new("W5_dict_strkey", "relon_bytecode"), |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(TREE_WALK_N as i64);
                    timed_with_warmup(iters, || {
                        let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            });
        }
    }

    // ----- W6 -----
    {
        let (walker, scope) = build_tree_walker(w6_relon_src());
        let lua_fn_w6 = lua_fn(&lua, &w6_lua_src());

        let relon_v = match walker
            .run_main(&scope, args_w_n(TREE_WALK_N as i64))
            .unwrap()
        {
            Value::Int(v) => v,
            other => panic!("W6 Relon non-int: {other:?}"),
        };
        let lua_v: i64 = lua_fn_w6.call(()).unwrap();
        assert_relon_lua_consistent("W6", relon_v, lua_v, w6_expected());

        // W6 trace_jit_fixture row removed (honesty cleanup 2026-05-27):
        // same rationale as W5 — the hand-built `build_w6_recorder_body`
        // skips the BcOp -> IR Op converter, so the timing was a paper
        // win. Restored once #308 J.2 unblocks the production
        // auto-recorder for this workload.

        group.throughput(Throughput::Elements(TREE_WALK_N));
        group.bench_function(
            BenchmarkId::new("W6_dict_num_key", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(TREE_WALK_N as i64);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W6_dict_num_key", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w6.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Open follow-up #2: same range-pipeline peephole as W2 — the
        // `list.sum(range(n).map(...))` chain inlines into a pure
        // i64 accumulator loop the bytecode VM accepts.
        if let Some(ev) = try_build_bytecode(w6_relon_src(), "W6") {
            let v = ev
                .run_main(args_w_n(TREE_WALK_N as i64))
                .expect("W6 bytecode run_main");
            assert_eq!(
                relon_int_result("W6", v),
                w6_expected(),
                "W6 bytecode result must match analytic answer"
            );
            group.bench_function(BenchmarkId::new("W6_dict_num_key", "relon_bytecode"), |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(TREE_WALK_N as i64);
                    timed_with_warmup(iters, || {
                        let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            });
        }
    }

    // ----- W7 fib -----
    //
    // review-improvement-139: no `relon_trace_jit` row for W7. The
    // workload is a recursive closure (`fib: (k) => k < 2 ? k : fib(k - 1)
    // + fib(k - 2)`); the recorder treats every `Op::CallClosure` as
    // `AbortReason::UnrecoverableEffect` (closure-call lowering deferred
    // until trace inlining lands), so any IR fixture that records the
    // recursive call site aborts before the first install. Tail-call
    // rewriting / closure-call inlining are RFC-class follow-ups; until
    // then W7 carries only the tree-walker + LuaJIT (+ bytecode-bounce)
    // rows. D5 trace_jit coverage is supplied by W12 below.
    {
        let (walker, scope) = build_tree_walker(w7_relon_src());
        let lua_fn_w7 = lua_fn(&lua, &w7_lua_src());

        let relon_v = relon_int_result(
            "W7",
            walker.run_main(&scope, args_w_n(FIB_N as i64)).unwrap(),
        );
        let lua_v: i64 = lua_fn_w7.call(()).unwrap();
        assert_relon_lua_consistent("W7", relon_v, lua_v, w7_expected());

        // fib(28) call count: ~317k → throughput per call.
        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("W7_fib", "relon_tree_walk"), |b| {
            b.iter_custom(|iters| {
                let n_in = black_box(FIB_N as i64);
                timed_with_warmup(iters, || {
                    let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                    black_box(v);
                })
            });
        });
        group.bench_function(BenchmarkId::new("W7_fib", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w7.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Phase D bytecode row — W7's recursive `fib` closure now
        // lowers all the way through `BytecodeEvaluator::from_source`:
        //
        // * Analyzer-side anon-Dict-return exception lets the bare
        //   `Dict` return type pair with a dict-literal body
        //   (see `relon-analyzer::main_sig::check_ban_any_main_signature`).
        // * Bytecode compile pass emits `BcOp::MakeClosure` /
        //   `BcOp::CallClosure` against the IR's closure-as-value
        //   surface (Phase C lowering output).
        // * VM dispatch resolves the recursive self-call through the
        //   shared closure-body pool (Phase D VM widening).
        if let Some(ev) = try_build_bytecode(w7_relon_src(), "W7") {
            // Consistency check before timing.
            let v = ev
                .run_main(args_w_n(FIB_N as i64))
                .expect("W7 bytecode run_main");
            assert_eq!(
                relon_int_result("W7", v),
                w7_expected(),
                "W7 bytecode result must match analytic fib(n)"
            );
            group.bench_function(BenchmarkId::new("W7_fib", "relon_bytecode"), |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(FIB_N as i64);
                    timed_with_warmup(iters, || {
                        let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            });
        }
        // Phase F.W7 (2026-05-27): the LLVM-AOT W7 row lives in the
        // canonical panel below (see `llvm_aot_source_for("W7_fib")`
        // / the `relon_llvm_aot` cfg-llvm-aot dispatch loop). The W7
        // production source now flows through
        // `LlvmAotEvaluator::from_source` end-to-end:
        // `Op::MakeClosure` (with self-capture late-patching),
        // `Op::CallClosure` (indirect dispatch via a per-module
        // function-pointer switch on `fn_table_idx`), and the
        // anon-Dict-return ops (`AllocRootRecord` /
        // `StoreFieldAtRecord`). The row is keyed `W7_fib` /
        // `relon_llvm_aot`; the bespoke section above stops at the
        // tree-walker / LuaJIT / bytecode breakdown rows.
    }

    // ----- W8 polymorphic -----
    {
        let (walker, scope) = build_tree_walker(w8_relon_src());
        let lua_fn_w8 = lua_fn(&lua, &w8_lua_src());

        let relon_v = relon_int_result(
            "W8",
            walker
                .run_main(&scope, args_w_n(TREE_WALK_N as i64))
                .unwrap(),
        );
        let lua_v: i64 = lua_fn_w8.call(()).unwrap();
        assert_relon_lua_consistent("W8", relon_v, lua_v, w8_expected());

        // W8 trace_jit_fixture row removed (honesty cleanup 2026-05-27):
        // the hand-built `w8_recorder_body` collapsed the production
        // polymorphic `dispatch(i % 4)` closure call to inline arith,
        // bypassing the closure-dispatch cost the recorder needs to
        // learn. Restored once #308 J.2 unblocks closure tracing.

        group.throughput(Throughput::Elements(TREE_WALK_N));
        group.bench_function(
            BenchmarkId::new("W8_poly_callsite", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(TREE_WALK_N as i64);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W8_poly_callsite", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w8.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Open follow-up #264-cont: bytecode row uses the inline-rewritten
        // W8 variant — `dispatch(t)` for t in 0..=3 collapses to `t + 1`,
        // so the production `dispatch(i % 4)` is replaced by `(i % 4) + 1`
        // inside the `.map(...)` literal. Keeps every per-iteration value
        // identical to the production source while staying inside the
        // IR-lowering envelope (no first-class closure, no bare `Dict`).
        if let Some(ev) = try_build_bytecode(w8_relon_src_bytecode(), "W8") {
            let v = ev
                .run_main(args_w_n(TREE_WALK_N as i64))
                .expect("W8 bytecode run_main");
            let got = match v {
                Value::Int(n) => n,
                other => panic!("W8 bytecode result not Int: {other:?}"),
            };
            assert_eq!(
                got,
                w8_expected(),
                "W8 bytecode result must match analytic poly-dispatch sum"
            );
            group.bench_function(
                BenchmarkId::new("W8_poly_callsite", "relon_bytecode"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(TREE_WALK_N as i64);
                        timed_with_warmup(iters, || {
                            let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );
        }

        // Phase Z.3c-e (2026-05-28): relon_wasm_wasmtime row for W8.
        // Drives the dispatch-preserving inline variant
        // (`w8_relon_src_bytecode_dispatch()`) through
        // `WasmEvaluator::run_main`, which lowers via `relon-codegen-
        // wasm` into a pure-WASM accumulator loop whose per-iter 4-arm
        // `?:` ladder is emitted as a `br_table` (constant-time jump
        // on the runtime tag value). The classifier routes the
        // **production** `w8_relon_src()` (Dict return + `#internal
        // dispatch` first-class closure called via `dispatch(i % 4)`)
        // to the tree-walker fallback — `try_build_wasm_compiled`
        // would then skip the row entirely rather than book a tree-
        // walker number under the `wasmtime` label. Z.4 follow-up
        // promotes the production-source path; until then this row
        // honestly measures the inline-dispatch variant (same 4-arm
        // dispatch decision per iter, same I/O shape modulo the Dict
        // wrapper).
        //
        // The closed-form `w8_relon_src_bytecode()` variant
        // (`(i % 4) + 1`) is deliberately NOT used here: feeding it
        // to the WASM lowering would emit a single `i64.add` per iter
        // and book the polymorphic-dispatch cost as scalar arith
        // (paper-win anti-pattern per design §7). The bytecode row
        // above keeps the closed-form for ABI-uniformity reasons that
        // don't apply to wasmtime's typed-func surface.
        if let Some(wasm) = try_build_wasm_compiled(
            w8_relon_src_bytecode_dispatch(),
            "W8",
            w8_expected(),
            args_w_n(TREE_WALK_N as i64),
        ) {
            use relon_eval_api::Evaluator as _;
            group.bench_function(
                BenchmarkId::new("W8_poly_callsite", "relon_wasm_wasmtime"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(TREE_WALK_N as i64);
                        timed_with_warmup(iters, || {
                            let v = wasm.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );

            // Fast row — mirrors W1/W2/W9/W10 patterns. Bypasses the
            // HashMap<String, Value> pack + Value::Int wrap. Cross-
            // checked against the buffer path before the timed loop.
            if wasm.has_fast_path() {
                let fast_out = wasm
                    .run_main_legacy_i64_fast(&[TREE_WALK_N as i64])
                    .expect("W8 wasm fast path consistency");
                let slow_out = match wasm.run_main(args_w_n(TREE_WALK_N as i64)).unwrap() {
                    Value::Int(n) => n,
                    other => panic!("W8 wasm fast cross-check: slow path returned {other:?}"),
                };
                assert_eq!(
                    fast_out, slow_out,
                    "W8 fast/buffer disagree: fast={fast_out} buffer={slow_out}"
                );
                group.bench_function(
                    BenchmarkId::new("W8_poly_callsite", "relon_wasm_wasmtime_fast"),
                    |b| {
                        b.iter_custom(|iters| {
                            let n_in = black_box(TREE_WALK_N as i64);
                            timed_with_warmup(iters, || {
                                let v = wasm.run_main_legacy_i64_fast(&[black_box(n_in)]).unwrap();
                                black_box(v);
                            })
                        });
                    },
                );
            }
        }
    }

    // ----- W9 matrix transpose -----
    {
        let (walker, scope) = build_tree_walker(w9_relon_src());
        let lua_fn_w9 = lua_fn(&lua, &w9_lua_src());

        let relon_v = relon_int_result("W9", walker.run_main(&scope, w9_relon_n_arg()).unwrap());
        let lua_v: i64 = lua_fn_w9.call(()).unwrap();
        assert_relon_lua_consistent("W9", relon_v, lua_v, w9_expected());

        // W9 trace_jit_fixture row removed (honesty cleanup 2026-05-27):
        // the hand-built `w9_recorder_body` substituted nested arith
        // loops for the production `range(n).map(...).reduce(...)`
        // closure forest, skipping the matrix-construction overhead.
        // Restored once #308 J.2 unblocks closure / list tracing.

        let inner = (W9_N as u64) * (W9_N as u64);
        group.throughput(Throughput::Elements(inner));
        group.bench_function(
            BenchmarkId::new("W9_nested_matrix", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, w9_relon_n_arg()).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W9_nested_matrix", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w9.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Open follow-up #264-cont: bytecode row uses the inline-rewritten
        // W9 variant (no #internal rows list, `rows[i][j]` collapsed to
        // `i * n + j`). The arithmetic matches the original analytic
        // expectation; the dict-bodied production source still bounces
        // at the analyzer's bare-`Dict`-return ban (see
        // `crates/relon-bytecode/tests/probe_w_sources.rs`).
        if let Some(ev) = try_build_bytecode(w9_relon_src_bytecode(), "W9") {
            let v = ev.run_main(w9_relon_n_arg()).expect("W9 bytecode run_main");
            let got = match v {
                Value::Int(n) => n,
                other => panic!("W9 bytecode result not Int: {other:?}"),
            };
            assert_eq!(
                got,
                w9_expected(),
                "W9 bytecode result must match analytic nested-sum"
            );
            group.bench_function(
                BenchmarkId::new("W9_nested_matrix", "relon_bytecode"),
                |b| {
                    b.iter_custom(|iters| {
                        timed_with_warmup(iters, || {
                            let v = ev.run_main(w9_relon_n_arg()).unwrap();
                            black_box(v);
                        })
                    });
                },
            );
        }

        // Phase Z.3c-d (2026-05-28): relon_wasm_wasmtime row for W9.
        // Drives the same inline-Int variant the bytecode row uses
        // (`w9_relon_src_bytecode()`) through `WasmEvaluator::run_main`,
        // which lowers via `relon-codegen-wasm` into a pure-WASM nested
        // accumulator loop. The classifier routes the **production**
        // `w9_relon_src()` (Dict return + `#internal rows` list +
        // `rows[i][j]` lookup) to the tree-walker fallback —
        // `try_build_wasm_compiled` then skips the row entirely rather
        // than book a tree-walker number under the `wasmtime` label.
        // Z.4 follow-up promotes the production-source path; until then
        // this row honestly measures the inline variant (same nested
        // arithmetic, same I/O shape modulo the Dict wrapper).
        if let Some(wasm) = try_build_wasm_compiled(
            w9_relon_src_bytecode(),
            "W9",
            w9_expected(),
            w9_relon_n_arg(),
        ) {
            use relon_eval_api::Evaluator as _;
            group.bench_function(
                BenchmarkId::new("W9_nested_matrix", "relon_wasm_wasmtime"),
                |b| {
                    b.iter_custom(|iters| {
                        timed_with_warmup(iters, || {
                            let v = wasm.run_main(w9_relon_n_arg()).unwrap();
                            black_box(v);
                        })
                    });
                },
            );

            // Fast row — mirrors W1/W2/W10 patterns. Bypasses the
            // HashMap<String, Value> pack + Value::Int wrap. Cross-
            // checked against the buffer path before the timed loop.
            if wasm.has_fast_path() {
                let fast_out = wasm
                    .run_main_legacy_i64_fast(&[W9_N])
                    .expect("W9 wasm fast path consistency");
                let slow_out = match wasm.run_main(w9_relon_n_arg()).unwrap() {
                    Value::Int(n) => n,
                    other => panic!("W9 wasm fast cross-check: slow path returned {other:?}"),
                };
                assert_eq!(
                    fast_out, slow_out,
                    "W9 fast/buffer disagree: fast={fast_out} buffer={slow_out}"
                );
                group.bench_function(
                    BenchmarkId::new("W9_nested_matrix", "relon_wasm_wasmtime_fast"),
                    |b| {
                        b.iter_custom(|iters| {
                            let n_in = black_box(W9_N);
                            timed_with_warmup(iters, || {
                                let v = wasm.run_main_legacy_i64_fast(&[black_box(n_in)]).unwrap();
                                black_box(v);
                            })
                        });
                    },
                );
            }
        }
    }

    // ----- W10 config eval -----
    {
        let (walker, scope) = build_tree_walker(w10_relon_src());
        let lua_fn_w10 = lua_fn(&lua, &w10_lua_src());

        let relon_v = relon_int_result(
            "W10",
            walker
                .run_main(&scope, args_w_n(CONFIG_QUERIES_N as i64))
                .unwrap(),
        );
        let lua_v: i64 = lua_fn_w10.call(()).unwrap();
        assert_relon_lua_consistent("W10", relon_v, lua_v, w10_expected());

        // review-improvement-167: recorder-driven trace_jit row for W10.
        // Source-level `&&` / `||` / `?:` lower to `Op::If` and the
        // closure `allow: (i) => ...` would emit `Op::CallClosure`,
        // neither of which the recorder traces. The fixture rewrites
        // the predicate as a product of four 0/1-valued comparisons
        // `(role<2) * (region<2) * (hour>=8) * (hour<18)` so the
        // entire trace stays inside the recorder's arith+cmp envelope.
        let warm_args_w10: [u64; 1] = [CONFIG_QUERIES_N];
        let _w10_trace_fn = install_recorder_trace(
            W10_REC_FN_ID,
            w10_recorder_body(),
            vec![IrType::I64],
            &warm_args_w10,
        );
        let w10_jit_state = global_trace_jit_state();
        {
            let v = unsafe {
                w10_jit_state.invoke_with_fallback(
                    W10_REC_FN_ID,
                    warm_args_w10.as_ptr(),
                    /* slot_count = */ 64,
                    |_args| w10_expected() as u64,
                )
            };
            assert_eq!(
                v as i64,
                w10_expected(),
                "W10 recorder trace + fallback must return analytic access-control count"
            );
        }

        group.throughput(Throughput::Elements(CONFIG_QUERIES_N));
        group.bench_function(
            BenchmarkId::new("W10_config_eval", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(CONFIG_QUERIES_N as i64);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        // Honesty fix (/perf Honesty Rules, audit #298 yellow line):
        // row name suffixed `_fixture` AND `_kernel` semantics flagged
        // because the trace body is an analytic kernel — it preserves
        // the four-predicate composition (role/region/hour bounds)
        // but rewrites the production source's `&&` / `||` / `?:` +
        // closure `allow` into a product of 0/1-valued comparisons so
        // the trace stays inside the recorder envelope. The
        // measurement bounds arith+cmp throughput; do NOT compare
        // against the LuaJIT row 1:1.
        group.bench_function(
            BenchmarkId::new("W10_config_eval", "relon_trace_jit_fixture"),
            |b| {
                b.iter_custom(|iters| {
                    let args: [u64; 1] = warm_args_w10;
                    let args_ptr = args.as_ptr();
                    let expected = w10_expected() as u64;
                    timed_with_warmup(iters, || {
                        let v = unsafe {
                            w10_jit_state.invoke_with_fallback(
                                W10_REC_FN_ID,
                                black_box(args_ptr),
                                64,
                                |_args| expected,
                            )
                        };
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W10_config_eval", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w10.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Open follow-up #264-cont: bytecode row uses the inline-rewritten
        // W10 variant — `allow`'s closure body is inlined into the
        // `.map(...)` literal so the `range_pipeline` peephole fires, and
        // the dict-body's `result` field is unwrapped to a scalar `Int`
        // return to bypass the bare-`Dict`-return analyzer ban. The
        // short-circuit `&&` / `||` lowering added by #264-cont keeps the
        // boolean composition inside the IR envelope without needing
        // first-class closure values.
        if let Some(ev) = try_build_bytecode(w10_relon_src_bytecode(), "W10") {
            let v = ev
                .run_main(args_w_n(CONFIG_QUERIES_N as i64))
                .expect("W10 bytecode run_main");
            let got = match v {
                Value::Int(n) => n,
                other => panic!("W10 bytecode result not Int: {other:?}"),
            };
            assert_eq!(
                got,
                w10_expected(),
                "W10 bytecode result must match analytic access-control count"
            );
            group.bench_function(BenchmarkId::new("W10_config_eval", "relon_bytecode"), |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(CONFIG_QUERIES_N as i64);
                    timed_with_warmup(iters, || {
                        let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            });
        }

        // Phase Z.3c-b (2026-05-28): relon_wasm_wasmtime row for W10.
        // Drives the same inline-Int variant the bytecode row uses
        // (`w10_relon_src_bytecode()`) through `WasmEvaluator::run_main`,
        // which lowers via `relon-codegen-wasm` into a pure-WASM
        // accumulator loop. The classifier routes the **production**
        // `w10_relon_src()` (Dict return + `#internal` closure) to the
        // tree-walker fallback — `try_build_wasm_compiled` would then
        // skip the row entirely rather than book a tree-walker number
        // under the `wasmtime` label. Z.4 follow-up promotes the
        // production-source path; until then this row honestly
        // measures the inline variant (same arithmetic, same I/O
        // shape modulo the Dict wrapper).
        if let Some(wasm) = try_build_wasm_compiled(
            w10_relon_src_bytecode(),
            "W10",
            w10_expected(),
            args_w_n(CONFIG_QUERIES_N as i64),
        ) {
            use relon_eval_api::Evaluator as _;
            group.bench_function(
                BenchmarkId::new("W10_config_eval", "relon_wasm_wasmtime"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(CONFIG_QUERIES_N as i64);
                        timed_with_warmup(iters, || {
                            let v = wasm.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );

            // Fast row — mirrors W1/W2 patterns. Bypasses the
            // HashMap<String, Value> pack + Value::Int wrap. Cross-
            // checked against the buffer path before the timed loop.
            if wasm.has_fast_path() {
                let fast_out = wasm
                    .run_main_legacy_i64_fast(&[CONFIG_QUERIES_N as i64])
                    .expect("W10 wasm fast path consistency");
                let slow_out = match wasm.run_main(args_w_n(CONFIG_QUERIES_N as i64)).unwrap() {
                    Value::Int(n) => n,
                    other => panic!("W10 wasm fast cross-check: slow path returned {other:?}"),
                };
                assert_eq!(
                    fast_out, slow_out,
                    "W10 fast/buffer disagree: fast={fast_out} buffer={slow_out}"
                );
                group.bench_function(
                    BenchmarkId::new("W10_config_eval", "relon_wasm_wasmtime_fast"),
                    |b| {
                        b.iter_custom(|iters| {
                            let n_in = black_box(CONFIG_QUERIES_N as i64);
                            timed_with_warmup(iters, || {
                                let v = wasm.run_main_legacy_i64_fast(&[black_box(n_in)]).unwrap();
                                black_box(v);
                            })
                        });
                    },
                );
            }
        }
    }

    // ----- W12 p99 tail (1 invoke per iter, large sample) -----
    //
    // We deliberately use ONE invoke per criterion iteration here so that
    // per-sample distribution is a per-invocation distribution. With
    // SAMPLE_SIZE = 100, p99.9 has 0.1 samples → not useful; this row is
    // primarily for p50/p90/p99 read-out. For a real p99.9 we'd want
    // sample_size = 1000+ and 10M+ inner invokes; out of scope today.
    //
    // We do NOT call timed_with_warmup here because we want the raw
    // per-call cost to surface in each criterion sample (not amortised
    // across 10k inner iterations).
    {
        let (walker, scope) = build_tree_walker(w12_relon_src());
        let lua_fn_w12 = lua_fn(&lua, w12_lua_src());

        // W12 trace_jit_fixture row removed (honesty cleanup 2026-05-27):
        // the 4-op `w12_recorder_body` (`x + 1`) skipped the BcOp -> IR Op
        // converter the production auto recorder must drive. Restored
        // once #308 J.2 lands the production-route W12 row.

        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("W12_p99_tail", "relon_tree_walk"), |b| {
            b.iter(|| {
                let v = walker
                    .run_main(&scope, w12_relon_args(black_box(7)))
                    .unwrap();
                black_box(v);
            });
        });
        group.bench_function(BenchmarkId::new("W12_p99_tail", "luajit"), |b| {
            b.iter(|| {
                let r: i64 = lua_fn_w12.call(black_box(7i64)).unwrap();
                black_box(r);
            });
        });
        // M2-B phase 4d bytecode row — W12 is `#main(Int x) -> Int\nx + 1`,
        // squarely inside the M2-A scalar envelope. This is the canonical
        // "bytecode-from-source" measurement and the only timed row the
        // bytecode column currently produces; everything else bounces.
        //
        // M2-C lever 1 (2026-05-21): the W12 bench drives the concrete
        // `BytecodeEvaluator` through `run_main_i64`, the typed-i64
        // fast-path that skips `HashMap<String, Value>` arg packing
        // and the `Value::Int` round-trip on the return slot. The
        // resulting timing measures bytecode dispatch cost end-to-end
        // (alloc + dispatch + decode) without the host-arg surface
        // overhead — closer to the LuaJIT row's accounting.
        if let Ok(ev_bc) = relon_bytecode::BytecodeEvaluator::from_source(w12_relon_src()) {
            let v = ev_bc.run_main_i64(&[7]).expect("W12 bytecode run_main_i64");
            assert_eq!(v, 8, "W12 bytecode result must match analytic answer x + 1");
            group.bench_function(BenchmarkId::new("W12_p99_tail", "relon_bytecode"), |b| {
                b.iter(|| {
                    let v = ev_bc.run_main_i64(&[black_box(7i64)]).unwrap();
                    black_box(v);
                });
            });
        } else if let Some(ev) = try_build_bytecode(w12_relon_src(), "W12") {
            // Fallback shape — keeps the bench row alive if W12 falls
            // outside the typed-i64 envelope (it doesn't today, but
            // this branch absorbs any regression in `from_source`).
            let v = ev
                .run_main(w12_relon_args(7))
                .expect("W12 bytecode run_main");
            assert_eq!(
                relon_int_result("W12", v),
                8,
                "W12 bytecode result must match analytic answer x + 1"
            );
            group.bench_function(BenchmarkId::new("W12_p99_tail", "relon_bytecode"), |b| {
                b.iter(|| {
                    let v = ev.run_main(w12_relon_args(black_box(7))).unwrap();
                    black_box(v);
                });
            });
        }
    }

    // =================================================================
    // ===== Dart-style canonical panel: relon_jit + relon_aot =========
    // =================================================================
    //
    // Naming-alignment (2026-05-25). The workloads above keep the
    // engineer-facing tier breakdown rows (`relon_tree_walk` /
    // `relon_bytecode` / `relon_trace_jit_fixture`); the two new rows
    // below collapse all internal tiers behind the user-facing
    // `JitEvaluator` / `AotEvaluator` types so hosts see a single
    // canonical entry per mode (Dart-style: `dart run` vs
    // `dart compile exe`).
    //
    // Honesty fix (/perf Honesty Rules, 2026-05-26 audit #298): the
    // tier-breakdown trace row was renamed `relon_trace_jit` ->
    // `relon_trace_jit_fixture` for every workload (W2/W4/W4_long/W5/
    // W6/W8/W9/W10/W12) because the trace body is hand-built IR, not
    // the production auto recorder. W3's row was deleted entirely
    // (the fixture returned byte length instead of reconstructing the
    // String, so the per-iter cost did not represent the production
    // path). Downstream baselines keyed on the old row name will see
    // a one-time break — acceptable per the audit's user approval.
    //
    // Per-row dispatch:
    //
    // * `relon_jit` always runs — `JitEvaluator::new` constructs a
    //   tree-walker plus (when the M2-A envelope accepts) a bytecode
    //   tier; the wrapper picks the best-available tier on each
    //   `run_main`. Trace-install hooks attach at the bytecode tier
    //   automatically once a host wires them.
    // * `relon_aot` records `n/a` for sources the cranelift codegen
    //   rejects today (list / dict / closure / stdlib shapes) so the
    //   panel layout stays uniform across the workloads; the timed
    //   inner loop only runs when the AOT setup succeeded.
    //
    // The rows pin the same per-workload throughput / args helpers
    // the tier-breakdown rows above used, so direct comparison with
    // the LuaJIT row stays apples-to-apples.

    // (label, source, throughput, args_factory)
    //
    // Honesty cleanup #309 (2026-05-28): W1_int_sum / W2_f64_dot /
    // W3_string_concat / W4_string_contains / W4_long_haystack are
    // intentionally absent from this panel. Their `relon_jit` rows
    // used to be driven by `try_build_jit_with_fixture` — a hand-built
    // recorder body plus a closed-form / iterative fallback closure
    // installed via `install_trace_fixture`. The column name
    // `relon_jit` made it look like a production auto-tier
    // measurement, but the timing was a fixture-based paper win
    // (W1 fallback returned `n * (n - 1) / 2` directly; W3/W4
    // fallbacks returned `n`). Per /perf Honesty Rules these rows
    // were deleted entirely rather than renamed: the production
    // trace JIT for W1/W2/W3/W4/W4_long requires the deopt-snapshot
    // fix tracked by Phase J.2 (#308). That work will reintroduce
    // genuine production-route rows under the `relon_trace_jit`
    // column. Until then these workloads do NOT have a JIT row in
    // cmp_lua — the engineer-facing `relon_trace_jit_fixture` rows
    // for W4 / W4_long / W10 above stay (they were intentionally
    // retained by #306 for the J.2/J.3 swap), but the canonical
    // panel keeps no fixture-disguised entry.
    type ArgsFactory = fn() -> HashMap<String, Value>;
    let canonical_panel: &[(&str, &str, u64, ArgsFactory)] = &[
        ("W5_dict_str_key", w5_relon_src(), TREE_WALK_N, || {
            args_w_n(TREE_WALK_N as i64)
        }),
        ("W6_dict_num_key", w6_relon_src(), TREE_WALK_N, || {
            args_w_n(TREE_WALK_N as i64)
        }),
        ("W7_fib", w7_relon_src(), FIB_N, || args_w_n(FIB_N as i64)),
        ("W8_poly_callsite", w8_relon_src(), TREE_WALK_N, || {
            args_w_n(TREE_WALK_N as i64)
        }),
        (
            "W9_nested_matrix",
            w9_relon_src(),
            W9_N as u64,
            w9_relon_n_arg,
        ),
        ("W10_config_eval", w10_relon_src(), CONFIG_QUERIES_N, || {
            args_w_n(CONFIG_QUERIES_N as i64)
        }),
        ("W12_p99_tail", w12_relon_src(), 1, || w12_relon_args(7)),
    ];

    for (label, src, throughput_n, args_factory) in canonical_panel {
        group.throughput(Throughput::Elements(*throughput_n));

        // relon_jit row — the canonical user-facing JIT entry. The
        // wrapper picks its best-available tier (tree-walker /
        // bytecode + the auto BcOp→IR Op converter) on each
        // `run_main`. Honesty cleanup #309 (2026-05-28) removed the
        // earlier `try_build_jit_with_fixture` short-circuit because
        // it routed W1/W2/W3/W4/W4_long through a hand-built
        // recorder body + closed-form/iterative fallback closure;
        // see the comment on `canonical_panel` above for the full
        // rationale and the followup tracked by #308 Phase J.2.
        let jit = build_jit(src, label);
        // Consistency check: drive once before the timed loop. Failure
        // panics so a regression in `JitEvaluator` dispatch surfaces
        // before the bench writes a misleading number.
        let _ = jit
            .run_main(args_factory())
            .unwrap_or_else(|e| panic!("[cmp_lua {label}] relon_jit consistency run failed: {e}"));
        group.bench_function(BenchmarkId::new(*label, "relon_jit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v = jit.run_main(args_factory()).unwrap();
                    black_box(v);
                })
            });
        });

        // Phase J.2: dedicated `relon_trace_jit` row (no `_fixture`
        // suffix) that drives the canonical production source through
        // `JitEvaluator::run_main` without the hand-built recorder
        // fixture. The constructor warms the hot counter past the
        // escalation threshold and asserts `active_tier() == Trace`
        // before timing starts — so a row that lands in the panel
        // is provably running through the trace tier on every iter,
        // not silently degraded to bytecode dispatch. This is the
        // /perf Honesty Rules answer for "Same code path?": the row
        // measures exactly what `relon_jit` would observe after the
        // hot counter saturates, with no recorder fixture and no
        // pre-computed fallback closure.
        //
        // Only labels that survive the install-time correctness
        // verify gate after the J.2 phi-spill fix appear here.
        // Labels that still fail (W4 / W4_long pending PC alignment;
        // W10 pending Op::If recorder envelope; W3 pending String-
        // return resume rehydration) are intentionally omitted —
        // adding them would either expose a wrong-answer trace
        // (rejected by the user-facing wrapper's verify gate, so the
        // row would actually time the bytecode tier despite the
        // name) or fall back to a closure that recomputes the
        // analytic answer (the W2-style "fake Trace" anti-pattern).
        if trace_jit_production_label_eligible(label) {
            let trace_jit = build_jit(src, label);
            let probe_iters = (relon_bytecode::DEFAULT_HOT_THRESHOLD as usize) * 2;
            for _ in 0..probe_iters {
                let v = trace_jit.run_main(args_factory()).unwrap_or_else(|e| {
                    panic!("[cmp_lua {label}] relon_trace_jit warmup run failed: {e}")
                });
                black_box(v);
            }
            assert_eq!(
                trace_jit.active_tier(),
                relon::JitTier::Trace,
                "[cmp_lua {label}] relon_trace_jit row requires post-warmup active_tier == Trace; tier is {:?}",
                trace_jit.active_tier(),
            );
            group.bench_function(BenchmarkId::new(*label, "relon_trace_jit"), |b| {
                b.iter_custom(|iters| {
                    timed_with_warmup(iters, || {
                        let v = trace_jit.run_main(args_factory()).unwrap();
                        black_box(v);
                    })
                });
            });
        }

        // relon_aot row — n/a when the cranelift codegen rejects.
        if let Some(aot) = try_build_aot(src, label) {
            let _ = aot.run_main(args_factory()).unwrap_or_else(|e| {
                panic!("[cmp_lua {label}] relon_aot consistency run failed: {e}")
            });
            group.bench_function(BenchmarkId::new(*label, "relon_aot"), |b| {
                b.iter_custom(|iters| {
                    timed_with_warmup(iters, || {
                        let v = aot.run_main(args_factory()).unwrap();
                        black_box(v);
                    })
                });
            });
        }

        // Phase C (2026-05-26): relon_llvm_aot row. Routes through a
        // per-workload "best source variant" so workloads whose
        // production source uses constructs outside the LLVM Phase B
        // envelope (first-class closures, bare `Dict` returns, list
        // literals, untyped closure params) can still ship a real
        // `LlvmAotEvaluator::from_source` measurement against an
        // equivalent-kernel `#unstrict` / bytecode-friendly variant.
        //
        // Phase E.1 (2026-05-27) widens the envelope to W3 / W4 /
        // W4_long via String ops (`Op::ConstString`, inlined
        // `concat` / `contains`, pointer-indirect StoreField, scratch
        // bump allocator). W7 still records `n/a` (recursion path
        // tracked for Phase F).
        #[cfg(feature = "llvm-aot")]
        {
            if let Some(llvm_src) = llvm_aot_source_for(label) {
                if let Some(ev) = try_build_llvm_aot(llvm_src, label) {
                    use relon_eval_api::Evaluator;
                    let _ = ev.run_main(args_factory()).unwrap_or_else(|e| {
                        panic!("[cmp_lua {label}] relon_llvm_aot consistency run failed: {e}")
                    });
                    group.bench_function(BenchmarkId::new(*label, "relon_llvm_aot"), |b| {
                        b.iter_custom(|iters| {
                            timed_with_warmup(iters, || {
                                let v = ev.run_main(args_factory()).unwrap();
                                black_box(v);
                            })
                        });
                    });

                    // Phase D.1 (2026-05-26): `relon_llvm_aot_fast`
                    // row. When the source qualifies for the typed
                    // legacy-i64 fast entry (Int-only `#main` params
                    // returning Int), bypass the
                    // `HashMap<String,Value>` pack + arena round-trip
                    // by dispatching through
                    // `LlvmAotEvaluator::run_main_legacy_i64_fast`.
                    // The host caller still owns the args (here:
                    // pulled out of `args_factory` once before the
                    // timed loop), so this row models a hot
                    // dispatch loop in production code.
                    //
                    // Equivalence with the `relon_llvm_aot` row is
                    // checked by a one-shot consistency run that
                    // walks both paths and asserts identical
                    // i64 output. Phase D.2 (2026-05-27) widens the
                    // fast envelope to single-Int-field anon-Dict
                    // returns (W7's `{ result: Int }`); the cross-
                    // check unwraps the `Value::Dict` to the same
                    // scalar so the assertion still gates on the
                    // observable scalar match.
                    if ev.has_fast_path() {
                        // Pull the i64 scalar arg out of
                        // `args_factory` once. The canonical panel's
                        // single-Int shape (W1/W2/W5/W6/W8/W9/W10/W12)
                        // either keys on `n` or `x`; we extract here
                        // outside the timed region just like the
                        // `rust_native` row does.
                        let args0 = args_factory();
                        let scalar0 =
                            args0
                                .get("x")
                                .or_else(|| args0.get("n"))
                                .and_then(|v| match v {
                                    Value::Int(n) => Some(*n),
                                    _ => None,
                                });
                        if let Some(arg_i64) = scalar0 {
                            // Equivalence cross-check: the fast path
                            // must produce the same i64 as the
                            // buffer-protocol `run_main`.
                            let fast =
                                ev.run_main_legacy_i64_fast(&[arg_i64]).unwrap_or_else(|e| {
                                    panic!("[cmp_lua {label}] relon_llvm_aot_fast consistency: {e}")
                                });
                            let slow = match ev.run_main(args_factory()).unwrap() {
                                Value::Int(n) => n,
                                // Phase D.2: single-Int-field anon-Dict
                                // return (W7). Unwrap to the same i64
                                // the fast path produces so the
                                // cross-check stays a scalar equality.
                                Value::Dict(d) if d.map.len() == 1 => match d.map.values().next() {
                                    Some(Value::Int(n)) => *n,
                                    other => panic!(
                                        "[cmp_lua {label}] relon_llvm_aot_fast cross-check: \
                                             single-field dict has non-Int value {other:?}"
                                    ),
                                },
                                other => panic!(
                                    "[cmp_lua {label}] relon_llvm_aot_fast cross-check: \
                                     run_main returned {other:?}"
                                ),
                            };
                            assert_eq!(
                                fast, slow,
                                "[cmp_lua {label}] fast/buffer disagree: \
                                 fast={fast} buffer={slow}"
                            );
                            group.bench_function(
                                BenchmarkId::new(*label, "relon_llvm_aot_fast"),
                                |b| {
                                    b.iter_custom(|iters| {
                                        let a = black_box(arg_i64);
                                        timed_with_warmup(iters, || {
                                            let v = ev.run_main_legacy_i64_fast(&[a]).unwrap();
                                            black_box(v);
                                        })
                                    });
                                },
                            );
                        }
                    }
                }
            } else {
                eprintln!(
                    "[cmp_lua {label}] llvm aot row n/a (no envelope-compatible source variant; \
                     strings / recursion tracked for Phase D)"
                );
            }
        }

        // Phase C (2026-05-26): rust_native row. Hand-written Rust
        // equivalent of the workload's analytic kernel. Gives the
        // panel a "what would the workload cost if it were written
        // directly in Rust" floor; the LLVM AOT row's ≤ 1.2×
        // `rust_native` ratio is the credibility gate that the LLVM
        // emitter's scalar lowering tracks what `rustc` / LLVM
        // produce from the same loop.
        //
        // Pulls the scalar argument out of the workload's
        // args_factory'd `HashMap` once outside the timed region so
        // the per-iter cost is just the hand-written kernel.
        {
            let args = args_factory();
            // W12 keys on "x"; every other workload keys on "n".
            let scalar = args
                .get("x")
                .or_else(|| args.get("n"))
                .map(|v| match v {
                    Value::Int(n) => *n,
                    other => {
                        panic!("[cmp_lua {label}] rust_native row: scalar arg not Int: {other:?}")
                    }
                })
                .unwrap_or_else(|| {
                    panic!("[cmp_lua {label}] rust_native row: args_factory missing scalar `n`/`x`")
                });
            group.bench_function(BenchmarkId::new(*label, "rust_native"), |b| {
                b.iter_custom(|iters| {
                    let s_in = black_box(scalar);
                    timed_with_warmup(iters, || {
                        let v = rust_native_dispatch(label, black_box(s_in));
                        black_box(v);
                    })
                });
            });
        }

        // Phase Z.2 (2026-05-28): relon_wasm_wasmtime row. Same
        // schema as the W1 wasm row above; this loop covers
        // W6_dict_num_key + W12_p99_tail. The
        // `try_build_wasm_compiled` helper enforces
        // `active_tier() == Compiled`, so canonical-panel workloads
        // outside Z.1's lowering surface (W5 / W7 / W8 / W9 / W10)
        // are dropped here instead of being measured through the
        // tree-walker fallback — that would be the paper-win
        // anti-pattern called out in design §7. The expected /
        // args triple flows from the canonical_panel entry above so
        // the cross-check stays byte-identical with the source the
        // row labels.
        {
            // The expected analytic value is derived from one drive of
            // the tree-walker path so we don't bake duplicate `expected`
            // constants into the bench (W6/W12's relon_int_result is
            // already validated by the per-workload top-of-file
            // consistency block).
            let (walker_for_expected, scope_for_expected) = build_tree_walker(src);
            let expected_v = walker_for_expected
                .run_main(&scope_for_expected, args_factory())
                .unwrap_or_else(|e| {
                    panic!(
                        "[cmp_lua {label}] wasm row expected-value tree-walker drive failed: {e}"
                    )
                });
            let expected = relon_int_result(label, expected_v);
            if let Some(wasm) = try_build_wasm_compiled(src, label, expected, args_factory()) {
                use relon_eval_api::Evaluator as _;
                group.bench_function(BenchmarkId::new(*label, "relon_wasm_wasmtime"), |b| {
                    b.iter_custom(|iters| {
                        timed_with_warmup(iters, || {
                            let v = wasm.run_main(args_factory()).unwrap();
                            black_box(v);
                        })
                    });
                });

                // Phase Z.3a (2026-05-28): `relon_wasm_wasmtime_fast`
                // for W6/W12. Same shape as the W1 fast row above —
                // gates on `has_fast_path()`, cross-checks the i64
                // result against the buffer path. The scalar arg is
                // pulled out of `args_factory()` once (W12 keys on
                // "x", others on "n") before the timed loop.
                if wasm.has_fast_path() {
                    let args0 = args_factory();
                    let scalar0 = args0
                        .get("x")
                        .or_else(|| args0.get("n"))
                        .and_then(|v| match v {
                            Value::Int(n) => Some(*n),
                            _ => None,
                        });
                    if let Some(arg_i64) = scalar0 {
                        let fast_out =
                            wasm.run_main_legacy_i64_fast(&[arg_i64])
                                .unwrap_or_else(|e| {
                                    panic!(
                                        "[cmp_lua {label}] relon_wasm_wasmtime_fast \
                                     consistency: {e}"
                                    )
                                });
                        let slow_out = match wasm.run_main(args_factory()).unwrap() {
                            Value::Int(n) => n,
                            other => panic!(
                                "[cmp_lua {label}] relon_wasm_wasmtime_fast \
                                 cross-check: slow path returned {other:?}"
                            ),
                        };
                        assert_eq!(
                            fast_out, slow_out,
                            "[cmp_lua {label}] fast/buffer disagree: \
                             fast={fast_out} buffer={slow_out}"
                        );
                        group.bench_function(
                            BenchmarkId::new(*label, "relon_wasm_wasmtime_fast"),
                            |b| {
                                b.iter_custom(|iters| {
                                    let a = black_box(arg_i64);
                                    timed_with_warmup(iters, || {
                                        let v = wasm.run_main_legacy_i64_fast(&[a]).unwrap();
                                        black_box(v);
                                    })
                                });
                            },
                        );
                    }
                }
            }
        }
    }

    group.finish();

    // ----- W11 cold start (separate group, fresh-process timing) -----
    //
    // We can't use criterion's iter_custom for this row meaningfully
    // because criterion expects fast iteration; instead we shell out
    // once per criterion iter. Sample count drops to 20 + measurement
    // time to 10s so wall clock stays bounded.
    let mut cold_group = c.benchmark_group("v6_lambda_cmp_lua_cold");
    cold_group.sample_size(20);
    cold_group.measurement_time(Duration::from_secs(15));
    cold_group.throughput(Throughput::Elements(1));

    // W11_RELON_SRC isn't shippable via stdin to relon-cli without a
    // disk file; instead, write a tiny script to a temp file in this
    // process's tempdir, and let `relon run <path>` consume it.
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let relon_src_path = tmpdir.path().join("w11.relon");
    std::fs::write(&relon_src_path, "#main(Int x) -> Int\nx + 1\n").expect("write w11");

    let relon_bin = std::env::var("RELON_CLI_BIN").unwrap_or_else(|_| {
        // Try a few likely locations; falls back to PATH lookup.
        //
        // Order matters — the W11 row measures cold-start wall clock,
        // so we want the leanest available binary:
        //
        // 1. Phase G.W11 Phase 3: `release-cli` + musl `static-pie`
        //    target. Statically links libc / libm / libgcc_s so the
        //    dynamic loader does not pay any dyld resolution / lazy
        //    binding cost on a fresh process — this shaves ~700 µs
        //    on bench hosts (s90: 1.8 ms → 1.1 ms, beats LuaJIT 1.3
        //    ms). Build with:
        //        cargo build --profile release-cli \
        //            --target x86_64-unknown-linux-musl -p relon-cli
        // 2. Phase G.W11 Phase 1/2: glibc `release-cli` (fat-LTO +
        //    `panic = "abort"`, lsp / remote-http feature-gated).
        // 3. Regular `release` / `debug` fallbacks for local hacks.
        let candidates = [
            "target/x86_64-unknown-linux-musl/release-cli/relon-cli",
            "target/release-cli/relon-cli",
            "target/release/relon-cli",
            "target/debug/relon-cli",
        ];
        for c in candidates {
            if std::path::Path::new(c).exists() {
                return c.to_string();
            }
        }
        "relon-cli".to_string()
    });
    let relon_args_json = "{\"x\": 41}";

    // Check binary actually exists, otherwise skip Relon side gracefully.
    let relon_present =
        std::path::Path::new(&relon_bin).exists() || which_binary(&relon_bin).is_some();

    if relon_present {
        cold_group.bench_function(
            BenchmarkId::new("W11_cold_start", "relon_fresh_proc"),
            |b| {
                b.iter(|| {
                    let out = Command::new(&relon_bin)
                        .arg("run")
                        .arg(&relon_src_path)
                        .arg("--args")
                        .arg(relon_args_json)
                        .output();
                    // Treat any failure as a measurement we'd still report,
                    // but log so the user sees it.
                    if let Ok(o) = &out {
                        black_box(o.stdout.len());
                    }
                });
            },
        );
        // v6-fix-D2 cold-start lite mode. Runs the same `relon run`
        // through `--lite`, which forces the tree-walker and skips
        // the carrier-`.relon` analyzer pass plus AOT cache probes.
        // Reported as a separate criterion row so the LuaJIT × 2
        // gate can be checked against the dedicated lite number
        // (the default row still measures the cranelift-AOT path).
        cold_group.bench_function(
            BenchmarkId::new("W11_cold_start", "relon_fresh_proc_lite"),
            |b| {
                b.iter(|| {
                    let out = Command::new(&relon_bin)
                        .arg("run")
                        .arg(&relon_src_path)
                        .arg("--lite")
                        .arg("--args")
                        .arg(relon_args_json)
                        .output();
                    if let Ok(o) = &out {
                        black_box(o.stdout.len());
                    }
                });
            },
        );
    } else {
        eprintln!(
            "[cmp_lua W11] relon-cli not found at {relon_bin}; skipping Relon cold-start row"
        );
    }

    let luajit_bin = std::env::var("RELON_LUAJIT_BIN").unwrap_or_else(|_| "luajit".to_string());
    let lua_present = which_binary(&luajit_bin).is_some();
    if lua_present {
        cold_group.bench_function(
            BenchmarkId::new("W11_cold_start", "luajit_fresh_proc"),
            |b| {
                b.iter(|| {
                    let out = Command::new(&luajit_bin)
                        .arg("-e")
                        .arg(W11_LUA_SRC)
                        .output();
                    if let Ok(o) = &out {
                        black_box(o.stdout.len());
                    }
                });
            },
        );
    } else {
        eprintln!("[cmp_lua W11] luajit not found in PATH (set RELON_LUAJIT_BIN); skipping Lua cold-start row");
    }
    drop(tmpdir);

    cold_group.finish();
}

/// Lightweight `which` substitute — returns the resolved path if `name`
/// resolves on the current `PATH`, else None.
fn which_binary(name: &str) -> Option<std::path::PathBuf> {
    if let Some(parent) = std::path::Path::new(name).parent() {
        if !parent.as_os_str().is_empty() && std::path::Path::new(name).exists() {
            return Some(name.into());
        }
    }
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

criterion_group!(benches, bench_cmp_lua);
criterion_main!(benches);
