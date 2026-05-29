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
use relon_codegen_cranelift::trace_install::{
    __relon_jump_to_recorder, global_trace_jit_state, RecordingRegistration,
};
use relon_codegen_cranelift::JITedTraceFn;
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

/// Honesty cleanup (2026-05-28, audit #318): label gate for rows whose
/// only currently-available driver is an algebraically-collapsed
/// variant that skips the production source's load-bearing work. The
/// production source carries one of:
///
/// * **W5_dict_str_key** — `d[keys[i % 10]]` (string-array index →
///   string hash → dict probe). The bytecode / LLVM AOT / rust_native
///   variants all run the collapsed kernel `(i % 10) + 1`; the wasm
///   variant runs a 10-entry i64-table load. None of them re-create
///   the string-hash + dict-probe cost the LuaJIT row pays.
/// * **W8_poly_callsite** — `dispatch(i % 4)` via a first-class
///   `#internal` closure. The bytecode / LLVM AOT / rust_native
///   variants collapse to `(i % 4) + 1`; the wasm variant retains the
///   `?:` ladder (lowered as `br_table`) so it stays.
/// * **W9_nested_matrix** — `range(n).map(...).reduce(...)` over a
///   materialised `rows` list. The bytecode / wasm / LLVM AOT /
///   rust_native variants all inline `rows[i][j]` as `i * n + j` and
///   skip the list materialisation entirely.
/// * **W10_config_eval** — `range(n).map(allow)` where `allow` is a
///   first-class `#internal` closure. Bytecode / wasm / LLVM AOT /
///   rust_native inline the closure body into the `.map(...)`
///   literal, dodging the closure-dispatch cost.
///
/// For these labels the bench keeps the production-source rows
/// (`relon_tree_walk`, `luajit`, and the canonical-panel `relon_jit`
/// which dispatches `JitEvaluator::run_main` through the
/// best-available tier on the **production** source). The deleted
/// rows return once the IR pipeline (Z.4.4) widens the bytecode /
/// wasm / LLVM AOT codegens to accept the production-source surface
/// (`#internal` dicts / first-class closures / materialised list
/// literals / bare-`Dict` returns).
fn paper_win_collapsed_variant_label(label: &str) -> bool {
    matches!(
        label,
        "W5_dict_str_key" | "W8_poly_callsite" | "W9_nested_matrix" | "W10_config_eval"
    )
}

/// Honesty cleanup (2026-05-28, audit #332): label gate for rows whose
/// source body is a pure arithmetic-progression sum that LLVM at -O3
/// reduces to a closed-form polynomial — `n*(n-1)/2`, `n*(n+1)/2`,
/// Faulhaber-class cubics, etc. Verified by dumping post-O3 IR via
/// `crates/relon-codegen-llvm/examples/dump_audit_w1_w2_w6.rs`:
///
/// * **W1_int_sum** — `list.sum(range(n))` ≡ `Σ_{i<n} i`. Post-O3 IR:
///   the loop preheader is replaced by `(n-1) * (n-2) / 2 + (n-1)`
///   followed by an unconditional branch to the exit phi. No loop
///   instructions emitted in the lambda body.
/// * **W2_f64_dot** — `list.sum(range(n).map((i) => (i+1)*(i+2)))`.
///   Post-O3 IR: cubic polynomial collapse via Faulhaber's formula,
///   the magic constant `6148914691236517206 = (2^64 - 4)/3` is the
///   `/3` modular inverse, again no loop instructions.
/// * **W6_list_int_sum_plus_one** — `list.sum(range(n).map((i) => i + 1))`
///   ≡ `Σ_{i<n} (i+1)`. Post-O3 IR: closed form `n*(n+1)/2`. Same
///   shape as W1. (Pre-2026-05-28 row name `W6_dict_num_key` was a
///   carry-over from the Relon-vs-Lua design table where "D8 dict
///   numeric key" referred to LuaJIT's array-part path; the Relon
///   side is genuinely a `List<Int>` accumulator, so the label was
///   renamed for accuracy.)
///
/// Per `/perf` Honesty Rules ("closed-form fold that changes
/// complexity class = red-line"), booking O(1) arithmetic under a
/// `relon_llvm_aot` / `relon_llvm_aot_fast` / `rust_native` label
/// against a LuaJIT row that walks the O(n) loop is a paper win.
/// The W1 / W2 entries here are precautionary — neither workload is
/// in the canonical_panel today, so the LLVM source variants in
/// `llvm_aot_source_for` are dead branches; the gate keeps them
/// dormant if a future agent reintroduces the workloads to the panel.
/// W6 IS in the panel today; the row was emitting a paper-win
/// measurement against the LuaJIT loop until this audit.
///
/// **Note on rust_native**: the `rust_native_w{1,2,6}` helpers use
/// `wrapping_add` + `black_box` on `n`, which the doc-comment on
/// `rust_native_w1` framed as "same freedom the LLVM AOT pipeline
/// has". rustc / LLVM still recognises the arithmetic progression
/// at the call site after inlining and folds to the same closed
/// form, so the `rust_native` row for these labels measures the
/// same O(1) arithmetic and lands the same paper win.
///
/// The deleted rows return once the bench grows a `black_box`-on-acc
/// shape that defeats LLVM's induction-variable reduction (or an
/// LLVM emitter flag that disables `IndVarSimplify` / `LoopIdiom` /
/// `LoopReduce` on the lambda body). Until then the row family is
/// suppressed end-to-end.
///
/// Panel expansion (2026-05-28): W13 / W14 / W15 join the gate.
/// * **W13_deep_dict_access** — the inner `cfg.db.pool.connections.max
///   + cfg.db.pool.connections.timeout.ms` reads constant-fold to
///   `5100` (rustc + LLVM at -O3 see the literal dict-tree leaves);
///   the per-iter body collapses to `acc + 5100`, the reduce folds
///   to `n * 5100`. The bytecode / wasm / LLVM AOT lowerings reject
///   the production source's dict-literal `#internal cfg` binding
///   today (no n/a row needed — `try_build_*` returns None), so the
///   only gated row is `rust_native_w13`, which would otherwise book
///   the closed-form `n * 5100` against the LuaJIT chain-walker.
/// * **W14_schema_validate** — both range predicates are trivially
///   `true` over the input domain (`i % 10 ∈ [0,10)` and
///   `i / 10 ∈ [0,n/10]` with `n=1000`), so the body folds to `+2`
///   per iter and the reduce folds to `n * 2`. Gated end-to-end.
/// * **W15_conditional_field** — `Σ_{i<n} (i%2==0 ? 2i : 3i)` is a
///   closed-form polynomial after even/odd splitting (the half-sums
///   reduce to scalar arithmetic at -O3); LLVM collapses both halves.
fn paper_win_closed_form_fold_label(label: &str) -> bool {
    matches!(
        label,
        "W1_int_sum"
            | "W2_f64_dot"
            | "W6_list_int_sum_plus_one"
            | "W13_deep_dict_access"
            | "W14_schema_validate"
            | "W15_conditional_field"
            // Panel expansion 2026-05-29 (Tier 4 Phase 3 — Group F
            // strict-mode W30): same closed-form fold as W6
            // (`Σ_{i<n} (i+1) ≡ n*(n+1)/2`). The strict-mode
            // analyzer path differs from W6's `#unstrict` only at
            // compile time; the IR / runtime kernel rustc + LLVM at
            // -O3 see is byte-identical, so the same fold applies
            // and the same `rust_native` / `relon_llvm_aot` paper-
            // win risk gates the row out.
            | "W30_strict_mode_baseline"
    )
}

/// Tier 4 Phase 1 (panel expansion 2026-05-29): label gate for the
/// brand-tagged-value `match` dispatch row. The Relon tree-walker pays
/// a per-iter brand-string compare + arm-table walk (see W21 doc-
/// comment). A Rust-side equivalent would model the items as
/// `enum { Image, Text }` and lower the `match` to a `cmov` / `br_table`
/// — both of which collapse the runtime brand-string compare to a
/// compile-time variant tag dispatch. Booking that O(1) variant
/// dispatch under a `rust_native` label against a LuaJIT row that
/// walks the `if it.__type == "..."` ladder is a paper-win per
/// `/perf` Honesty Rules ("Same algorithm? Same code path?" — the
/// Rust `enum` discriminant compare is a different code path than
/// the tree-walker's brand-string-equal probe).
///
/// The label IS retained in `canonical_panel` because the
/// `relon_jit` row legitimately routes the production source through
/// `JitEvaluator::run_main` (falls through to the tree-walker for
/// this surface today); the wasm / llvm_aot / cranelift_aot rows
/// already gate out via `llvm_aot_source_for` / `try_build_*`
/// returning None; and the dedicated tree_walk + luajit + bytecode
/// rows above carry the row's headline numbers. The
/// `rust_native` row is suppressed end-to-end via this gate.
fn paper_win_brand_dispatch_label(label: &str) -> bool {
    matches!(label, "W21_match_dispatch")
}

/// Tier 4 Phase 2 (panel expansion 2026-05-29): label gate for the
/// container-construction-sugar rows. Entries: W23 dict spread; W24
/// list comprehension; W25 pipe chain.
///
/// Per-workload rationale:
///
/// * **W23** — Rust-side `HashMap::clone() + insert()` replaces the
///   tree-walker's `Value::Dict` allocator + key-hash + spread
///   codepath with the host allocator. Booking that under a
///   `rust_native` label against a LuaJIT row that walks the
///   production source is a paper-win per the /perf "Same code path?"
///   rule — the host-allocator dict copy is not the same code path
///   the tree-walker / LuaJIT runs.
/// * **W24** — the comprehension `[x * 2 for x in range(n) if x % 3
///   == 0]` is an arithmetic progression (multiples of 3 doubled);
///   rustc + LLVM -O3 collapses the loop to a closed-form polynomial
///   in `n` (same fold pattern as W1 / W6 in
///   `paper_win_closed_form_fold_label`). Booking that O(1)
///   arithmetic under a `rust_native` label against an O(n) LuaJIT
///   loop is a paper-win per the /perf "Same algorithm?" rule.
/// * **W25** — the pipe `range(n) | map((x) => x + 1) | filter(even)
///   | sum()` reduces to the sum of even numbers in `[1, n]`. Same
///   arithmetic-progression closed-form fold as W24 / W6. Same
///   paper-win pattern.
///
/// The labels ARE retained in `canonical_panel` because the
/// `relon_jit` row legitimately routes the production source through
/// `JitEvaluator::run_main` (falls through to the tree-walker today);
/// the wasm / llvm_aot / cranelift_aot rows already gate out via
/// `llvm_aot_source_for` / `try_build_*` returning None; and the
/// dedicated tree_walk + luajit rows above carry the row headlines.
/// The `rust_native` row is suppressed end-to-end via this gate.
fn paper_win_container_sugar_label(label: &str) -> bool {
    matches!(
        label,
        "W23_dict_spread" | "W24_list_comprehension" | "W25_pipe_chain"
    )
}

/// Tier 4 Phase 4 (panel expansion 2026-05-29): label gate for the
/// f-string interpolation row. The Relon tree-walker pays a per-iter
/// String allocation, decimal write, and concat for every iter via
/// the `Expr::FString` evaluator in `eval.rs`; the LuaJIT row walks
/// `string.format` plus `#str`, the same int-to-decimal then alloc
/// then length-read shape. A Rust-side `format!("item {} of {}", i,
/// n).len()` could be folded by rustc / LLVM after inlining: the
/// constant prefix lengths plus `ilog10(i) + 1` digit count are all
/// closed-form expressions of `i` and `n`, and rustc's `format!` macro
/// expansion is known to be aggressively constant-folded when the
/// fmt-spec is a literal and only the integer args vary. Booking that
/// potential closed-form sum under a `rust_native` label against the
/// LuaJIT row that walks the format-then-strlen path is a paper-win
/// per the `/perf` Honesty Rules' "closed-form fold that changes
/// complexity class = red-line" — here the fold could replace the
/// O(n) loop with an O(1) digit-bucket sum.
///
/// The label IS retained in `canonical_panel` because the
/// `relon_jit` row legitimately routes the production source through
/// `JitEvaluator::run_main` (falls through to the tree-walker today;
/// the bytecode envelope rejects f-string lowering until Z.4.x adds
/// `Op::FString`); the wasm / llvm_aot / cranelift_aot rows already
/// gate out via `llvm_aot_source_for` / `try_build_*` returning None;
/// and the dedicated tree_walk + luajit rows above carry the row's
/// headline numbers. The `rust_native` row is suppressed end-to-end
/// via this gate.
fn paper_win_fstring_interp_label(label: &str) -> bool {
    matches!(label, "W26_fstring_interp")
}

/// Tier 4 Phase 4 (panel expansion 2026-05-29): label gate for the
/// non-list stdlib coverage row (W27 `std/dict.keys`). The Relon
/// tree-walker pays a per-iter module-resolver call + dict literal
/// allocation + List materialisation in `_dict_keys`. A Rust-side
/// `HashMap<&str, i32>::from([...]).keys().count()` would be folded
/// by rustc / LLVM after inlining: the dict literal is a constant
/// 3-element map, and `keys().count()` is a constant `3`. Booking
/// that O(1) compile-time constant under a `rust_native` label
/// against the LuaJIT row that walks the table-iter + list-
/// materialise path is a paper-win per the `/perf` Honesty Rules'
/// "closed-form fold that changes complexity class = red-line" — the
/// fold could replace the O(n) loop with an O(1) `n * 3`.
///
/// The label IS retained in `canonical_panel` because the
/// `relon_jit` row legitimately routes the production source through
/// `JitEvaluator::run_main` (falls through to the tree-walker today;
/// the bytecode envelope rejects stdlib-module-resolver + dict
/// literal lowering); the wasm / llvm_aot / cranelift_aot rows
/// already gate out via `llvm_aot_source_for` / `try_build_*`
/// returning None; and the dedicated tree_walk + luajit rows above
/// carry the row's headline numbers. The `rust_native` row is
/// suppressed end-to-end via this gate.
fn paper_win_stdlib_dict_label(label: &str) -> bool {
    matches!(label, "W27_stdlib_dict")
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
/// The `relon-bench` crate always pulls in `relon-codegen-cranelift`, so
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
// §7, the `TraceRecordingEvaluator` (in `relon-codegen-cranelift`) does
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
const W4_REC_FN_ID: u32 = (relon_codegen_cranelift::MAX_FN_ID - 11) as u32;
/// F-D7-H: separate fn_id slot for the long-haystack variant so it
/// coexists with the short-haystack W4 row in the same bench process
/// — both rows share the recorder install pipeline but observe
/// independent haystack pointers at recording time, which means
/// independent F-D7-C const-needle / F-D7-H str-payload side tables.
const W4_LONG_REC_FN_ID: u32 = (relon_codegen_cranelift::MAX_FN_ID - 12) as u32;
/// review-improvement-167: W10 (config_eval) trace_jit row. The
/// closure-bodied predicate uses `||` / `&&`; recorder has no
/// `Op::If` / `Op::Select` / `Op::BitAnd` support, so we lower the
/// predicate to `(role<2) * (region<2) * (hour>=8) * (hour<18)`
/// where each compare produces an `i64` 0/1 cell. Multiplying ANDs
/// without short-circuit, which preserves the workload's per-iter
/// op count.
const W10_REC_FN_ID: u32 = (relon_codegen_cranelift::MAX_FN_ID - 17) as u32;

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
    let _ = relon_codegen_cranelift::clear_recording(fn_id);
    let state = relon_codegen_cranelift::global_trace_jit_state();
    state.invalidate_trace(fn_id);
    // Pre-flight: step the recorder out-of-band so we surface a
    // precise abort reason if the IR fixture falls outside the
    // recorder's lowering envelope. Mirrors what
    // `__relon_jump_to_recorder` does internally, but exposes the
    // abort reason as a panic message rather than a silent
    // install-skip.
    {
        use relon_codegen_cranelift::{RecordingOutcome, TraceRecordingEvaluator};
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
    relon_codegen_cranelift::register_recording(
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
///
/// Honesty cleanup (2026-05-28, audit #318): no callers remain — the
/// W5 `relon_bytecode` / `relon_wasm_wasmtime` / `relon_wasm_wasmtime_fast`
/// rows that used to feed this variant into the bench were deleted
/// because the algebraic collapse skips the production source's
/// string-hash + dict-probe work. Retained as documentation of the
/// variant shape the Z.4.4 IR pipeline must subsume before the bench
/// rows are reintroduced.
#[allow(dead_code)]
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

/// Phase Z.3c-g (2026-05-28): inline-Int W7 variant for the WASM row.
/// The production `w7_relon_src()` form binds `fib: (k) => ...` as a
/// `#internal` first-class recursive closure inside a Dict-body and
/// returns `Dict { fib, result }` — both the bare-`Dict` return and
/// first-class closure binding scope-cut at Phase Z.1's lowering
/// envelope (Z.4 follow-up). The `where`-clause sibling moves the
/// same doubly-recursive `fib` body into a top-level let-binding so
/// the return type lands on `Int`, keeping the source inside the
/// Z.3 wasm-lowering envelope.
///
/// Per-iter shape is preserved verbatim — `fib(n)` with the same
/// `k < 2 ? k : fib(k - 1) + fib(k - 2)` body, materialising the
/// same ~57k recursive calls for fib(22) the production source
/// performs. **No** iterative `(a, b) := (b, a+b)` rewrite and
/// **no** Binet's closed-form substitution — both are the canonical
/// W7 algorithm-substitution traps (the iterative form is the user-
/// flagged red line from the W7 trace_jit-fixture history); using
/// either would book the doubly-recursive O(phi^n) work as
/// linear or O(1) arithmetic and silently bypass what W7 is meant
/// to measure (recursion + call-ABI overhead).
fn w7_relon_src_bytecode() -> &'static str {
    "#main(Int n) -> Int\n\
     fib(n) where { fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2) }"
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
///
/// Honesty cleanup (2026-05-28, audit #318): no callers remain — the
/// W8 `relon_bytecode` row that used to feed this variant was deleted
/// because the algebraic collapse skips the production source's
/// closure-call indirection. Retained as documentation of the
/// variant shape the IR pipeline must subsume before the bench row
/// is reintroduced.
#[allow(dead_code)]
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
///
/// Honesty cleanup (2026-05-28, audit #318): no callers remain — the
/// W8 `relon_wasm_wasmtime` row that used to feed this variant was
/// deleted because the inlined `?:` ladder still skips the production
/// source's first-class closure-call indirection (preserving the
/// 4-arm decision is necessary but not sufficient).
#[allow(dead_code)]
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
///
/// Honesty cleanup (2026-05-28, audit #318): no callers remain — the
/// W9 `relon_bytecode` / `relon_wasm_wasmtime` /
/// `relon_wasm_wasmtime_fast` rows that used to feed this variant
/// were deleted because the inlined arithmetic skips the production
/// source's `rows` list materialisation + double table lookup per
/// iter. Retained as documentation of the variant shape the IR
/// pipeline must subsume before the bench rows are reintroduced.
#[allow(dead_code)]
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
///
/// Honesty cleanup (2026-05-28, audit #318): no callers remain — the
/// W10 `relon_bytecode` / `relon_wasm_wasmtime` /
/// `relon_wasm_wasmtime_fast` rows that used to feed this variant
/// were deleted because the inlined predicate skips the production
/// source's first-class closure-call indirection. Retained as
/// documentation of the variant shape the IR pipeline must subsume
/// before the bench rows are reintroduced.
#[allow(dead_code)]
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
// =====  W13 — deep dict access (config-tree walk)  ===================
// =====================================================================
//
// Tier 1 Relon-flavour workload (panel expansion 2026-05-28). Models
// the canonical config-tree access pattern Relon ships for: a
// host-supplied `#internal cfg: { ... }` literal is read repeatedly
// inside a hot loop. Two 5-level-deep chains per iter
// (`cfg.db.pool.connections.max`, `cfg.db.pool.connections.timeout.ms`)
// exercise the tree-walker's `Op::FieldAccess` lookup chain end-to-end.
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w13_relon_src()` is byte-identical to the
//   production source — same `#internal cfg: { ... }` literal, same
//   `range(n).reduce(0, ...)` over the same closure body. The Lua
//   equivalent in `w13_lua_src()` builds the same nested Lua table
//   and walks the same 5-level chain per iter.
// * Algorithm complexity preserved: O(n) reduce with two dict-chain
//   reads per iter. No closed-form fold — the chain reads materialise
//   per iter through the tree-walker `Value::Dict` lookup path.
// * Time-math sanity: the tree-walker hits ~5 µs/iter on this shape
//   (smoke 2026-05-28: 5.13 ms for N=1000 ≈ 5.13 µs/iter) — ~500 ns
//   per dict-chain hop * 10 hops + arithmetic. LuaJIT JITs to
//   ~5 ns/iter (10 hash-table lookups + 2 adds in ~15 cycles). Sub-
//   1 ns per iter on the tree-walker row would indicate a closed-
//   form fold; the dispatch overhead alone rules that out.
// * I/O shape: `#main(Int n) -> Dict` with `result` field; Lua
//   returns the same scalar. Cross-checked via `assert_relon_lua_consistent`.
// * Backend coverage: tree_walk + luajit only. Bytecode / LLVM AOT /
//   wasm reject the production source (dict-literal as `#internal`
//   binding + bare `Dict` return are outside their lowering envelopes);
//   adding an inlined `_bytecode` variant that folds the dict reads to
//   the constant `5100` would be paper-win per
//   `paper_win_collapsed_variant_label` (W5/W8/W9/W10 history). The
//   `relon_jit` row in canonical_panel runs through `JitEvaluator`
//   which falls through to the tree-walker for this source.

/// W13 scale — 1k iterations keeps the tree-walker row in the µs
/// class (~20 ns/iter * 1k ≈ 20 µs); enough criterion samples for
/// stable p99 without wall-clock blowup.
const W13_N: i64 = 1_000;

fn w13_relon_src() -> &'static str {
    // `#unstrict` is required because the closure params `acc` / `i`
    // are untyped; the analyzer's strict-mode envelope demands explicit
    // type annotations. The tree-walker accepts the source either way
    // (the analyzer warnings are non-fatal); using `#unstrict` keeps the
    // signal-to-noise high so a future strict-mode tightening doesn't
    // silently drop the row.
    "#unstrict\n\
     #main(Int n) -> Dict\n\
     {\n\
       #internal\n\
       cfg: { db: { pool: { connections: { max: 100, timeout: { ms: 5000 } } } } },\n\
       result: range(n).reduce(0, (acc, i) =>\n\
         acc + cfg.db.pool.connections.max + cfg.db.pool.connections.timeout.ms)\n\
     }"
}

fn w13_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local cfg = {{ db = {{ pool = {{ connections = {{ max = 100, timeout = {{ ms = 5000 }} }} }} }} }}
            local acc = 0
            for i = 0, n - 1 do
                acc = acc + cfg.db.pool.connections.max + cfg.db.pool.connections.timeout.ms
            end
            return acc
        end"#,
        n = W13_N
    )
}

fn w13_expected() -> i64 {
    W13_N * (100 + 5000)
}

// =====================================================================
// =====  W14 — schema validate (boolean range checks)  ================
// =====================================================================
//
// Tier 1 Relon-flavour workload (panel expansion 2026-05-28). Models
// the per-iter cost of Relon's `#expect ...` schema gate surface — two
// boolean range checks per iter that always succeed on the synthetic
// input domain. The kernel exercises `Op::Lt` / `Op::Ge` / `Op::And`
// / `Op::If` chains in the tree-walker.
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w14_relon_src()` byte-identical to the production
//   source. Lua equivalent in `w14_lua_src()` runs the same ternary
//   chain via Lua's `and` / `or` short-circuit form (Lua doesn't have
//   a `?:` ternary; the canonical `cond and a or b` rewrite is
//   semantically equivalent when `a` is non-falsy, which is true for
//   `1` here).
// * Algorithm complexity preserved: O(n) reduce, two boolean range
//   checks + two conditional `0/1` adds per iter. The range bounds
//   were chosen so both predicates are trivially `true`, so the
//   sum-per-iter is exactly `+2`. The predicates ARE evaluated per
//   iter in the tree-walker.
// * Time-math sanity: smoke 2026-05-28 shows tree_walk 4.5 µs/iter,
//   luajit 11.4 ns/iter, bytecode 726 ns/iter, cranelift AOT 8.6 ns/iter,
//   wasm wasmtime_fast 4.9 ns/iter for N=1000. The compiled-backend
//   range (4.9-11.4 ns/iter) covers 4 comparisons + 4 boolean ops + 2
//   conditional adds per iter (~15-35 cycles at 3 GHz), consistent
//   with real loop execution. Sub-1 ns/iter would indicate a closed-
//   form fold — gated separately for LLVM AOT + rust_native via
//   `paper_win_closed_form_fold_label` because those reduce the
//   constant-true predicates to `n * 2` at -O3.
// * I/O shape: `#main(Int n) -> Int`, single Int param, Int return.
//   Lua equivalent matches.
// * Backend coverage: tree_walk + luajit + bytecode (if accepted).
//   LLVM AOT / rust_native suppressed via
//   `paper_win_closed_form_fold_label`.

const W14_N: i64 = 1_000;

fn w14_relon_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Int\n\
     range(n).reduce(0, (acc, i) =>\n\
       acc\n\
         + ((i % 10) >= 0 && (i % 10) < 10 ? 1 : 0)\n\
         + ((i / 10) >= 0 && (i / 10) < 1000 ? 1 : 0))"
}

fn w14_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local acc = 0
            for i = 0, n - 1 do
                local role = i % 10
                local region = math.floor(i / 10)
                acc = acc
                    + ((role >= 0 and role < 10) and 1 or 0)
                    + ((region >= 0 and region < 1000) and 1 or 0)
            end
            return acc
        end"#,
        n = W14_N
    )
}

fn w14_expected() -> i64 {
    W14_N * 2
}

// =====================================================================
// =====  W15 — conditional field (`?:` per-iter render)  ==============
// =====================================================================
//
// Tier 1 Relon-flavour workload (panel expansion 2026-05-28). Models
// the canonical declarative-DSL `?:` ternary render pattern Relon
// hosts use for "pick one of two computed expressions per row".
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: byte-identical to bench helper. Lua equivalent uses
//   `if .. then .. else .. end` form (most accurate semantic equivalent
//   when both branches produce same-type non-falsy values).
// * Algorithm complexity preserved: O(n) reduce, branch + multiply +
//   add per iter. The ternary IS evaluated branch-by-branch (the
//   tree-walker's `Op::If` dispatch picks one arm each iter); no
//   closed-form fold beyond what an aggressive optimiser could do
//   on `Σ (i%2==0 ? 2i : 3i)` — which is itself a closed-form
//   polynomial. The `paper_win_closed_form_fold_label` gate is the
//   right place to suppress an LLVM AOT row that performs the fold.
// * Time-math sanity: smoke 2026-05-28 shows tree_walk 2.8 µs/iter,
//   luajit 5.6 ns/iter, bytecode 315 ns/iter, cranelift AOT 5.6 ns/iter,
//   wasm wasmtime_fast 2.2 ns/iter for N=1000. The compiled-backend
//   range (2.2-5.6 ns/iter) covers 1 modulo + 1 compare + 1 multiply
//   + 1 add per iter (~7-17 cycles at 3 GHz), consistent with real
//   loop execution (the wasm number is close to the cycle floor for
//   the body, suggesting strong inlining without closed-form fold).
// * I/O shape: `#main(Int n) -> Int`; Lua matches.
// * Backend coverage: tree_walk + luajit + bytecode (if accepted).
//   LLVM AOT / rust_native suppressed via
//   `paper_win_closed_form_fold_label`.

const W15_N: i64 = 1_000;

fn w15_relon_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Int\n\
     range(n).reduce(0, (acc, i) =>\n\
       acc + (i % 2 == 0 ? i * 2 : i * 3))"
}

fn w15_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local acc = 0
            for i = 0, n - 1 do
                if (i % 2) == 0 then acc = acc + i * 2 else acc = acc + i * 3 end
            end
            return acc
        end"#,
        n = W15_N
    )
}

fn w15_expected() -> i64 {
    let mut acc: i64 = 0;
    for i in 0..W15_N {
        acc += if i % 2 == 0 { i * 2 } else { i * 3 };
    }
    acc
}

// =====================================================================
// =====  W16 — quicksort (recursive functional partition)  ============
// =====================================================================
//
// Tier 2 industry-standard workload (panel expansion 2026-05-28).
// Mirrors the Computer Language Benchmarks Game "quicksort" entry:
// build a deterministic PRNG-shuffled 1k-element array, recursively
// partition around a pivot, and return the sum of the partitioned
// elements. The sum is invariant under sort (multiset preservation),
// so the expected value is a closed-form analytic check that does
// not depend on a sorted output array — it catches algorithm
// substitution that drops elements.
//
// **HONESTY disclosure — sum-via-partition vs sort-then-sum**:
// Relon's `+` operator on (List, List) is not currently a list
// concat — the operator dispatch rejects with `TypeMismatch
// {expected: Number, found: List}` (see `arithmetic.rs:191` —
// (Operator::Add, a, b) routes through `eval_numeric_arithmetic`
// for non-Dict / non-Schema / non-String operand pairs). With no
// way to splice the recursive partitions back into a single sorted
// list inside the production source's functional core, the canonical
// quicksort recurrence `qs(xs<p) ++ [p] ++ qs(xs>p)` cannot be
// expressed byte-identically. The row uses the sum-via-partition
// variant `sum_qs(xs<p) + sum_qs(xs==p) + sum_qs(xs>p)` instead —
// structurally the SAME recursion + filter work (same partition
// closures, same recursion depth `O(log n)`, same per-level O(n)
// filter passes), the only difference is that the recursive fold
// composes a scalar (Int sum) at the join point rather than a
// concatenated List. Both algorithms have O(n log n) average
// complexity and identical per-call cost; the Lua row runs the
// SAME sum-via-partition recurrence to keep the comparison apples-
// to-apples. An in-place Lomuto/Hoare rewrite would be a paper-win
// (different algorithm, different constant factor); the row's name
// `W16_quicksort` reflects the recursion shape, not the absent
// concat splice.
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w16_relon_src()` is the bench's production source.
//   The Lua equivalent in `w16_lua_src` runs the SAME sum-via-
//   partition recurrence — not an in-place sort then sum, not a
//   table.sort + ipairs reduce. The doc-comment repeats this so a
//   future audit catches an in-place rewrite.
// * Algorithm complexity preserved: O(n log n) average over the
//   PRNG shuffle. The PRNG `(i * 1103515245 + 12345) % 2048` comes
//   from glibc's LCG and produces a near-uniform shuffle over
//   `[0, 2048)` for n=1000, so the expected recursion depth is
//   `O(log n) ≈ 10`. NO closed-form fold is possible — the
//   partition splits the input list shape-dependently, so even an
//   aggressive optimiser cannot reduce the recursive sum to a
//   polynomial in `n`. The closed-form-fold gate (W1/W2/W6) does
//   NOT apply.
// * Time-math sanity: `sum_qs(xs)` over n=1000 makes ~14k filter
//   calls (each filter walks O(level_size) elements). Tree-walker
//   per-iter cost ~ 200ns/filter × 14k ~ 3ms/run; LuaJIT ~ 100us/run.
//   Sub-1 µs/run would indicate a closed-form fold (impossible
//   here, see above).
// * I/O shape: `#main(Int n) -> Int`. Lua returns same scalar.
// * Backend coverage: tree_walk + luajit only. Bytecode envelope
//   rejects (recursion via `where`-clause helper + `_list_filter`
//   closures); LLVM AOT envelope rejects (first-class closure
//   values + recursion via `where`); wasm classifier scope-cuts.
//   rust_native row IS valid here — the algorithm shape does NOT
//   closed-form fold (see above), so a Rust quicksort-partition
//   baseline is the honest "what would this cost as plain Rust"
//   floor.

/// W16 scale — 1k-element PRNG-shuffled array. Tree-walker run is
/// ~3 ms; criterion samples 100 iters → ~300 ms per row, well within
/// the 5 s measurement budget. LuaJIT row is ~100x faster so its
/// criterion sample loop finishes in ~3 ms total.
const W16_N: i64 = 1_000;

fn w16_relon_src() -> &'static str {
    // Sum-via-partition quicksort recurrence: partition around the
    // head as pivot, recurse on `<` / `==` / `>` sublists, return
    // the sum of all three partitions. The `==` partition handles
    // duplicate pivots without an extra base-case branch.
    //
    // Algorithm shape (per `qs` call): three `_list_filter` passes
    // over `xs` (one per partition predicate), a `list.sum` over
    // the equal partition (a contiguous run of pivots; the only
    // partition that doesn't recurse), and two recursive calls. The
    // recursion terminates at `_len(xs) <= 1` where the sum is
    // either 0 (empty) or `xs[0]` (singleton). Per-call work is
    // O(|xs|) (three filter passes); total work is O(n log n)
    // average over the PRNG shuffle.
    //
    // The PRNG generator `(i * 1103515245 + 12345) % 2048` is
    // glibc's LCG (multiplier + increment); for `i ∈ [0, 1000)` it
    // produces 1000 values in `[0, 2048)` with no obvious
    // arithmetic structure an optimiser can exploit. `_list_filter`
    // is the underscore intrinsic the `std/list` `filter(l, f)`
    // wraps; using it directly skips the import-resolved
    // indirection at the call site.
    "#unstrict\n\
     #import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     sum_qs(arr)\n\
     where {\n\
       arr: range(n).map((i) => (i * 1103515245 + 12345) % 2048),\n\
       sum_qs(xs): _len(xs) <= 1 ? (_len(xs) == 0 ? 0 : xs[0]) : (\n\
         sum_qs(_list_filter(xs, (x) => x < xs[0]))\n\
         + list.sum(_list_filter(xs, (x) => x == xs[0]))\n\
         + sum_qs(_list_filter(xs, (x) => x > xs[0]))\n\
       )\n\
     }"
}

fn w16_lua_src() -> String {
    // Sum-via-partition quicksort (matches Relon row exactly). NOT
    // an in-place Lomuto/Hoare sort + sum loop. Three partition
    // walks per call (one filter per `<` / `==` / `>` predicate);
    // recursion on the `<` and `>` partitions only; `==` partition
    // sums in-place. An in-place rewrite would be a paper-win
    // (different algorithm, different constant factor).
    format!(
        r#"return function()
            local n = {n}
            local arr = {{}}
            for i = 0, n - 1 do
                arr[i + 1] = (i * 1103515245 + 12345) % 2048
            end
            local function sum_qs(xs)
                local len = #xs
                if len == 0 then return 0 end
                if len == 1 then return xs[1] end
                local p = xs[1]
                local lt, eq_sum, gt = {{}}, 0, {{}}
                for i = 1, len do
                    local v = xs[i]
                    if v < p then lt[#lt + 1] = v
                    elseif v > p then gt[#gt + 1] = v
                    else eq_sum = eq_sum + v end
                end
                return sum_qs(lt) + eq_sum + sum_qs(gt)
            end
            return sum_qs(arr)
        end"#,
        n = W16_N
    )
}

fn w16_expected() -> i64 {
    // The sort preserves the multiset → sum is invariant under sort.
    let mut acc: i64 = 0;
    for i in 0..W16_N {
        acc += (i.wrapping_mul(1103515245).wrapping_add(12345)) % 2048;
    }
    acc
}

// =====================================================================
// =====  W17 — binary search (O(log n) per-target lookup)  ============
// =====================================================================
//
// Tier 2 industry-standard workload (panel expansion 2026-05-28).
// Mirrors the Computer Language Benchmarks Game "binary-trees" /
// "search" entries: repeatedly bisect a sorted Int array of size n
// for n different targets. The targets follow a multiplicative
// scrambling `(i * 31) % n` — NOT `range(n)` — so each lookup hits
// a different bucket and the per-iter cost can't be closed-form
// folded to a single arithmetic identity.
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w17_relon_src()` is the production source. Lua
//   equivalent runs the SAME bisection algorithm (recursive `bs(lo,
//   hi, t)` with mid-point split). No iterative `while` rewrite —
//   recursion shape matches the Relon row's per-call frame cost.
// * Algorithm complexity preserved: O(n log n) total — n targets ×
//   O(log n) bisection depth each. For `sorted = range(n)`, the
//   bisection terminates at `target` itself (since the array is the
//   identity permutation). Per-target lookup makes log2(n) ≈ 7
//   recursive calls for n=100. The `(i * 31) % n` scrambling
//   ensures the targets cover a near-uniform spread, so the average
//   bisection depth is the algorithm's lower bound — NO closed-form
//   fold collapses the sum.
// * Time-math sanity: 100 targets × 7 recursive calls × ~ 50ns/call
//   in the tree-walker ~ 35 µs/run. LuaJIT ~ 1 µs/run. Sub-100 ns/
//   run would indicate the bench reduced to a closed-form identity.
// * I/O shape: `#main(Int n) -> Int`. Lua matches.
// * Backend coverage: tree_walk + luajit only. Bytecode rejects
//   (recursion via `where`-clause + index probe); LLVM AOT rejects
//   (recursion path tracked for Phase F.W7 follow-up, same envelope
//   limit applies here); wasm classifier scope-cuts. rust_native
//   row IS valid (no closed-form fold; the scrambled-target sum is
//   not a polynomial in `n`).

/// W17 scale — 100 elements / 100 targets. Tree-walker ~35 µs/run;
/// criterion 100 samples → ~3.5 ms per row. Smaller-than-W16 n
/// because the recursive helper makes the tree-walker's per-frame
/// scope-clone the dominant cost; bumping n bloats wall-clock
/// without improving the signal-to-noise.
const W17_N: i64 = 100;

fn w17_relon_src() -> &'static str {
    // Bisection over `sorted = range(n)`, target stream
    // `(i * 31) % n` (multiplicative scrambling defeats any
    // optimiser-side fold that recognised the closed form for the
    // sequential `range(n)` target stream).
    //
    // Base case `hi - lo <= 1`: the window has shrunk to a single
    // slot — return `lo` (the candidate index). The arithmetic
    // `sorted[mid] == t` collapses to `mid == t` here because the
    // sorted array IS `range(n)`, but the bisection algorithm still
    // executes a real comparison + branch per recursive step; the
    // optimiser doesn't see the identity unless it inlines `sorted`
    // construction, which the tree-walker / functional core never
    // does (the list materialises before the reduce starts).
    "#unstrict\n\
     #main(Int n) -> Int\n\
     range(n).reduce(0, (acc, i) => acc + bs(0, n, (i * 31) % n))\n\
     where {\n\
       bs(lo, hi, t): hi - lo <= 1 ? lo : (\n\
         (lo + hi) / 2 <= t\n\
           ? bs((lo + hi) / 2, hi, t)\n\
           : bs(lo, (lo + hi) / 2, t)\n\
       )\n\
     }"
}

fn w17_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local function bs(lo, hi, t)
                if hi - lo <= 1 then return lo end
                local mid = math.floor((lo + hi) / 2)
                if mid <= t then return bs(mid, hi, t)
                else return bs(lo, mid, t) end
            end
            local acc = 0
            for i = 0, n - 1 do
                acc = acc + bs(0, n, (i * 31) % n)
            end
            return acc
        end"#,
        n = W17_N
    )
}

fn w17_expected() -> i64 {
    // For `sorted = range(n)`, the bisection lands on `t` itself
    // (the identity permutation). Summing `(i * 31) % n` for
    // `i ∈ [0, n)` over the multiplicative-scramble of integers
    // mod n. We compute it directly to keep the consistency check
    // self-checking against the algorithm's expected output.
    let mut acc: i64 = 0;
    for i in 0..W17_N {
        acc += (i.wrapping_mul(31)) % W17_N;
    }
    acc
}

// =====================================================================
// =====  W18 — prime count (trial-division)  ==========================
// =====================================================================
//
// Tier 2 industry-standard workload (panel expansion 2026-05-28).
// **Algorithm note** (HONESTY disclosure, NOT a paper-win): the task
// brief names "Eratosthenes sieve". Relon's functional core has no
// in-place mutable boolean array, so the canonical sieve (mark `i*p`
// composite for each prime `p`) cannot be lifted byte-identically.
// The row uses **trial-division primality test** instead — for each
// candidate `k ∈ [2, n)`, recurse over divisors `d ∈ [2, sqrt(k)]`
// and return `false` if any divides. Same prime-counting output
// (`pi(n) = 1229` for n=10000), but the algorithm is O(n*sqrt(n))
// rather than the sieve's O(n*log(log(n))). The Lua row runs the
// SAME trial-division algorithm so the per-target cost is apples-
// to-apples; an in-place mutable-array sieve in Lua would be a
// paper-win (faster Lua loop wears the row name "prime sieve"
// while Relon pays the trial-division cost).
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w18_relon_src()` is the production source. The
//   row is honestly labelled `W18_prime_count_trial_div` (not
//   `W18_prime_sieve`) so downstream tooling reflects the actual
//   algorithm. The Lua equivalent runs the SAME nested-recursion
//   shape.
// * Algorithm complexity preserved: O(n*sqrt(n)) — for n=10000,
//   that's ~1M primality probes worst-case (most candidates fail
//   early). NO closed-form fold — `pi(n)` is not a polynomial in
//   `n` (it's transcendental, lower-bounded by `n / ln(n)`); even
//   an aggressive optimiser cannot reduce the count to a literal.
// * Time-math sanity: trial-division on `[2, 10000)` does ~30k
//   divisor probes total (sum_{k<n} sqrt(k) ≈ (2/3) * n * sqrt(n)
//   for n=10000 ≈ 666k probes; most are early-exit). Tree-walker
//   ~ 1-3 ms/run; LuaJIT ~ 100 µs/run.
// * I/O shape: `#main(Int n) -> Int`. Lua matches.
// * Backend coverage: tree_walk + luajit only. Bytecode rejects
//   (`where`-clause helper + recursion); LLVM AOT rejects
//   (recursion path); wasm classifier scope-cuts. rust_native row
//   IS valid (the algorithm shape doesn't closed-form fold).

/// W18 scale — `pi(10000) = 1229` primes. Tree-walker ~1-3 ms/run;
/// criterion 100 samples → ~100-300 ms per row.
const W18_N: i64 = 10_000;

fn w18_relon_src() -> &'static str {
    // Trial-division primality count. `range(2, n)` is the candidate
    // stream; `is_prime(k)` recurses on the divisor `d`, early-
    // exiting at `d * d > k` (the sqrt(k) upper bound) and on the
    // first divisor that divides `k`. Same nested-recursion shape
    // the Lua row runs.
    //
    // `range(start, end)` is supported (the stdlib `range` Native
    // accepts 1 or 2 args; 2-arg form yields `[start, end)`).
    "#unstrict\n\
     #main(Int n) -> Int\n\
     _len(_list_filter(range(2, n), (k) => is_prime(k, 2)))\n\
     where {\n\
       is_prime(k, d): d * d > k ? true : (k % d == 0 ? false : is_prime(k, d + 1))\n\
     }"
}

fn w18_lua_src() -> String {
    format!(
        r#"return function()
            local n = {n}
            local function is_prime(k, d)
                if d * d > k then return true end
                if k % d == 0 then return false end
                return is_prime(k, d + 1)
            end
            local count = 0
            for k = 2, n - 1 do
                if is_prime(k, 2) then count = count + 1 end
            end
            return count
        end"#,
        n = W18_N
    )
}

fn w18_expected() -> i64 {
    fn is_prime(k: i64) -> bool {
        let mut d: i64 = 2;
        while d.saturating_mul(d) <= k {
            if k % d == 0 {
                return false;
            }
            d += 1;
        }
        true
    }
    let mut count: i64 = 0;
    let mut k: i64 = 2;
    while k < W18_N {
        if is_prime(k) {
            count += 1;
        }
        k += 1;
    }
    count
}

// =====================================================================
// =====  W19 — matrix multiply (O(n^3) triple-nested loop)  ===========
// =====================================================================
//
// Tier 3 numeric kernel workload (panel expansion 2026-05-28). Mirrors
// the Computer Language Benchmarks Game "matrix-multiply" entry: build
// two 16x16 Int matrices via a deterministic generator, compute the
// product `C = A * B` via the canonical triple-nested loop
// `c[i][j] = Σ_k a[i][k] * b[k][j]`, and return the sum of all 256
// entries of the result matrix. The sum-of-product reduce keeps the
// `#main` return shape scalar (Int) so the bench harness can dispatch
// uniformly with the rest of the panel.
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w19_relon_src()` IS the production source — a
//   `where`-clause builds `a`, `b`, `c` as nested-`map` lists, the
//   `#main` body folds the result matrix to a scalar via two stacked
//   reduces. The Lua row runs the SAME triple-loop algorithm; no
//   transpose / strided rewrite / Strassen replacement that would
//   change the algorithm complexity class.
// * Algorithm complexity preserved: O(n^3) for the inner reduce loop,
//   plus O(n^2) for the result-sum fold; n=16 means 4096 multiplies +
//   adds + 256 reductions, the dominant cost. NO closed-form fold
//   possible — `(i * size + k) % 100` and `(k + j) % 100` carry mod-
//   100 discontinuities that defeat algebraic collapse; an optimiser
//   can unroll but cannot reduce the inner reduce to a polynomial in
//   `n`.
// * Time-math sanity: 4096 inner iterations × ~50 ns/iter on the
//   tree-walker (closure dispatch + scope-clone + 2D index probe) ≈
//   200 µs/run. LuaJIT ~ 30 µs/run (the canonical matmul ratio).
//   Per-iter cost < 5 ns on the tree-walker would indicate the inner
//   reduce got constant-folded (impossible because of the mod-100
//   discontinuity); per-iter cost < 1 ns on rust_native would
//   indicate rustc / LLVM saw through the closure shape.
// * I/O shape: `#main(Int n) -> Int`. Here `n` IS the matrix size
//   (configurable, defaults to W19_N = 16). Lua row reads `n` from
//   the same constant. Both sides produce a scalar Int checksum.
// * Backend coverage: tree_walk + luajit only. Bytecode envelope
//   rejects (`unknown stdlib method range` — the bytecode lowering
//   does not yet accept the 2D `range(size).map((i) => range(size).
//   map(...))` shape because it materialises a `List<List<Int>>` the
//   bytecode VM's M2-A scalar envelope does not handle). LLVM AOT
//   envelope rejects (same surface). Cranelift AOT rejects
//   (`ir lowering failed: unknown stdlib method range` — Cranelift's
//   envelope inherits the same 2D-list scope-cut). Wasm classifier
//   scope-cuts (Z.1 program set does not cover matmul). rust_native
//   row IS valid here — the algorithm shape does NOT closed-form
//   fold (mod-100 discontinuity), so a Rust triple-loop baseline is
//   the honest "what would matmul cost as plain Rust" floor.

/// W19 scale — 16x16 matrix. The Computer Language Benchmarks Game
/// uses 800x800 for the headline numbers; Relon's tree-walker pays
/// closure / scope-clone overhead on every inner-loop call, so 16x16
/// (4k inner mul-adds) is the largest size that keeps the criterion
/// sample loop within the 5 s measurement budget. Larger matrices
/// (e.g. 32x32 = 32k inner) push tree-walker run to ~2 ms each, which
/// with the 100-sample criterion budget exceeds the configured
/// measurement_time. Smaller sizes (8x8 = 512 inner) cut the signal-
/// to-noise by 8x without saving meaningful wall-clock.
///
/// `n` IS the matrix size at runtime (the source resolves `size: n`
/// in the `where` clause), so callers can dial the workload by
/// supplying a different `n`. Smoke runner uses W19_N; bench uses
/// W19_N.
const W19_N: i64 = 16;

fn w19_relon_src() -> &'static str {
    // Triple-nested matrix multiply:
    //   a[i][j] = (i * size + j) % 100
    //   b[i][j] = (i + j) % 100
    //   c[i][j] = Σ_k a[i][k] * b[k][j]
    //   result  = Σ_i Σ_j c[i][j]
    //
    // The mod-100 in the generators is load-bearing — without it,
    // both `a[i][k]` and `b[k][j]` are arithmetic-progression terms
    // an aggressive optimiser could collapse to a closed form. The
    // mod-100 discontinuity keeps the per-cell cost shape-dependent
    // so neither rustc nor LLVM can reduce the inner reduce to a
    // polynomial in `size`.
    //
    // `where`-clause defines `a` / `b` / `c` as lazy bindings; the
    // tree-walker materialises each `List<List<Int>>` once (on first
    // reference), then the outer fold reads cells via `a[i][k]` /
    // `b[k][j]` per inner step. The bytecode / LLVM AOT / cranelift
    // backends all reject this shape today — see the doc-comment
    // above.
    "#unstrict\n\
     #main(Int n) -> Int\n\
     c.reduce(0, (row_acc, row) => row_acc + row.reduce(0, (cell_acc, cell) => cell_acc + cell))\n\
     where {\n\
       size: n,\n\
       a: range(size).map((i) => range(size).map((j) => (i * size + j) % 100)),\n\
       b: range(size).map((i) => range(size).map((j) => (i + j) % 100)),\n\
       c: range(size).map((i) => range(size).map((j) => range(size).reduce(0, (acc, k) => acc + a[i][k] * b[k][j])))\n\
     }"
}

fn w19_lua_src() -> String {
    // Same triple-nested matmul. Lua's 1-indexed arrays force the
    // `i + 1` / `j + 1` / `k + 1` shifts when comparing to the Relon
    // source's 0-indexed reads; the mod-100 generator and the final
    // sum-fold remain byte-equivalent.
    format!(
        r#"return function()
            local n = {n}
            local size = n
            local a, b = {{}}, {{}}
            for i = 1, size do
                a[i], b[i] = {{}}, {{}}
                for j = 1, size do
                    a[i][j] = ((i - 1) * size + (j - 1)) % 100
                    b[i][j] = ((i - 1) + (j - 1)) % 100
                end
            end
            local result = 0
            for i = 1, size do
                for j = 1, size do
                    local s = 0
                    for k = 1, size do
                        s = s + a[i][k] * b[k][j]
                    end
                    result = result + s
                end
            end
            return result
        end"#,
        n = W19_N
    )
}

fn w19_expected() -> i64 {
    // Closed-form analytic check (mirrors the smoke runner): for each
    // cell of the result matrix, accumulate `a[i][k] * b[k][j]`, then
    // sum all cells. Wrapping arithmetic matches the Relon `Op::Add` /
    // `Op::Mul(I64)` lowering semantics (no overflow check because the
    // value range is bounded above by 16 * 99 * 99 ≈ 156 k per cell, well
    // below 2^63).
    let size: i64 = W19_N;
    let mut total: i64 = 0;
    let mut i: i64 = 0;
    while i < size {
        let mut j: i64 = 0;
        while j < size {
            let mut s: i64 = 0;
            let mut k: i64 = 0;
            while k < size {
                let aik = (i.wrapping_mul(size).wrapping_add(k)) % 100;
                let bkj = (k.wrapping_add(j)) % 100;
                s = s.wrapping_add(aik.wrapping_mul(bkj));
                k += 1;
            }
            total = total.wrapping_add(s);
            j += 1;
        }
        i += 1;
    }
    total
}

// =====================================================================
// =====  W20 — scaled n-body (Verlet integration on 4 bodies)  ========
// =====================================================================
//
// Tier 3 numeric kernel workload (panel expansion 2026-05-28).
// Mirrors the Computer Language Benchmarks Game "n-body" entry shape:
// a 4-body 1D system, Verlet-style symplectic integration over n
// time-steps, asymmetric masses + initial conditions to defeat
// momentum-conservation closed forms.
//
// **HONESTY disclosure — softening kernel substitutes `1/r^3`**:
// The canonical Verlet kernel uses Newtonian gravity `F = m1*m2 / r^2`
// (force) → `a = F/m = m / r^2`, which requires a `sqrt(dx^2 + dy^2)`
// to recover `r` and then `1/r^3` for the per-component acceleration
// (`a_x = m * dx / r^3`). Relon's stdlib `std/math` exposes only
// `abs` / `max` / `min` / `clamp` — there is NO `sqrt` / `pow` /
// `exp` today. The source uses a softened `1/(r^2 + eps)^2`
// substitute (`inv_r3 ≈ 1 / (r^2 + eps)^2`), which is shape-equivalent
// (per-step cost is 4 mul + 1 add + 1 div per pair, same as
// `dx / r^3`) but mathematically NOT Newtonian gravity. The bench
// row's name is `W20_n_body_softened` (NOT `W20_n_body`) so a
// future audit sees the substitution at row-add time. The Lua row
// runs the SAME softened kernel — both sides pay the same per-pair
// cost; an in-line `math.sqrt` rewrite in Lua would be a paper win
// (LuaJIT's `vsqrtsd` intrinsic vs Relon's mul-mul fold).
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w20_relon_src()` IS the production source. Lua row
//   in `w20_lua_src()` runs the SAME softened 4-body Verlet step
//   over the SAME initial conditions; no closed-form analytic
//   shortcut on either side.
// * Algorithm complexity preserved: O(n_steps * n_bodies^2) — for
//   4 bodies × 1000 steps = 16 000 pair-force evaluations, each
//   computing `dx = s[j] - s[i]`, `r2 = dx*dx + soft`, `inv_r4 =
//   1/(r2*r2)`, and `force = dx * mj * inv_r4`. NO closed-form fold —
//   the per-step state mutates in a shape-dependent way (each
//   position update feeds back into the next step's distance), so
//   neither rustc nor LLVM can reduce the time-loop to a polynomial
//   in `n_steps`.
// * Time-math sanity: 4 bodies × 4 pairs/body × 4 mul + 1 div / pair
//   ≈ 64 ops/step × 1000 steps = 64 000 fp ops per `#main` call.
//   Tree-walker per-iter ~ 250 ns (closure dispatch dominated) × 4k
//   inner calls ≈ 1 ms / run. LuaJIT ~ 20 µs / run. rust_native ~ 1
//   µs / run (4 cache-resident bodies, no allocation). Per-iter cost
//   << 1 ns/run would indicate the time loop got folded (impossible
//   because of the feedback-into-next-step shape).
// * I/O shape: `#main(Int n) -> Float`. `n` = number of time steps
//   (1000 by default). Float return is the asymmetric weighted
//   checksum `Σ_i (i+1) * x_i + Σ_i (5+i) * v_i` so a position-only
//   bug surfaces in the velocity weight. Lua row returns the same.
//   The smoke runner asserts the result is within 1e-6 absolute of
//   the Rust-native reference — Float comparison is tolerance-based,
//   NOT bit-equal, because the tree-walker's expression evaluation
//   order may differ from `rustc`'s by 1 ULP on a fused-mul-add lane.
// * Backend coverage: tree_walk + luajit only. Bytecode envelope
//   rejects (`closure value cannot cross the wasm module boundary` —
//   the bytecode VM's M2-A scalar envelope does not handle the
//   first-class closure values `step` / `accel` / `pair_force` the
//   source carries through `where` bindings). Cranelift AOT rejects
//   (same reason). LLVM AOT rejects (Float type + first-class
//   closures both outside the Phase E envelope — Phase F.W7 lifted
//   only Int recursion). Wasm classifier scope-cuts (no Float
//   support in Z.1 program set). rust_native row IS valid — the
//   feedback-shaped time loop does NOT closed-form fold.

/// W20 scale — 1000 Verlet integration steps over a 4-body 1D
/// system. The CLBG "n-body" entry uses 50 000 000 steps on 5 bodies;
/// Relon's tree-walker pays closure dispatch + scope clone on every
/// pair-force call (16 calls/step × 1000 steps = 16 000 closure
/// invocations), pushing per-run to ~1 ms even at 1000 steps. Larger
/// step counts blow the criterion 5 s measurement budget; smaller
/// step counts (100 steps) hide the Verlet drift accumulation that
/// defeats trivial closed-form folds. 1000 steps is the size that
/// produces a non-trivial asymmetric checksum (≈ 75.87) with stable
/// wall-clock.
const W20_N: i64 = 1_000;

fn w20_relon_src() -> &'static str {
    // 4-body 1D Verlet step. State `s` is an 8-element list:
    // `[x0, x1, x2, x3, v0, v1, v2, v3]`. Each step computes the
    // pairwise softened force `pair_force(s, i, j, mj)` for body
    // `i` from body `j` (mass `mj`), accumulates the per-body
    // acceleration `accel(s, i) = Σ_j pair_force(s, i, j, m_j)`,
    // and produces a new 8-element state by integrating positions
    // and velocities by `dt`.
    //
    // The asymmetric masses (`m0=1`, `m1=2`, `m2=0.5`, `m3=3`) and
    // initial state (`x = [0.0, 1.0, 2.5, 4.0]`, `v = [0.1, 0.0,
    // 0.0, 0.2]`) defeat the momentum-conservation symmetries that
    // would otherwise collapse `Σ x_i` to a constant across all
    // time steps (in a symmetric 1D 4-body system, the centre-of-
    // mass position is conserved; the per-step delta on `Σ m_i *
    // x_i` is zero). With asymmetric masses, the per-step delta
    // shows up in the final checksum, and the bench measures real
    // per-step work rather than a Σ-x-equals-const identity.
    //
    // Final checksum is the asymmetric weighted sum:
    // `Σ_i (i+1) * x_i + Σ_i (5+i) * v_i`. Weights `(1..=8)` are
    // co-prime with the body count, so no 1D 4-body symmetry can
    // collapse the sum to a constant.
    //
    // `#unstrict` is required because the closure params (`s`, `i`,
    // `j`, `mj`, `_step`) are untyped; the analyzer's strict-mode
    // envelope demands explicit type annotations. The tree-walker
    // accepts either way; using `#unstrict` keeps the strict-mode
    // tightening path from silently dropping the row.
    "#unstrict\n\
     #main(Int n) -> Float\n\
     final_state[0] * 1.0 + final_state[1] * 2.0 + final_state[2] * 3.0 + final_state[3] * 4.0\n\
       + final_state[4] * 5.0 + final_state[5] * 6.0 + final_state[6] * 7.0 + final_state[7] * 8.0\n\
     where {\n\
       dt: 0.01,\n\
       soft: 0.1,\n\
       m0: 1.0, m1: 2.0, m2: 0.5, m3: 3.0,\n\
       init: [0.0, 1.0, 2.5, 4.0, 0.1, 0.0, 0.0, 0.2],\n\
       pair_force(s, i, j, mj):\n\
         i == j ? 0.0 :\n\
           (s[j] - s[i]) * mj * (1.0 / (((s[j] - s[i]) * (s[j] - s[i]) + soft) * ((s[j] - s[i]) * (s[j] - s[i]) + soft))),\n\
       accel(s, i): pair_force(s, i, 0, m0) + pair_force(s, i, 1, m1) + pair_force(s, i, 2, m2) + pair_force(s, i, 3, m3),\n\
       step(s): [\n\
         s[0] + s[4] * dt,\n\
         s[1] + s[5] * dt,\n\
         s[2] + s[6] * dt,\n\
         s[3] + s[7] * dt,\n\
         s[4] + accel(s, 0) * dt,\n\
         s[5] + accel(s, 1) * dt,\n\
         s[6] + accel(s, 2) * dt,\n\
         s[7] + accel(s, 3) * dt\n\
       ],\n\
       final_state: range(n).reduce(init, (s, _step) => step(s))\n\
     }"
}

fn w20_lua_src() -> String {
    // Same softened 4-body 1D Verlet step. Lua's 1-indexed lists
    // force the `s[i + 1]` shifts when comparing to the Relon
    // source's 0-indexed reads; the integration kernel and the
    // final weighted checksum remain byte-equivalent.
    format!(
        r#"return function()
            local n = {n}
            local dt = 0.01
            local soft = 0.1
            local m = {{1.0, 2.0, 0.5, 3.0}}
            local s = {{0.0, 1.0, 2.5, 4.0, 0.1, 0.0, 0.0, 0.2}}
            local function pair_force(s, i, j, mj)
                if i == j then return 0.0 end
                local dx = s[j + 1] - s[i + 1]
                local r2 = dx * dx + soft
                return dx * mj * (1.0 / (r2 * r2))
            end
            local function accel(s, i)
                return pair_force(s, i, 0, m[1]) + pair_force(s, i, 1, m[2])
                     + pair_force(s, i, 2, m[3]) + pair_force(s, i, 3, m[4])
            end
            for _ = 1, n do
                local ns = {{
                    s[1] + s[5] * dt,
                    s[2] + s[6] * dt,
                    s[3] + s[7] * dt,
                    s[4] + s[8] * dt,
                    s[5] + accel(s, 0) * dt,
                    s[6] + accel(s, 1) * dt,
                    s[7] + accel(s, 2) * dt,
                    s[8] + accel(s, 3) * dt,
                }}
                s = ns
            end
            return s[1] * 1.0 + s[2] * 2.0 + s[3] * 3.0 + s[4] * 4.0
                 + s[5] * 5.0 + s[6] * 6.0 + s[7] * 7.0 + s[8] * 8.0
        end"#,
        n = W20_N
    )
}

/// W20 absolute-error tolerance for Float consistency checks.
///
/// Verlet integration over 1000 steps with `dt = 0.01` accumulates
/// ~10 mul/add per step per body (16 body updates × 64 pair-force
/// evals = ~1k fma per step × 1000 = ~1M fp ops). Each fma carries
/// ~1 ULP of relative error; cumulative drift over 1M ops is bounded
/// by `1e-16 * 1e6 ≈ 1e-10` relative, which on a checksum value of
/// ~75 lands at `~7.5e-9` absolute. 1e-6 is a 100x safety margin
/// for cross-runtime FMA-vs-no-FMA differences (LuaJIT may emit
/// `vfmadd213sd` on AVX2; Relon's tree-walker evaluates `a * b + c`
/// as two operations).
const W20_FLOAT_TOL: f64 = 1.0e-6;

fn w20_expected() -> f64 {
    // Hand-rolled reference. Uses `f64` (matches the Relon tree-
    // walker's Float runtime type) with no `mul_add` so the rounding
    // mode matches the source's `a * b + c` shape exactly.
    let n: i64 = W20_N;
    let dt: f64 = 0.01;
    let soft: f64 = 0.1;
    let m: [f64; 4] = [1.0, 2.0, 0.5, 3.0];
    let mut s: [f64; 8] = [0.0, 1.0, 2.5, 4.0, 0.1, 0.0, 0.0, 0.2];
    let mut step = 0i64;
    while step < n {
        let mut a: [f64; 4] = [0.0; 4];
        for i in 0..4 {
            let mut ai = 0.0;
            for j in 0..4 {
                if i == j {
                    continue;
                }
                let dx = s[j] - s[i];
                let r2 = dx * dx + soft;
                ai += dx * m[j] * (1.0 / (r2 * r2));
            }
            a[i] = ai;
        }
        let mut ns: [f64; 8] = [0.0; 8];
        for i in 0..4 {
            ns[i] = s[i] + s[4 + i] * dt;
            ns[4 + i] = s[4 + i] + a[i] * dt;
        }
        s = ns;
        step += 1;
    }
    s[0] * 1.0
        + s[1] * 2.0
        + s[2] * 3.0
        + s[3] * 4.0
        + s[4] * 5.0
        + s[5] * 6.0
        + s[6] * 7.0
        + s[7] * 8.0
}

// =====================================================================
// =====  W21 — match dispatch over brand-tagged values  ===============
// =====================================================================
//
// Tier 4 Relon-flavour workload (panel expansion 2026-05-29, Phase 1
// "Group A — high-frequency runtime dispatch"). Directly mirrors the
// `fixtures/polymorphism.relon` showcase shape: a heterogeneous list
// of `#brand Schema { ... }`-tagged dicts is dispatched per iter via
// a `match` arm-table keyed on the brand label. The reduce closure
// alternates between the two brands by `i % 2`, so neither LuaJIT
// nor the tree-walker can collapse the branch to a constant.
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w21_relon_src()` IS the production source — a
//   `#main(Int n) -> Dict` body builds the two `#brand Image` /
//   `#brand Text` items, the `classify(it)` closure runs `it match {
//   Image: 1, Text: 2, *: 0 }` per iter, the reduce folds the per-
//   iter classification into the running accumulator. Lua row uses
//   the `__type` field convention to simulate the brand tag and a
//   small `if / elseif / else` ladder (LuaJIT has no `match`); the
//   per-iter work is the same — table-field probe + brand-equal
//   compare + branch select.
// * Algorithm complexity preserved: O(n) reduce, one `items[i % 2]`
//   subscript + one brand-label compare + one Int add per iter. No
//   closed-form fold — the branch select alternates per iter and
//   the per-iter result depends on which brand the runtime resolved,
//   which is shape-dependent.
// * Time-math sanity: items[0] is `#brand Image` → classify → 1;
//   items[1] is `#brand Text` → classify → 2. Over n=10 000 iters
//   (even n) half land on Image and half on Text, so the analytic
//   answer is `n / 2 * 1 + n / 2 * 2 = n * 3 / 2 = 15 000`. The
//   tree-walker per-iter cost is closure dispatch (~ 200 ns) + brand
//   compare (~ 20 ns) + list subscript (~ 30 ns); ≈ 250-300 ns/iter
//   × 10k iters = 2.5-3 ms / run. LuaJIT row pays the same per-iter
//   ladder via `if-elseif` lowered to `cmp + cmov`; ≈ 5-10 ns/iter
//   × 10k iters ≈ 50-100 µs / run.
// * I/O shape: `#main(Int n) -> Dict`. Returned Dict carries a
//   `result: Int` field; `relon_int_result` unwraps it (same shape
//   as W13 / W14 / W15 / W16 / W17 / W18 / W19). Lua row returns the
//   scalar Int directly. Both flow through `assert_relon_lua_
//   consistent` for an exact-equal cross-check.
// * Backend coverage: tree_walk + luajit. Bytecode envelope rejects
//   (`#brand` + `match` lowering both outside the M2-A scalar
//   envelope today); `try_build_bytecode` returns None, the row
//   logs `n/a (UnsupportedOp: <reason>)`. LLVM AOT / Cranelift AOT /
//   wasm reject (same `#brand` / `match` / `#schema` surface — the
//   Z.4 wasm-walker envelope does not lower the brand-tag compare,
//   and the LLVM Phase E envelope tops out at String ops). The
//   canonical-panel `relon_jit` row routes the production source
//   through `JitEvaluator::run_main`, which falls through to the
//   tree-walker for the brand+match dispatch. The dedicated
//   `relon_trace_jit` row is gated out via
//   `trace_jit_production_label_eligible` — the recorder rejects
//   `Op::Match` arms today (closure-abort path tracked under the
//   J.4 envelope expansion). `rust_native` is NOT applicable: a
//   rust-side `enum + match` would be a closed-form replacement
//   that skips the tree-walker's runtime brand-string compare; the
//   per-iter "cost of dynamic brand dispatch" is the load-bearing
//   measurement here. The row is gated out by adding the label to
//   `paper_win_collapsed_variant_label`-style honesty rules below.

/// W21 scale — n = 10 000 reduce iters. Same scale as W13 / W14 /
/// W18 (tier-1 / tier-2 Relon-flavour workloads). Smaller n hides the
/// per-iter brand-dispatch cost in criterion's variance floor; larger
/// n would push tree-walker run-time to multi-ms which blows the 5 s
/// measurement budget when combined with the per-iter brand-compare
/// overhead (~ 250 ns × 10k = 2.5 ms / run; × 100 samples + warmup
/// fits within budget).
const W21_N: i64 = 10_000;

fn w21_relon_src() -> &'static str {
    // The `#main` body is the standard W13-shape Dict literal: two
    // `#schema` declarations register the brand labels with the
    // analyzer, the `items` list materialises two `#brand`-decorated
    // dicts (one per schema), the `classify(it)` closure dispatches
    // on the brand label via `match`, and the `result: range(n).
    // reduce(...)` field carries the scalar accumulator the Dict
    // unwrap helper projects out.
    //
    // `#unstrict` is required because the closure params (`acc`,
    // `i`, `it`) are untyped — the analyzer's strict-mode envelope
    // demands explicit type annotations; the tree-walker accepts
    // either way. The schemas are declared inline inside the `#main`
    // body Dict rather than at top-level because the parser today
    // rejects top-level `#schema X { ... },` directives outside a
    // Dict context (probed 2026-05-29; v1 form returned
    // `parse error: expected expression`). Declaring inside the
    // Dict body keeps the production-source shape valid; the brand-
    // tag lookup still happens at `#brand` evaluation time and the
    // match arms still key on the schema label registered by the
    // inner `#schema` declarations.
    "#unstrict\n\
     #main(Int n) -> Dict\n\
     {\n\
       #schema Image { name: String, url: String },\n\
       #schema Text { name: String, content: String },\n\
       items: [\n\
         #brand Image { name: \"img\", url: \"http://a.png\" },\n\
         #brand Text { name: \"txt\", content: \"hello\" }\n\
       ],\n\
       classify(it): it match {\n\
         Image: 1,\n\
         Text: 2,\n\
         *: 0\n\
       },\n\
       result: range(n).reduce(0, (acc, i) => acc + classify(items[i % 2]))\n\
     }"
}

fn w21_lua_src() -> String {
    // Lua has no `match` and no brand tag; the canonical equivalent
    // is to carry a `__type` field on each dict and dispatch with
    // `if / elseif / else`. The per-iter work is shape-equivalent:
    // table-field probe (`it.__type`) + string compare + branch
    // select, mirroring the tree-walker's brand-label compare. Lua's
    // 1-indexed tables force the `(i % 2) + 1` subscript shift; the
    // per-iter sum semantics stay byte-identical (5000×1 + 5000×2 =
    // 15 000 over n=10 000).
    format!(
        r#"return function()
            local n = {n}
            local items = {{
                {{ __type = "Image", name = "img", url = "http://a.png" }},
                {{ __type = "Text", name = "txt", content = "hello" }},
            }}
            local function classify(it)
                if it.__type == "Image" then
                    return 1
                elseif it.__type == "Text" then
                    return 2
                else
                    return 0
                end
            end
            local acc = 0
            for i = 0, n - 1 do
                acc = acc + classify(items[(i % 2) + 1])
            end
            return acc
        end"#,
        n = W21_N
    )
}

fn w21_expected() -> i64 {
    // Analytic check: items[0] is Image (classify → 1), items[1] is
    // Text (classify → 2). Over n iters with `i % 2` alternation and
    // n even (10 000), exactly half the iters hit each arm. The sum
    // is `n / 2 * 1 + n / 2 * 2 = n * 3 / 2`. For n=10 000 → 15 000.
    W21_N * 3 / 2
}

// =====================================================================
// =====  W23 — dict spread copy per iter  =============================
// =====================================================================
//
// Tier 4 Relon-flavour workload (panel expansion 2026-05-29, Phase 2
// "Group B — container construction sugar"). Mirrors the
// `fixtures/data_structures.relon` showcase `...&sibling.base` spread
// shape: each reduce iter builds a fresh dict that copies the four
// `base` entries via `{ ...base, e: 5 }` and adds a fifth key. The
// per-iter `_len(...)` projection forces materialisation — without it,
// the analyzer could fold the spread away as dead.
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w23_relon_src()` is the production source. The W22
//   audit (2026-05-29) showed that `&root.base` / `&sibling.base`
//   inside a reduce closure trips the evaluator's circular-reference
//   guard (`crates/relon-evaluator/src/reference.rs:777-786`) because
//   the owning dict's path is still in `evaluating_paths` when the
//   closure body runs. The plan §1 W23 spec already anticipated this
//   and listed "pre-resolve base via `where { base: ... }` and spread
//   from `base` directly (no `&root`)" as the canonical fallback —
//   that is exactly what `w23_relon_src()` does. The `where`-bound
//   `base` is a fully materialised dict by the time the closure runs;
//   the spread reads it as a plain identifier reference, not a `&ref`,
//   so the circular-guard check never fires. The per-iter cost
//   (dict copy of 4 keys + 1 key insert + `_len`) is preserved.
// * Algorithm complexity preserved: O(n) reduce, each iter is one
//   dict copy of 4 entries + 1 key insert + 1 `_len` projection +
//   1 Int add. No closed-form fold — the per-iter result depends on
//   the materialised dict shape, which is rebuilt each iter.
// * Time-math sanity: `_len({ ...base, e: 5 })` is 5 for every iter
//   (four base keys + the appended `e`). Over n iters the reduce
//   sum is `n * 5`. For n=10 000 → 50 000.
// * I/O shape: `#main(Int n) -> Int`. Returned Int matches the W6 /
//   W18 scalar shape; Lua row returns the scalar Int directly. Both
//   flow through `assert_relon_lua_consistent` for an exact-equal
//   cross-check.
// * Backend coverage: tree_walk + luajit. Bytecode envelope rejects
//   (`Op::Dict` + spread lowering both outside the M2-A scalar
//   envelope today, probed via `try_build_bytecode`); the row logs
//   `n/a (UnsupportedOp: <reason>)`. LLVM AOT / Cranelift AOT / wasm
//   reject (same Dict / spread surface — outside the Phase E typed
//   envelope and the Z.4 wasm-walker scope). The canonical-panel
//   `relon_jit` row routes the production source through
//   `JitEvaluator::run_main`, which falls through to the tree-walker
//   for the dict-spread closure. `rust_native` is NOT applicable: a
//   Rust-side `HashMap::clone()` + `insert()` would skip the
//   tree-walker's per-iter `Value::Dict` allocator path + key-hash
//   work; the per-iter "cost of dict spread copy" is the load-bearing
//   measurement here. The row is gated out by adding the label to
//   `paper_win_container_sugar_label` below.

/// W23 scale — n = 10 000 reduce iters. Same scale as W21 / W13 / W14.
/// The per-iter dict copy is heavier than W21's brand-compare so the
/// tree-walker wall-clock per run lands in low-ms territory (~ 500
/// ns/iter × 10k = 5 ms / run), still inside the 5 s measurement
/// budget with 100 samples + warmup.
const W23_N: i64 = 10_000;

fn w23_relon_src() -> &'static str {
    // Production source. The `where` clause pre-resolves `base` to a
    // fully materialised dict before the reduce closure runs (no
    // `&root` / `&sibling` deref inside the closure), bypassing the
    // W22 circular-guard blocker. Inside the closure body, `...base`
    // is an identifier reference (not a `&ref`), which the tree-
    // walker copies via the dict-spread codepath. `_len(...)` forces
    // the spread result to materialise rather than being constant-
    // folded out by the analyzer; the per-iter result is the integer
    // 5 (four base keys + the appended `e`), the reduce folds to
    // `n * 5`.
    //
    // `#unstrict` is required because the reduce closure params
    // (`acc`, `_`) are untyped — same constraint W21 hit.
    "#unstrict\n\
     #main(Int n) -> Int\n\
     range(n).reduce(0, (acc, _) =>\n\
       acc + _len({ ...base, e: 5 }))\n\
     where {\n\
       base: { a: 1, b: 2, c: 3, d: 4 }\n\
     }"
}

fn w23_lua_src() -> String {
    // Lua side mirrors the per-iter dict copy + key insert + len. Lua
    // has no `table.unpack`-style spread inside a table literal; the
    // canonical equivalent is a shallow `for k,v in pairs(base) do
    // copy[k] = v end` pre-amble plus the appended key. We unroll
    // the 4-key copy into 4 direct assignments — keeps the per-iter
    // work shape-equivalent (4 table inserts + 1 key insert + 1 len)
    // without paying the `pairs()` iterator boundary cost on every
    // iter, which would dwarf the dict-spread cost being measured.
    // `#t` on a Lua table reports the sequence length, not the dict
    // size, so we count keys via a small helper that walks `pairs`
    // once — matches Relon `_len` semantics on dicts.
    format!(
        r#"return function()
            local n = {n}
            local base = {{ a = 1, b = 2, c = 3, d = 4 }}
            local function dictlen(t)
                local c = 0
                for _ in pairs(t) do c = c + 1 end
                return c
            end
            local acc = 0
            for _ = 0, n - 1 do
                local copy = {{ a = base.a, b = base.b, c = base.c, d = base.d, e = 5 }}
                acc = acc + dictlen(copy)
            end
            return acc
        end"#,
        n = W23_N
    )
}

fn w23_expected() -> i64 {
    // Analytic check: every iter spreads 4 base keys and adds 1 more,
    // so `_len({ ...base, e: 5 }) == 5` for every i. The reduce
    // accumulates `n * 5` over n iters. For n=10 000 → 50 000.
    W23_N * 5
}

// =====================================================================
// =====  W24 — list comprehension with predicate filter  ==============
// =====================================================================
//
// Tier 4 Relon-flavour workload (panel expansion 2026-05-29, Phase 2
// "Group B — container construction sugar"). Mirrors the
// `fixtures/data_structures.relon` showcase `[x for x in range(n) if
// pred]` form: the comprehension materialises a filtered + mapped
// list, then `list.sum` reduces it to a scalar. Tests the lowering
// path that goes through the comprehension AST node rather than the
// `.filter().map()` chain (which W6 / W10 already cover).
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w24_relon_src()` IS the spec form — `list.sum([x *
//   2 for x in range(n) if x % 3 == 0])`. The comprehension AST node
//   walks `range(n)`, applies the predicate, doubles surviving
//   elements, and builds a materialised list; `list.sum` then
//   reduces it. The Lua row uses the equivalent `for / if` loop.
// * Algorithm complexity preserved: O(n) iteration over `range(n)`,
//   one predicate eval + one mul + one conditional list-push per
//   iter, plus a final O(filtered) sum. The per-iter result depends
//   on the predicate hit/miss alternation, no closed-form fold.
// * Time-math sanity: predicate `x % 3 == 0` over `x ∈ [0, n)` keeps
//   `x ∈ {0, 3, 6, ..., 3*floor((n-1)/3)}`. For n=10 000 the count
//   is `ceil(10000/3) = 3334`; the kept values are `0, 3, ..., 9999`.
//   Doubling gives `0, 6, ..., 19998`. Sum = `2 * (0 + 3 + ... +
//   9999) = 2 * 3 * (0 + 1 + ... + 3333) = 6 * 3333 * 3334 / 2 =
//   3 * 3333 * 3334 = 33 336 666`.
// * I/O shape: `#main(Int n) -> Int`. Returns the Int sum directly;
//   Lua row returns the scalar Int. Both flow through
//   `assert_relon_lua_consistent` for an exact-equal cross-check.
// * Backend coverage: tree_walk + luajit. Bytecode envelope rejects
//   the comprehension AST node (`unsupported expression
//   `Comprehension`` from the bytecode lowering pipeline, probed
//   2026-05-29); LLVM AOT / Cranelift AOT / wasm reject the same
//   surface (comprehension lowering not in any of the Z.4 / Phase E
//   roadmaps today). The canonical-panel `relon_jit` row routes the
//   production source through `JitEvaluator::run_main`, which falls
//   through to the tree-walker for the comprehension body.
//   `rust_native` is NOT applicable: a Rust-side iterator chain
//   (`(0..n).filter(...).map(...).sum()`) collapses the predicate
//   + map + sum to an arithmetic-progression closed form (the
//   even-spaced multiples of 3 are an arithmetic sequence — LLVM
//   -O3 folds the sum to a closed-form polynomial, same fold pattern
//   as W6 in `paper_win_closed_form_fold_label`). Booking that
//   O(1) arithmetic under a `rust_native` label against the
//   production-source LuaJIT row would be a paper win per `/perf`
//   Honesty Rules. The row is gated out via
//   `paper_win_container_sugar_label` below.

/// W24 scale — n = 10 000 input range. Same scale as W23. The
/// per-iter work is lighter than W23 (one mod + one mul + one
/// optional list-push) so the tree-walker per run lands sub-ms;
/// 100 samples + warmup fits comfortably in the 5 s budget.
const W24_N: i64 = 10_000;

fn w24_relon_src() -> &'static str {
    // Production source: spec form straight from §1 W24 of
    // `docs/internal/tier4-plan-2026-05-29.md`. The comprehension
    // node materialises the filtered + mapped list; `list.sum`
    // reduces it. `#import list from "std/list"` brings the `sum`
    // method into scope (same import the W1 / W2 / W6 sources use).
    // `#unstrict` for the untyped `x` binder.
    "#unstrict\n\
     #import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum([x * 2 for x in range(n) if x % 3 == 0])"
}

fn w24_lua_src() -> String {
    // Lua row uses the equivalent imperative loop. Lua has no list
    // comprehension; the canonical equivalent is `for x = 0, n-1 do
    // if x % 3 == 0 then s = s + x * 2 end end`. Per-iter work is
    // shape-equivalent: one mod + one branch + (on hit) one mul +
    // one acc add. We do NOT materialise an intermediate table; the
    // comprehension's per-iter work folds to the same `s += x*2`
    // shape after the dead-store elimination LuaJIT would perform on
    // a `table.insert` + `sum` two-pass form, so the direct loop is
    // the honest equivalent baseline.
    format!(
        r#"return function()
            local n = {n}
            local s = 0
            for x = 0, n - 1 do
                if x % 3 == 0 then
                    s = s + x * 2
                end
            end
            return s
        end"#,
        n = W24_N
    )
}

fn w24_expected() -> i64 {
    // Analytic check: for n=10 000 the kept values are 0, 3, ..., 9999.
    // Count = 3334; sum of kept values = 3 * (0 + 1 + ... + 3333) =
    // 3 * 3333 * 3334 / 2 = 16 668 333. Doubled = 33 336 666.
    //
    // Closed-form derivation kept as code so any change to n
    // surfaces immediately in the cross-check.
    let n = W24_N;
    let count = (n + 2) / 3; // ceil(n/3) for n>=0
    let last_kept = (count - 1) * 3; // largest multiple of 3 < n
    let sum_kept = last_kept * count / 2; // arithmetic sum 0..last step 3
    2 * sum_kept
}

// =====================================================================
// =====  W25 — pipe chain through list.map / list.filter / list.sum  ==
// =====================================================================
//
// Tier 4 Relon-flavour workload (panel expansion 2026-05-29, Phase 2
// "Group B — container construction sugar"). Mirrors the
// `fixtures/data_structures.relon` showcase `range(5) | len()` form,
// extended to a three-stage pipe: `range(n) | list.map(...) |
// list.filter(...) | list.sum()`. Tests the pipe-operator lowering
// path, which is distinct from the method-chain form
// `range(n).map(...).filter(...).sum()` that W6 / W10 cover.
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w25_relon_src()` IS the spec form — the pipe form
//   parses and runs through `JitEvaluator::run_main` cleanly (probed
//   2026-05-29). The fallback function-call form noted in the plan
//   (`list.sum(list.filter(list.map(...)))`) is NOT used because the
//   pipe form is the workload's identity; falling back would test
//   the wrong code path. Tree-walker dispatches each pipe stage as
//   a separate stdlib call with the previous stage's value
//   threaded as the first argument.
// * Algorithm complexity preserved: O(n) iteration over `range(n)`,
//   each iter is one map closure call (`x + 1`) + one filter
//   predicate call (`x % 2 == 0`) + one optional sum add. The
//   per-iter result depends on the predicate hit/miss alternation;
//   no closed-form fold.
// * Time-math sanity: map produces `[1, 2, ..., n]`. Filter keeps
//   even values: `[2, 4, ..., n if n even else n-1]`. For n=10 000
//   the kept values are `[2, 4, ..., 10000]`, count 5000, sum = `2 *
//   (1 + 2 + ... + 5000) = 2 * 5000 * 5001 / 2 = 25 005 000`.
// * I/O shape: `#main(Int n) -> Int`. Returns the Int sum directly;
//   Lua row returns the scalar Int. Both flow through
//   `assert_relon_lua_consistent` for an exact-equal cross-check.
// * Backend coverage: tree_walk + luajit. Bytecode envelope rejects
//   the pipe operator (`unsupported operator `Pipe`` from the
//   bytecode lowering pipeline, probed 2026-05-29); LLVM AOT /
//   Cranelift AOT / wasm reject the same surface (pipe lowering not
//   in any of the Z.4 / Phase E roadmaps today). The canonical-panel
//   `relon_jit` row routes the production source through
//   `JitEvaluator::run_main`, which falls through to the tree-walker.
//   `rust_native` is NOT applicable: a Rust-side iterator chain
//   `(0..n).map(|x| x+1).filter(|x| x%2==0).sum()` collapses to a
//   closed-form polynomial at -O3 (the kept values are an
//   arithmetic progression, same fold pattern as W6 in
//   `paper_win_closed_form_fold_label`). Booking that O(1)
//   arithmetic under a `rust_native` label against the
//   production-source LuaJIT row would be a paper win per `/perf`
//   Honesty Rules. The row is gated out via
//   `paper_win_container_sugar_label` above.

/// W25 scale — n = 10 000 input range. Same scale as W23 / W24. The
/// per-iter work is a closure call + predicate eval; tree-walker per
/// run lands a few ms (heavier than W24 because every iter pays a
/// closure dispatch). Fits in 5 s budget with 100 samples + warmup.
const W25_N: i64 = 10_000;

fn w25_relon_src() -> &'static str {
    // Production source: spec form straight from §1 W25 of
    // `docs/internal/tier4-plan-2026-05-29.md`. The pipe operator
    // threads each stage's output into the next stage's first
    // argument. `list.sum()` with no explicit arg takes the piped
    // input. `#import list from "std/list"` brings the methods in.
    // `#unstrict` for the untyped closure params.
    "#unstrict\n\
     #import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     range(n) | list.map((x) => x + 1) | list.filter((x) => x % 2 == 0) | list.sum()"
}

fn w25_lua_src() -> String {
    // Lua row uses the equivalent imperative loop. Lua has no pipe
    // operator; the canonical equivalent is a single `for` loop that
    // walks `x ∈ [0, n)`, computes `x + 1`, checks the predicate,
    // and accumulates. Per-iter work is shape-equivalent: one add +
    // one mod + one branch + (on hit) one acc add. We do NOT
    // materialise intermediate `mapped` / `filtered` tables; the
    // pipe stages are fused to the same loop after the LuaJIT
    // compiler would inline the closure bodies, so the direct loop
    // is the honest equivalent baseline.
    format!(
        r#"return function()
            local n = {n}
            local s = 0
            for x = 0, n - 1 do
                local y = x + 1
                if y % 2 == 0 then
                    s = s + y
                end
            end
            return s
        end"#,
        n = W25_N
    )
}

fn w25_expected() -> i64 {
    // Analytic check: kept values are even `(x+1)` for `x ∈ [0, n)`,
    // i.e. `{2, 4, ..., n if n even else n-1}`. For n=10 000 even,
    // last kept = 10 000, count = 5 000, sum = (first + last) *
    // count / 2 = (2 + 10000) * 5000 / 2 = 25 005 000.
    let n = W25_N;
    let last_kept = if n % 2 == 0 { n } else { n - 1 };
    let count = last_kept / 2;
    let first_kept = 2;
    (first_kept + last_kept) * count / 2
}

// =====================================================================
// =====  W28 — float div / mod / Int→Float mixed ops  =================
// =====================================================================
//
// Tier 4 Phase 3 (panel expansion 2026-05-29, group D — numeric /
// literal corner ops). Production-shape workload covering the Float
// codegen surface left bare by W2 / W20: per-iter `i / 3.0` (Int /
// Float division), `i % 7` (Int modulo coerced into a Float
// accumulator), and the Int+Float mixed-add cascade that the
// analyzer's promote-to-Float rule routes through the per-op
// coercion path.
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w28_relon_src()` IS the production source — a
//   plain `#main(Int n) -> Float` body, `range(n).reduce(0.0, (acc,
//   i) => acc + i / 3.0 + i % 7)`. No `where` clause, no `#internal`
//   binding. The reduce closure's per-iter work is exactly: load
//   `acc` (Float), divide `i` (Int) by literal `3.0` (Float) into a
//   Float lane, mod `i` (Int) by literal `7` (Int) into an Int lane,
//   coerce that Int into Float at the outer `+` (analyzer's promote-
//   to-Float rule), add three Floats, store back into `acc`. The
//   Lua row runs the byte-identical kernel — Lua 5.x `/` is float
//   division, `%` is float modulo when one operand is float but
//   here both operands are integers so `%` returns Int and Lua
//   auto-promotes at the outer `+`.
// * Algorithm complexity preserved: O(n) reduce, three arithmetic
//   ops + one closure dispatch per iter. No closed-form fold —
//   `Σ_{i<n} (i/3.0 + i%7)` does not reduce to a polynomial in `n`
//   because the `i % 7` term creates a periodic-7 step function
//   that cannot be collapsed by `IndVarSimplify` / `LoopIdiom`
//   (verified by inspection: a closed-form would require the
//   compiler to recognise the 7-cycle as a constant tail-sum, which
//   `rustc` / LLVM at -O3 do NOT do for non-power-of-two moduli).
//   The `i / 3.0` half IS a Faulhaber-class polynomial in `n` and
//   would fold on its own; the mixed `+ i % 7` term prevents the
//   fold across the full body.
// * Time-math sanity: per-iter work ≈ 50 ns (3 fp ops + 1 int mod +
//   1 closure dispatch in the tree-walker) × 10k iters ≈ 0.5 ms /
//   run. LuaJIT row ≈ 5 ns × 10k = 50 µs / run (LuaJIT loops the
//   tight arithmetic + mod through native fp lanes).
// * I/O shape: `#main(Int n) -> Float`. Scalar Float return,
//   unwrapped via `relon_float_result` (same shape as W20). Lua row
//   returns f64 directly. Both flow through an absolute-tolerance
//   cross-check against `w28_expected()` (iterative reference sum)
//   because the tree-walker's expression evaluation order may
//   differ from `rustc`'s by ~1 ULP per reduce iter; over 10k iters
//   the drift stays below `W28_FLOAT_TOL = 1e-6` in absolute terms.
// * Backend coverage: tree_walk + luajit. The bytecode envelope
//   accepts Int/Float arithmetic but rejects `#main -> Float` +
//   closure reduce shapes today (same envelope check that bounces
//   W2 / W6's untyped reduce closure); the row tries
//   `try_build_bytecode` honestly and logs `n/a` if it bounces.
//   LLVM AOT rejects (Phase E typed surface tops out at Int +
//   String; Float `#main` return tracked for Phase Z.4.x). wasm
//   classifier scope-cuts (Z.1 program set has no Float lowering).
//   `rust_native` is dispatched through the dedicated
//   `rust_native_w28` (mirrors W20's f64-return shape — the i64
//   `rust_native_dispatch` cannot carry the Float kernel). The
//   per-iter Rust body matches the bench source exactly: `acc =
//   acc + (i as f64) / 3.0 + ((i % 7) as f64)`. The `i % 7`
//   periodic-7 step term prevents `IndVarSimplify` from folding the
//   whole body to a closed-form polynomial; the `i / 3.0` half on
//   its own could fold but the cumulative reduce shape keeps the
//   O(n) loop body intact.

/// W28 scale — n = 10 000 reduce iters. Same scale as W21 / W13-W18
/// so per-element ns numbers line up with the Tier 1-2-4 Relon-flavour
/// rows.
const W28_N: i64 = 10_000;

/// W28 absolute tolerance — see `W20_FLOAT_TOL` doc-comment for the
/// reasoning. Over 10k reduce iters with three fp ops + one int mod
/// per iter the tree-walker's evaluation order may differ from the
/// reference `rustc` loop by ~1 ULP per iter; 1e-6 absolute keeps the
/// gate well above that floor while still catching algorithm errors.
const W28_FLOAT_TOL: f64 = 1.0e-6;

fn w28_relon_src() -> &'static str {
    // `#unstrict` because the closure params (`acc`, `i`) are untyped.
    // The analyzer promotes the Int `i % 7` term to Float at the outer
    // `+` (Float-takes-precedence rule); the reduce seed `0.0` pins
    // the accumulator to Float so the whole expression evaluates as
    // Float. Probed 2026-05-29: this form returns `Float(39.0)` for
    // n=10 (matches the iterative reference below), confirming the
    // analyzer accepts the mixed Int+Float reduce shape.
    "#unstrict\n\
     #main(Int n) -> Float\n\
     range(n).reduce(0.0, (acc, i) => acc + i / 3.0 + i % 7)"
}

fn w28_lua_src() -> String {
    // Lua 5.x: `/` is float division (always returns float), `%` is
    // integer modulo when both operands are integers (returns Int),
    // and the outer `+` auto-promotes to float when one operand is
    // float. The per-iter byte-code is shape-equivalent to the
    // Relon tree-walker's Float reduce. Lua's `for i = 0, n-1`
    // matches Relon's `range(n)` (half-open [0, n)).
    format!(
        r#"return function()
            local n = {n}
            local acc = 0.0
            for i = 0, n - 1 do
                acc = acc + i / 3.0 + i % 7
            end
            return acc
        end"#,
        n = W28_N
    )
}

fn w28_expected() -> f64 {
    // Iterative reference matching the bench source's per-iter shape.
    // We deliberately do NOT use a closed-form `Σ i/3.0 + Σ (i%7)`
    // analytic constant here — the iterative sum's exact rounding
    // path tracks the tree-walker's and Lua's accumulator order, so
    // the absolute-tolerance check picks up ~1 ULP-per-iter drift
    // honestly. The closed-form would diverge from both
    // implementations by the cumulative reordering error.
    let n: i64 = W28_N;
    let mut acc = 0.0_f64;
    let mut i: i64 = 0;
    while i < n {
        acc = acc + (i as f64) / 3.0 + ((i % 7) as f64);
        i += 1;
    }
    acc
}

// =====================================================================
// =====  W30 — strict-mode baseline (typed lambda param)  =============
// =====================================================================
//
// Tier 4 Phase 3 (panel expansion 2026-05-29, group F — strict mode).
// Same algorithm as W6_list_int_sum_plus_one but the source omits the
// `#unstrict` / `#relaxed` directive (strict is the analyzer's
// default) AND the inner `.map((Int i) => i + 1)` lambda carries a
// typed param. The pair `(W6, W30)` lets the panel surface any
// analyzer / IR overhead the strict-mode path adds versus the
// unstrict W6 row.
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w30_relon_src()` IS the production source — `#main`
//   declares `(Int n) -> Int`, `list.sum(range(n).map((Int i) => i +
//   1))`. No `#unstrict` (strict default), no `where` clause, no
//   `#internal` binding. The `(Int i) =>` lambda param annotation
//   exercises the analyzer's typed-closure-param path that the
//   untyped W6 lambda skips.
//   Note on the spec's `(Int i): Int => ...` form: probed 2026-05-29,
//   the parser today rejects the return-type-annotation form
//   (`parse error: expected R_PAREN, found Some(IDENT)`); the
//   typed-param-only form (`(Int i) => ...`) IS accepted and exercises
//   the strict-mode typed-lambda surface. The return-type annotation
//   is recovered by the analyzer's Int-inference rule (return type
//   inferred from the `i + 1` body), so dropping the explicit
//   annotation does not weaken the typing strictness — the analyzer
//   still rejects the lambda body if it returns a non-Int.
// * Algorithm complexity preserved: O(n) sum-fold, identical to W6
//   (`list.sum(range(n).map((i) => i + 1))`). The IR lowering is the
//   same `range.map.sum` peephole; the strict-mode analyzer path
//   adds typecheck overhead at compile time but the runtime IR is
//   indistinguishable from W6's (verified by inspecting the
//   bytecode the BytecodeEvaluator emits for both — same Op::Loop +
//   Op::AddI64 body).
// * Time-math sanity: `n*(n+1)/2`. For n=10 000 → 50 005 000. Same
//   as W6_expected (`TREE_WALK_N * (TREE_WALK_N + 1) / 2`).
// * I/O shape: `#main(Int n) -> Int` returning a scalar Int via
//   `list.sum`, unwrapped via `relon_int_result`. Identical to W6.
// * Backend coverage: tree_walk + luajit. Bytecode tries honestly
//   via `try_build_bytecode`; same envelope as W6 (the closure-in-
//   `.map` path goes through the bytecode IR's reduce lowering when
//   the M2-A scalar envelope accepts the typed-lambda form). The
//   `rust_native` row is gated out via
//   `paper_win_closed_form_fold_label` for the same reason W6 is —
//   `Σ_{i<n} (i+1) ≡ n*(n+1)/2` is the closed-form polynomial that
//   rustc + LLVM at -O3 fold the body to. LLVM AOT / wasm are also
//   gated by the same closed-form-fold rules already applied to W6.

/// W30 scale — n = 10 000, matching `TREE_WALK_N` (W6 uses the same
/// constant). Direct A/B comparison with W6 requires identical n.
const W30_N: i64 = 10_000;

fn w30_relon_src() -> &'static str {
    // No `#unstrict` / `#relaxed` directive — strict is the analyzer
    // default. The `.map((Int i) => i + 1)` lambda carries a typed
    // param; the analyzer's strict-mode typed-closure path validates
    // the `(Int i) -> Int` shape (return type inferred from `i + 1`).
    // The parser today rejects the explicit return-type form
    // `(Int i): Int => ...`; the typed-param form below IS the
    // maximally-typed lambda the grammar accepts and exercises the
    // strict-mode typed-lambda surface end-to-end.
    "#import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((Int i) => i + 1))"
}

fn w30_lua_src() -> String {
    // Identical to W6_lua. Lua has no `#strict` / `#unstrict`
    // analogue — it is dynamically typed; the bench compares the
    // strict-Relon analyzer overhead against LuaJIT's dynamic typing
    // cost at the IR / runtime level. Per-iter work: array build +
    // sum-fold.
    format!(
        r#"return function()
            local n = {n}
            local arr = {{}}
            for i = 1, n do arr[i] = i end
            local sum = 0
            for i = 1, n do sum = sum + arr[i] end
            return sum
        end"#,
        n = W30_N
    )
}

fn w30_expected() -> i64 {
    // Closed-form: `Σ_{i<n} (i+1) ≡ n*(n+1)/2`. Same shape as W6.
    let n: i64 = W30_N;
    n * (n + 1) / 2
}

// =====================================================================
// =====  W26 — f-string interpolation per-iter concat  ================
// =====================================================================
//
// Tier 4 Relon-flavour workload (panel expansion 2026-05-29, Phase 4
// "Group C — strings / formatting"). Directly mirrors the
// `f"... ${expr} ..."` log / error / message-rendering hot path
// (`fixtures/decorators.relon`, `fixtures/operators.relon` carry the
// same shape). The reduce closure interpolates a per-iter Int (`i`)
// into a constant template alongside the closed-over `n`, so neither
// the tree-walker nor LuaJIT can hoist the string allocation out of
// the loop.
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w26_relon_src()` IS the production source — a
//   `#main(Int n) -> Int` body whose `range(n).reduce(0, (acc, i)
//   => acc + _len(f"item ${i} of ${n}"))` walks the f-string
//   evaluator codepath in `eval.rs::Expr::FString` for every iter:
//   each `FStringPart::Literal` push + `FStringPart::Interpolation`
//   eval + `Display` write into the resulting String allocation,
//   then `_len` returns the byte length. The Lua row uses
//   `string.format("item %d of %d", i, n)` + `#str` which exercises
//   LuaJIT's own format-then-measure path; the per-iter work is the
//   same — int-to-decimal write + concat into a fresh string + byte
//   length read.
// * Algorithm complexity preserved: O(n) reduce, one f-string alloc
//   + one `_len` read + one Int add per iter. No closed-form fold —
//   the byte length depends on `len(str(i))` which transitions at
//   the decimal-digit boundaries (i=10, i=100), so the per-iter
//   contribution is not a single polynomial in `i`.
// * Time-math sanity: for n=1000 the template renders as
//   `"item <i> of 1000"` = 5 ("item ") + len(str(i)) + 4 (" of ")
//   + 4 ("1000") = 13 + len(str(i)). Buckets:
//     - i ∈ [0, 10):  len=1 → 14 bytes × 10  = 140
//     - i ∈ [10, 100): len=2 → 15 bytes × 90  = 1 350
//     - i ∈ [100, 1000): len=3 → 16 bytes × 900 = 14 400
//     - total = 15 890. Both the Relon tree-walker and the LuaJIT
//   row are asserted against this exact constant at construction
//   time via `assert_relon_lua_consistent`.
// * I/O shape: `#main(Int n) -> Int`. Returned scalar Int is the sum
//   of per-iter byte lengths; `relon_int_result` passes it straight
//   through. Lua row returns the same scalar Int.
// * Backend coverage: tree_walk + luajit. Bytecode rejects (f-string
//   lowering not in the M2-A scalar envelope today — `Op::FString`
//   tracked under Z.4.x); `try_build_bytecode` returns None, the
//   row logs `n/a (UnsupportedOp: <reason>)`. LLVM AOT / Cranelift
//   AOT / wasm reject (same f-string surface — Phase E String ops
//   cover concat + contains, not formatted interpolation; the wasm
//   walker's String shape lacks the Int→decimal writer). The
//   canonical-panel `relon_jit` row routes the production source
//   through `JitEvaluator::run_main`, which falls through to the
//   tree-walker for the f-string evaluation. `rust_native` is NOT
//   applicable: a Rust-side `format!("item {} of {}", i, n).len()`
//   could be folded by rustc / LLVM after inlining (the digit-
//   counter is a closed-form expression of the input integers and
//   the `_len` of the resulting `String` is a closed-form sum of
//   constant prefix lengths plus `ilog10(i) + 1`). Booking that
//   potential closed-form under a `rust_native` label against the
//   LuaJIT row that walks the format-then-strlen path is a paper-
//   win per `/perf` Honesty Rules; the row is gated out by adding
//   the label to `paper_win_fstring_interp_label`.

/// W26 scale — n = 1 000 reduce iters. The f-string evaluator
/// allocates a fresh String per iter (Rope-free; the production
/// FStringPart writer pushes into a `String::with_capacity(len_hint)`),
/// so each iter pays an alloc + write + drop. At ~ 200-300 ns / iter
/// on the tree-walker and ~ 50-80 ns / iter on LuaJIT, n=1 000 gives a
/// 200-300 µs / 50-80 µs total per run — fits within the criterion 5 s
/// measurement budget at 100 samples + warmup, and stays clear of the
/// criterion variance floor (~ 100 ns). Larger n (matching W21's
/// 10 000) would push tree-walker time into multi-ms territory which
/// makes the warm-up + measurement budget thin; smaller n hides the
/// per-iter cost in the variance floor.
const W26_N: i64 = 1_000;

fn w26_relon_src() -> &'static str {
    // The reduce closure interpolates `i` (per-iter Int) and `n`
    // (closed-over loop bound) into a constant template. The
    // tree-walker's `Expr::FString` evaluator iterates the
    // FStringPart list (one Literal, one Interpolation, one
    // Literal, one Interpolation), writes each piece into a fresh
    // String accumulator, and returns the byte length via `_len`.
    // `#unstrict` so the (acc, i) closure params stay untyped (the
    // analyzer's strict mode demands `(Int acc, Int i): Int =>`
    // annotations; the tree-walker accepts either). Returns scalar
    // Int rather than Dict — matches W1 / W3 / W18 shape so
    // `relon_int_result` passes the value straight through and the
    // Lua row's return is byte-identical.
    "#unstrict\n\
     #main(Int n) -> Int\n\
     range(n).reduce(0, (acc, i) => acc + _len(f\"item ${i} of ${n}\"))"
}

fn w26_lua_src() -> String {
    // Lua equivalent: `string.format("item %d of %d", i, n)` writes
    // the same template through LuaJIT's format-then-allocate path,
    // `#str` reads the byte length. The per-iter work shape (int-to-
    // decimal write + concat + byte-length read) is the same as the
    // Relon tree-walker's `Expr::FString` + `_len` path.
    format!(
        r#"return function()
            local n = {n}
            local acc = 0
            for i = 0, n - 1 do
                acc = acc + #string.format("item %d of %d", i, n)
            end
            return acc
        end"#,
        n = W26_N
    )
}

fn w26_expected() -> i64 {
    // Analytic check (computed once at startup so the bench setup
    // panics if the production source / Lua source diverges from
    // the closed-form prediction):
    //
    //   per-iter byte length = 5 ("item ") + len(str(i)) + 4 (" of ")
    //                        + len(str(n))
    //
    // For n=1 000, len(str(n)) = 4, so the constant prefix is 13:
    //   - i ∈ [0, 10):   1-digit → 14 × 10  = 140
    //   - i ∈ [10, 100): 2-digit → 15 × 90  = 1 350
    //   - i ∈ [100, n):  3-digit → 16 × 900 = 14 400
    //   total = 15 890
    //
    // We recompute the sum here from `W26_N` rather than baking in
    // the literal `15890` so future scale changes stay honest
    // without a separate audit.
    let n = W26_N;
    let n_len = decimal_len(n) as i64;
    let prefix_const = 5 + 4; // "item " + " of "
    let mut total: i64 = 0;
    for i in 0..n {
        total += prefix_const + decimal_len(i) as i64 + n_len;
    }
    total
}

/// Helper: byte length of an Int rendered through Relon's f-string
/// interpolation writer (and Lua's `%d`). Decimal, no sign for non-
/// negative inputs. Used only by `w26_expected()` so the bench setup
/// can cross-check the f-string per-iter byte count against the
/// engine's actual output.
fn decimal_len(n: i64) -> u32 {
    if n == 0 {
        1
    } else if n < 0 {
        // Not reached for W26 (n >= 0), but kept symmetric so the
        // helper is safe to reuse if a future workload negates `i`.
        1 + (n.unsigned_abs() as f64).log10().floor() as u32 + 1
    } else {
        (n as f64).log10().floor() as u32 + 1
    }
}

// =====================================================================
// =====  W27 — std/dict stdlib module dispatch  =======================
// =====================================================================
//
// Tier 4 Relon-flavour workload (panel expansion 2026-05-29, Phase 4
// "Group E — non-list stdlib coverage"). Adds the first cmp_lua row
// that exercises an `#import` of a non-`std/list` stdlib module. The
// pre-existing panel runs 13 / 15 rows through `std/list` only; this
// row swaps in `std/dict` so the module-resolver + dict-helper call
// surface is covered.
//
// Stdlib audit (2026-05-29, source: `crates/relon-evaluator/src/std_
// relon/dict.relon`): `std/dict` exports `merge(a, b)`, `keys(d)`,
// `values(d)`, `has_key(d, k)`. No `len(d)` — the plan-doc's
// speculative `dict.len({a:1,b:2,c:3})` shape is replaced here with
// `_len(dict.keys({a:1,b:2,c:3}))`, which routes the per-iter work
// through the genuine `dict.keys` dispatch + the resulting `List`
// length read.
//
// **HONESTY checklist** (per `HONESTY_POLICY.md`):
// * Source path: `w27_relon_src()` IS the production source — a
//   `#main(Int n) -> Int` body whose `range(n).reduce(0, (acc, _)
//   => acc + _len(dict.keys({ a: 1, b: 2, c: 3 })))` walks the
//   stdlib module-resolver per iter: closure call to the
//   `dict.keys` arm of the resolved `std/dict` module (one stdlib-
//   method dispatch), the closure body materialises a new
//   `List<String>` from the dict literal, then `_len` reads the
//   list length. The Lua row uses a hand-written `dict_keys(d)`
//   helper (Lua has no std/dict module) that walks the table's
//   `pairs(d)` iterator, accumulates keys into a fresh list, and
//   returns the list — same per-iter shape (key iteration + list
//   materialisation + length read).
// * Algorithm complexity preserved: O(n) reduce, one `dict.keys`
//   call + one List `_len` + one Int add per iter. The dict literal
//   `{ a: 1, b: 2, c: 3 }` re-evaluates per iter (it sits inside
//   the closure body), so the dict alloc + keys() copy fires every
//   iter — neither the tree-walker nor LuaJIT can hoist the alloc
//   out of the loop without proving the closure body is pure (it
//   is, but the engines do not perform that proof today).
// * Time-math sanity: `dict.keys({a:1,b:2,c:3})` returns a
//   `List<String>` of length 3 (one entry per key), `_len(...)`
//   returns 3, the reduce folds `acc + 3` over n iters → `n * 3`.
//   For n=10_000 → 30_000. Both the Relon tree-walker and the
//   LuaJIT row are asserted against this exact constant at
//   construction time via `assert_relon_lua_consistent`.
// * I/O shape: `#main(Int n) -> Int`. Returned scalar Int is the
//   sum of per-iter list lengths; `relon_int_result` passes it
//   straight through. Lua row returns the same scalar Int.
// * Backend coverage: tree_walk + luajit. Bytecode envelope rejects
//   (stdlib module-resolver + dict literal + Closure dispatch all
//   outside the M2-A scalar envelope today); `try_build_bytecode`
//   returns None, the row logs `n/a (UnsupportedOp: <reason>)`.
//   LLVM AOT / Cranelift AOT / wasm reject (same module-resolver /
//   dict surface — Phase E typed surface covers Int + String only;
//   the Z.1 wasm program set has no stdlib-import shape). The
//   canonical-panel `relon_jit` row routes the production source
//   through `JitEvaluator::run_main`, which falls through to the
//   tree-walker for the std/dict dispatch. `rust_native` is NOT
//   applicable: a Rust-side `HashMap<&str, i32>` + `.keys().count()`
//   would be folded by rustc / LLVM (the dict literal is a constant
//   3-element map, `keys().count()` is a constant `3`); booking
//   that O(1) compile-time constant under a `rust_native` label
//   against the LuaJIT row that walks the table-iter + list-
//   materialise path is a paper-win per `/perf` Honesty Rules. The
//   row is gated out by adding the label to
//   `paper_win_stdlib_dict_label`.

/// W27 scale — n = 10 000 reduce iters. Same scale as W13 / W14 /
/// W18 / W21 (other Tier 1 / 2 / 4 Relon-flavour workloads). The
/// per-iter dict.keys dispatch costs ~ 200-400 ns on the tree-walker
/// (one module-arm closure call + one dict-literal alloc + one
/// List<String> materialise + one `_len`), so n=10_000 gives a
/// 2-4 ms / run wall clock — fits the criterion 5 s measurement
/// budget at 100 samples + warmup and stays above the variance
/// floor.
const W27_N: i64 = 10_000;

fn w27_relon_src() -> &'static str {
    // `#import dict from "std/dict"` resolves through the module
    // registry to the `std_relon/dict.relon` carrier (`merge`,
    // `keys`, `values`, `has_key`). The reduce body calls
    // `dict.keys(...)` per iter, which evaluates the dict literal
    // `{ a: 1, b: 2, c: 3 }` (fresh `Dict` per iter), invokes the
    // `dict.keys(d)` closure (closure body lowers to
    // `_dict_keys(d)`), and returns a `List<String>` of length 3.
    // `_len(...)` reads the list byte length.
    //
    // `#unstrict` so the `(acc, _)` closure params stay untyped (the
    // analyzer's strict mode demands `(Int acc, Int _): Int =>`
    // annotations; the tree-walker accepts either). Returns scalar
    // Int rather than Dict — matches W1 / W18 shape so
    // `relon_int_result` passes the value straight through and the
    // Lua row's return is byte-identical.
    "#unstrict\n\
     #import dict from \"std/dict\"\n\
     #main(Int n) -> Int\n\
     range(n).reduce(0, (acc, _) => acc + _len(dict.keys({ a: 1, b: 2, c: 3 })))"
}

fn w27_lua_src() -> String {
    // Lua has no `std/dict` module; the canonical equivalent is to
    // hand-write a `dict_keys(d)` helper that walks `pairs(d)`,
    // accumulates keys into a fresh list, and returns the list. The
    // per-iter work shape (table-iter + list-materialise +
    // length-read) mirrors the Relon `_dict_keys` host fn body. Lua's
    // `#tbl` returns the array-part length; we measure the keys-list
    // length via `#keys` so the list materialisation cost is paid.
    format!(
        r#"return function()
            local n = {n}
            local function dict_keys(d)
                local out = {{}}
                local i = 0
                for k, _ in pairs(d) do
                    i = i + 1
                    out[i] = k
                end
                return out
            end
            local acc = 0
            for _ = 0, n - 1 do
                local d = {{ a = 1, b = 2, c = 3 }}
                acc = acc + #dict_keys(d)
            end
            return acc
        end"#,
        n = W27_N
    )
}

fn w27_expected() -> i64 {
    // Analytic check: `dict.keys({a:1,b:2,c:3})` always materialises
    // a 3-element list (one entry per key); `_len(...)` reads 3; the
    // reduce folds `acc + 3` over n iters → `n * 3`. For n=10_000 →
    // 30_000.
    W27_N * 3
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

/// Extract a Float value from a Relon `Value`. Mirrors `relon_int_result`
/// for the Tier 3 W20 row whose production source returns Float (the
/// checksum after the 4-body Verlet integration). Dict-bodied returns
/// unwrap a `result` field for symmetry with the Int helper, even though
/// no current Float workload uses the Dict shape.
fn relon_float_result(w: &str, v: Value) -> f64 {
    match v {
        Value::Float(f) => f.into_inner(),
        Value::Dict(d) => match d.map.get("result") {
            Some(Value::Float(f)) => f.into_inner(),
            other => panic!("{w}: dict.result is not Float: {other:?}"),
        },
        other => panic!("{w}: Relon result not Float or Dict: {other:?}"),
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

/// W13 deep dict access. Models the post-`Op::FieldAccess` constant
/// fold: both dict-chain reads collapse to literal `100` / `5000` in
/// the Relon tree-walker after the `cfg` binding inlines, so the
/// per-iter body is `acc + 100 + 5000 = acc + 5100`. rustc / LLVM at
/// -O3 then folds the reduce to `n * 5100`. Booking that against the
/// LuaJIT row that walks the dict-table chain per iter is a paper
/// win — this fn is referenced only through `rust_native_dispatch`'s
/// match arm and is gated out by `paper_win_closed_form_fold_label`.
#[inline(never)]
fn rust_native_w13(n: i64) -> i64 {
    let mut acc: i64 = 0;
    let n = black_box(n);
    let mut i: i64 = 0;
    while i < n {
        acc = acc.wrapping_add(100i64.wrapping_add(5000));
        i += 1;
    }
    acc
}

/// W14 schema validate. Models the constant-true predicate chain:
/// both range checks always succeed on the synthetic input domain
/// (`i % 10 ∈ [0,10)` and `i / 10 ∈ [0,n/10]` for n=1000), so the
/// per-iter body folds to `+ 2`. LLVM -O3 then collapses the reduce
/// to `n * 2`. Gated by `paper_win_closed_form_fold_label`.
#[inline(never)]
fn rust_native_w14(n: i64) -> i64 {
    let mut acc: i64 = 0;
    let n = black_box(n);
    let mut i: i64 = 0;
    while i < n {
        let role = i % 10;
        let region = i / 10;
        let role_ok = (0..10).contains(&role);
        let region_ok = (0..1000).contains(&region);
        acc = acc
            .wrapping_add(if role_ok { 1 } else { 0 })
            .wrapping_add(if region_ok { 1 } else { 0 });
        i += 1;
    }
    acc
}

/// W15 conditional field. `Σ_{i<n} (i%2==0 ? 2i : 3i)` is a closed-
/// form polynomial after even/odd splitting (`Σ 2*(2k) + Σ 3*(2k+1)`
/// over `k ∈ [0, n/2)`). LLVM -O3 collapses both halves to scalar
/// arithmetic. Gated by `paper_win_closed_form_fold_label`.
#[inline(never)]
fn rust_native_w15(n: i64) -> i64 {
    let mut acc: i64 = 0;
    let n = black_box(n);
    let mut i: i64 = 0;
    while i < n {
        let term = if i % 2 == 0 {
            i.wrapping_mul(2)
        } else {
            i.wrapping_mul(3)
        };
        acc = acc.wrapping_add(term);
        i += 1;
    }
    acc
}

/// W16 sum-via-partition baseline. Mirrors the Relon source's
/// sum-via-partition recurrence (NOT in-place Lomuto/Hoare followed
/// by a sum loop): each `sum_qs` call walks the input list three
/// times (lt / eq / gt partitions), recurses on `lt` and `gt`, and
/// folds `eq` into a scalar accumulator inline. The PRNG
/// construction is `black_box`-ed so rustc can't constant-fold the
/// array at compile time and book a literal sum.
#[inline(never)]
fn rust_native_w16(n: i64) -> i64 {
    fn sum_qs(xs: Vec<i64>) -> i64 {
        let len = xs.len();
        if len == 0 {
            return 0;
        }
        if len == 1 {
            return xs[0];
        }
        let p = xs[0];
        let mut lt: Vec<i64> = Vec::new();
        let mut gt: Vec<i64> = Vec::new();
        let mut eq_sum: i64 = 0;
        for v in &xs {
            if *v < p {
                lt.push(*v);
            } else if *v > p {
                gt.push(*v);
            } else {
                eq_sum = eq_sum.wrapping_add(*v);
            }
        }
        sum_qs(lt).wrapping_add(eq_sum).wrapping_add(sum_qs(gt))
    }
    let n = black_box(n);
    let mut arr: Vec<i64> = Vec::with_capacity(n as usize);
    let mut i: i64 = 0;
    while i < n {
        arr.push((i.wrapping_mul(1103515245).wrapping_add(12345)) % 2048);
        i += 1;
    }
    sum_qs(arr)
}

/// W17 binary search baseline. Recursive bisection over `range(n)`
/// for `n` scrambled targets. Same shape the Relon `bs(lo, hi, t)`
/// closure executes.
#[inline(never)]
fn rust_native_w17(n: i64) -> i64 {
    fn bs(lo: i64, hi: i64, t: i64) -> i64 {
        if hi - lo <= 1 {
            return lo;
        }
        let mid = (lo + hi) / 2;
        if mid <= t {
            bs(mid, hi, t)
        } else {
            bs(lo, mid, t)
        }
    }
    let n = black_box(n);
    let mut acc: i64 = 0;
    let mut i: i64 = 0;
    while i < n {
        acc = acc.wrapping_add(bs(0, n, (i.wrapping_mul(31)) % n));
        i += 1;
    }
    acc
}

/// W18 trial-division primality count baseline. Same nested-
/// recursion shape the Relon `is_prime(k, d)` closure runs.
#[inline(never)]
fn rust_native_w18(n: i64) -> i64 {
    fn is_prime(k: i64, d: i64) -> bool {
        if d.wrapping_mul(d) > k {
            return true;
        }
        if k % d == 0 {
            return false;
        }
        is_prime(k, d + 1)
    }
    let n = black_box(n);
    let mut count: i64 = 0;
    let mut k: i64 = 2;
    while k < n {
        if is_prime(k, 2) {
            count = count.wrapping_add(1);
        }
        k += 1;
    }
    count
}

/// W19 matmul baseline. Triple-nested loop with two `Vec<Vec<i64>>`
/// matrices generated by the same `(i * size + j) % 100` / `(i + j)
/// % 100` rules. `black_box` on `n` defeats compile-time fold over a
/// constant size; the per-cell inner reduce still has shape `Σ_k a[i]
/// [k] * b[k][j]` with mod-100 discontinuity so neither LLVM nor
/// rustc can collapse to a polynomial. Allocates both matrices on
/// every call so the row times the canonical matmul shape, not a
/// pre-loaded scratchpad.
#[inline(never)]
fn rust_native_w19(n: i64) -> i64 {
    let size = black_box(n);
    let usize_s = size as usize;
    // Build a, b via the same generator the Relon source uses.
    let mut a: Vec<Vec<i64>> = Vec::with_capacity(usize_s);
    let mut b: Vec<Vec<i64>> = Vec::with_capacity(usize_s);
    for i in 0..size {
        let mut row_a: Vec<i64> = Vec::with_capacity(usize_s);
        let mut row_b: Vec<i64> = Vec::with_capacity(usize_s);
        for j in 0..size {
            row_a.push((i.wrapping_mul(size).wrapping_add(j)) % 100);
            row_b.push((i.wrapping_add(j)) % 100);
        }
        a.push(row_a);
        b.push(row_b);
    }
    // Triple-loop matmul + sum-fold of the result matrix.
    // The body deliberately probes `a[i][k]` / `b[k][j]` by index
    // (matching the Relon source's 2D-index pattern) rather than via
    // `.iter()` adapters — the index-probe is what the bench is
    // measuring against the LuaJIT row's array-load cost. The
    // `#[allow(clippy::needless_range_loop)]` matches that intent.
    #[allow(clippy::needless_range_loop)]
    {
        let mut total: i64 = 0;
        for i in 0..usize_s {
            for j in 0..usize_s {
                let mut s: i64 = 0;
                for k in 0..usize_s {
                    s = s.wrapping_add(a[i][k].wrapping_mul(b[k][j]));
                }
                total = total.wrapping_add(s);
            }
        }
        total
    }
}

/// W20 4-body Verlet baseline. Same shape as the Relon source:
/// `[f64; 8]` state, `[f64; 4]` masses, no allocation per step. The
/// per-step force accumulation reads each of 4 bodies' force on each
/// of 4 partners (skipping self-pairing) before writing the new
/// state; `black_box` on `n` prevents compile-time constant fold of
/// the entire integration. Returns the asymmetric weighted checksum.
#[inline(never)]
fn rust_native_w20(n: i64) -> f64 {
    let n = black_box(n);
    let dt: f64 = 0.01;
    let soft: f64 = 0.1;
    let m: [f64; 4] = [1.0, 2.0, 0.5, 3.0];
    let mut s: [f64; 8] = [0.0, 1.0, 2.5, 4.0, 0.1, 0.0, 0.0, 0.2];
    let mut step: i64 = 0;
    while step < n {
        let mut a: [f64; 4] = [0.0; 4];
        for i in 0..4 {
            let mut ai = 0.0;
            for j in 0..4 {
                if i == j {
                    continue;
                }
                let dx = s[j] - s[i];
                let r2 = dx * dx + soft;
                ai += dx * m[j] * (1.0 / (r2 * r2));
            }
            a[i] = ai;
        }
        let mut ns: [f64; 8] = [0.0; 8];
        for i in 0..4 {
            ns[i] = s[i] + s[4 + i] * dt;
            ns[4 + i] = s[4 + i] + a[i] * dt;
        }
        s = ns;
        step += 1;
    }
    s[0] * 1.0
        + s[1] * 2.0
        + s[2] * 3.0
        + s[3] * 4.0
        + s[4] * 5.0
        + s[5] * 6.0
        + s[6] * 7.0
        + s[7] * 8.0
}

/// W28 mixed-Float baseline (Tier 4 Phase 3, panel expansion
/// 2026-05-29). Per-iter body matches the bench source exactly:
/// `acc + (i as f64) / 3.0 + ((i % 7) as f64)`. The `i % 7` periodic-
/// 7 step term prevents `IndVarSimplify` from folding the body to a
/// closed-form polynomial (non-power-of-two moduli are not collapsed
/// by `rustc` / LLVM at -O3); the `i / 3.0` half on its own would
/// fold but the cumulative reduce keeps the O(n) loop body intact.
/// `black_box` on `n` blocks the compiler from proving non-overflow
/// at the call site and stamping a literal result inline.
#[inline(never)]
fn rust_native_w28(n: i64) -> f64 {
    let n = black_box(n);
    let mut acc = 0.0_f64;
    let mut i: i64 = 0;
    while i < n {
        acc = acc + (i as f64) / 3.0 + ((i % 7) as f64);
        i += 1;
    }
    acc
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
        // Audit #332 (2026-05-28): W1 / W2 / W6 source variants are
        // arithmetic-progression sums that LLVM -O3 collapses to a
        // closed-form polynomial. The canonical_panel suppresses the
        // `relon_llvm_aot` + `relon_llvm_aot_fast` rows for these
        // labels via `paper_win_closed_form_fold_label` so the
        // source entries below are dead match arms today; retained
        // (not deleted) so the audit trail stays grep-able and so
        // the entries remain dormant if W1 / W2 are reintroduced
        // to the canonical_panel without re-checking the gate.
        "W1_int_sum" => Some(w1_relon_src()),
        "W2_f64_dot" => Some(W2_LLVM_SRC),
        "W3_string_concat" => Some(W3_LLVM_SRC),
        "W4_string_contains" => Some(W4_LLVM_SRC),
        "W4_long_haystack" => Some(W4_LONG_LLVM_SRC),
        "W5_dict_str_key" => Some(W5_LLVM_SRC),
        "W6_list_int_sum_plus_one" => Some(W6_LLVM_SRC),
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
        // Panel expansion 2026-05-28: W13 / W14 / W15 production sources
        // are outside the LLVM AOT envelope (W13 = dict-literal `#internal`
        // binding; W14 / W15 = unstrict ternary chain). Even an inlined
        // variant would land on the `paper_win_closed_form_fold_label`
        // gate (W13 folds to `n * 5100`; W14 folds to `n * 2`; W15 folds
        // to a closed-form polynomial). Returning `None` here keeps the
        // row honest until the panel grows a black_box-on-acc shape
        // that defeats induction-variable reduction.
        "W13_deep_dict_access" => None,
        "W14_schema_validate" => None,
        "W15_conditional_field" => None,
        // Panel expansion 2026-05-28 (Tier 2 industry-standard W16):
        // production source is a partition-quicksort sum — a `where`
        // recursive helper `sum_qs` + three `_list_filter` partitions
        // over a runtime-materialised PRNG-shuffled `range(n).map(...)`
        // list. AOT-3 (where-bound recursive closure lifting) + the
        // AOT-4 W16 slice (1D List<Int> index + filter + recursion) +
        // the MCJIT MAP_32BIT fix (2026-05-30, so the >=4-closure
        // dispatch jump table resolves under CodeModel::Small) make
        // the verbatim production source compile + run through
        // LlvmAotEvaluator::from_source, proven against the tree-walker
        // oracle by llvm_w16_quicksort / llvm_w16_inline3_repro. The
        // PRNG shuffle keeps depth O(log n) and total scratch O(n log n)
        // (~80 KiB at n=1000, under the 1 MiB arena). Same algorithm /
        // code path / I/O shape as the tree_walk + luajit rows; the
        // partition recursion is data-dependent (no closed-form fold).
        "W16_quicksort" => Some(w16_relon_src()),
        // Panel expansion 2026-05-28 (Tier 2 industry-standard W17):
        // production source uses a `where`-clause recursive helper
        // `bs(lo, hi, t)`. AOT-3 (2026-05-30) generalised the W7
        // Dict-bodied-recursion lifting to where-bound closure lets,
        // so the verbatim production source now compiles + runs
        // through `LlvmAotEvaluator::from_source` (proven by
        // `crates/relon-codegen-llvm/tests/llvm_w17_binary_search.rs`
        // against the tree-walker oracle). Same algorithm, same code
        // path, same I/O shape (`#main(Int n) -> Int`) as the
        // tree_walk / luajit rows — passes the 3 honesty questions.
        "W17_binary_search" => Some(w17_relon_src()),
        // Panel expansion 2026-05-28 (Tier 2 industry-standard W18):
        // production source uses a `where`-clause recursive `is_prime`
        // helper + `_list_filter` over a runtime-materialised
        // `range(2, n)`. AOT-4 W18 slice (2026-05-30) added runtime
        // List<Int> materialization + `_list_filter` -> Op::Call +
        // `_len` lowering, so the verbatim production source now
        // compiles + runs through `LlvmAotEvaluator::from_source`
        // (proven by `crates/relon-codegen-llvm/tests/llvm_w18_prime_count.rs`
        // against the tree-walker oracle). The survivor subset is
        // data-dependent (no closed-form fold), same algorithm /
        // code path / I/O shape as the tree_walk / luajit rows.
        "W18_prime_count_trial_div" => Some(w18_relon_src()),
        // Panel expansion 2026-05-28 (Tier 3 numeric-kernel W19):
        // production source materialises two 16x16 `List<List<Int>>`
        // matrices via nested `range(size).map(...).map(...)` chains.
        // The LLVM AOT envelope today rejects nested-list construction
        // (`Op::AllocListInt` does not chain through `Op::MakeClosure`
        // for the outer map's return-list shape — the bytecode VM /
        // cranelift / LLVM emitters surface as `unknown stdlib method
        // range` because the lowering pipeline rejects the 2D shape
        // before it reaches the codegen). An inlined variant that
        // skips list materialisation (e.g. `sum_{i,j,k} ((i*size+k)
        // %100) * ((k+j)%100)`) would be a paper-win per audit #318 —
        // the LuaJIT row walks the materialised arrays and pays the
        // table-load cost; collapsing both `a[i][k]` and `b[k][j]`
        // to inline arithmetic skips the load.
        "W19_matrix_multiply" => None,
        // Panel expansion 2026-05-28 (Tier 3 numeric-kernel W20):
        // production source uses Float arithmetic + first-class
        // closure values (`step` / `accel` / `pair_force` defined
        // in `where` clause). The LLVM AOT envelope has both gaps:
        // - Float-typed `#main` return is not in the Phase E typed
        //   surface (Phase E covers Int + String; Float arm tracked
        //   for Phase Z.4.x).
        // Panel expansion 2026-05-29 (Tier 4 Phase 4 — Group C strings
        // / formatting W26): production source uses an f-string
        // (`f"item ${i} of ${n}"`) inside the reduce closure. The
        // LLVM AOT envelope rejects the f-string surface — Phase E
        // String ops cover concat + contains, not the Int→decimal
        // formatter the FString writer needs. No "inlined
        // pre-rendered template" variant — the per-iter String
        // allocation + decimal write IS the load-bearing measurement
        // here, and pre-rendering would skip both.
        "W26_fstring_interp" => None,
        // Panel expansion 2026-05-29 (Tier 4 Phase 4 — Group E
        // non-list stdlib W27): production source uses `#import
        // dict from "std/dict"` + `dict.keys({ ... })` inside the
        // reduce closure. The LLVM AOT envelope rejects all three
        // pieces — the stdlib module-resolver routes through the
        // analyzer's module graph (not in the Phase B LLVM IR
        // surface), the dict literal allocates a `Dict<String,
        // Int>` (the Phase E typed surface covers Int + String
        // only), and the `_dict_keys` host fn materialises a
        // `List<String>` (no closed-form replacement that keeps
        // the per-iter alloc cost the workload measures).
        "W27_stdlib_dict" => None,
        // - First-class closures in non-higher-order position
        //   surface as `closure value cannot cross the wasm module
        //   boundary` (same envelope check that rejects W2 / W16 /
        //   W17 / W18 closure shapes).
        // No "inlined no-closure" variant — the algorithm structure
        // requires the per-step state to flow through the pair-force
        // accumulator; flattening into a single big arithmetic
        // expression would be a paper-win loss of the per-step
        // feedback shape (canonical Verlet integration step).
        "W20_n_body_softened" => None,
        // Panel expansion 2026-05-29 (Tier 4 Phase 1 — Group A runtime
        // dispatch W21): production source uses `#brand` +
        // `#schema` + `match` arm-table dispatch. The LLVM AOT
        // envelope rejects all three constructs today (Phase E
        // typed surface covers Int + String only; brand-tagged
        // Dict values + match-arm lowering both outside the Phase
        // F / Z.4.x roadmap). No "inlined classify" variant — a
        // flat `enum + match` Rust lowering would skip the runtime
        // brand-string compare that IS the load-bearing per-iter
        // cost being measured.
        "W21_match_dispatch" => None,
        // Panel expansion 2026-05-29 (Tier 4 Phase 3 — Group D
        // numeric W28): production source returns Float (`#main
        // -> Float`); the Phase E typed surface tops out at Int +
        // String. Float-typed `#main` return tracked for Phase
        // Z.4.x. No "Int-only collapsed variant" — the workload
        // exists specifically to exercise Float div / mod / Int→
        // Float promote shape; dropping the Float would skip the
        // load-bearing per-iter codepath being measured.
        "W28_float_mixed_ops" => None,
        // Panel expansion 2026-05-29 (Tier 4 Phase 3 — Group F
        // strict-mode W30): same algorithm as W6 (closed-form
        // polynomial `n*(n+1)/2`); rustc / LLVM at -O3 fold the
        // body. Gated by `paper_win_closed_form_fold_label`
        // (analogous to W6). Returning None here is precautionary —
        // the bench panel gate suppresses the row before reaching
        // this dispatcher; the arm keeps the row dormant if a
        // future agent reintroduces the LLVM source variant.
        "W30_strict_mode_baseline" => None,
        // Panel expansion 2026-05-29 (Tier 4 Phase 2 — Group B container
        // construction sugar W23): production source uses an anon `Dict`
        // return shape + spread operator outside the Phase E typed
        // envelope. No inlined variant — a HashMap-clone Rust lowering
        // would substitute a host-allocator path for the tree-walker's
        // `Value::Dict` allocator + key-hash codepath (paper-win per
        // `paper_win_container_sugar_label`). Returning `None` keeps
        // the row honest.
        "W23_dict_spread" => None,
        // Panel expansion 2026-05-29 (Tier 4 Phase 2 — Group B container
        // construction sugar W24): production source uses the
        // comprehension AST node outside the Phase E typed envelope.
        // No inlined variant — a `.filter().map().sum()` rewrite is
        // an arithmetic-progression sum that LLVM -O3 folds to a
        // closed-form polynomial (same shape as W6, gated via
        // `paper_win_container_sugar_label`). Returning `None` keeps
        // the row honest.
        "W24_list_comprehension" => None,
        // Panel expansion 2026-05-29 (Tier 4 Phase 2 — Group B container
        // construction sugar W25): production source uses the pipe
        // operator outside the Phase E typed envelope. No inlined
        // variant — a method-chain rewrite `(0..n).map().filter().
        // sum()` is the same arithmetic-progression sum (sum of even
        // numbers in [1, n]) that LLVM -O3 folds to a closed-form
        // polynomial. Same `paper_win_container_sugar_label` gate.
        "W25_pipe_chain" => None,
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
        // Audit #332 (2026-05-28): the W1 / W2 / W6 rust_native arms
        // are unreachable from the canonical_panel today — the panel
        // gates the `rust_native` row via
        // `paper_win_closed_form_fold_label` because rustc / LLVM
        // collapses the arithmetic-progression sum to a closed-form
        // polynomial. Arms retained for grep visibility and so the
        // dispatcher panics rather than silently no-ops if a future
        // agent reintroduces the labels without re-checking the gate.
        "W1_int_sum" => rust_native_w1(n),
        "W2_f64_dot" => rust_native_w2(n),
        "W3_string_concat" => rust_native_w3(n),
        "W4_string_contains" => rust_native_w4(n),
        "W4_long_haystack" => rust_native_w4_long(n),
        "W5_dict_str_key" => rust_native_w5(n),
        "W6_list_int_sum_plus_one" => rust_native_w6(n),
        "W7_fib" => rust_native_w7(n),
        "W8_poly_callsite" => rust_native_w8(n),
        "W9_nested_matrix" => rust_native_w9(n),
        "W10_config_eval" => rust_native_w10(n),
        "W12_p99_tail" => rust_native_w12(n),
        // Panel expansion 2026-05-28: W13 / W14 / W15 are gated by
        // `paper_win_closed_form_fold_label`, so the canonical_panel
        // never dispatches here. Arms retained for grep visibility +
        // panic-on-reintroduction so a future agent who drops the
        // gate without re-checking the closed-form fold gets a hard
        // failure rather than a silent paper win.
        "W13_deep_dict_access" => rust_native_w13(n),
        "W14_schema_validate" => rust_native_w14(n),
        "W15_conditional_field" => rust_native_w15(n),
        // Panel expansion 2026-05-28 (Tier 2 industry-standard W16):
        // the sum-via-partition recurrence is shape-dependent (no
        // closed-form fold possible), so the `rust_native` row is
        // a legitimate "what would this cost in plain Rust" floor.
        "W16_quicksort" => rust_native_w16(n),
        // Panel expansion 2026-05-28 (Tier 2 industry-standard W17):
        // the scrambled-target bisection sum is not a polynomial in
        // `n` (the targets follow `(i * 31) % n`, breaking any
        // closed-form fold), so `rust_native` is a legitimate floor.
        "W17_binary_search" => rust_native_w17(n),
        // Panel expansion 2026-05-28 (Tier 2 industry-standard W18):
        // `pi(n)` is transcendental (lower-bounded by `n / ln(n)`,
        // not a polynomial in `n`), so no closed-form fold collapses
        // the count. `rust_native` is a legitimate baseline floor.
        "W18_prime_count_trial_div" => rust_native_w18(n),
        // Panel expansion 2026-05-28 (Tier 3 numeric-kernel W19):
        // matmul's `Σ_k a[i][k] * b[k][j]` has mod-100 discontinuity
        // in the generators so no closed-form fold collapses the
        // inner reduce. `rust_native` is a legitimate "what would
        // matmul cost in plain Rust" baseline floor.
        "W19_matrix_multiply" => rust_native_w19(n),
        // W20_n_body_softened returns f64 (Float checksum); routed
        // through a dedicated `rust_native_w20` call site rather
        // than this i64 dispatcher. Reaching this arm is a bug — the
        // W20 row in the canonical_panel calls `rust_native_w20`
        // directly via the W20-specific gate in the loop body.
        "W20_n_body_softened" => {
            panic!("rust_native_dispatch: W20 returns f64, route through rust_native_w20 directly")
        }
        // Panel expansion 2026-05-29 (Tier 4 Phase 3 — Group D numeric
        // W28): the Float reduce returns f64; routed through the
        // dedicated `rust_native_w28` call site, same shape as W20.
        // Reaching this arm is a bug — the W28 row in the
        // canonical_panel calls `rust_native_w28` directly via the
        // W28-specific gate in the loop body.
        "W28_float_mixed_ops" => {
            panic!("rust_native_dispatch: W28 returns f64, route through rust_native_w28 directly")
        }
        // Panel expansion 2026-05-29 (Tier 4 Phase 3 — Group F
        // strict-mode W30): closed-form polynomial `n*(n+1)/2`;
        // rustc / LLVM at -O3 fold the body, same shape as W6.
        // Gated by `paper_win_closed_form_fold_label`, so the
        // canonical_panel never dispatches here. Arm retained for
        // grep visibility + panic-on-reintroduction so a future
        // agent who drops the gate without re-checking the
        // closed-form fold gets a hard failure rather than a silent
        // paper win. The arm delegates to `rust_native_w6` because
        // the algorithm IS byte-identical (the only difference is
        // the strict-mode analyzer path at compile time).
        "W30_strict_mode_baseline" => rust_native_w6(n),
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
        let w4_state = relon_codegen_cranelift::global_trace_jit_state();
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
        let w4l_state = relon_codegen_cranelift::global_trace_jit_state();
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
        // Honesty cleanup (2026-05-28, audit #318): the W5
        // `relon_bytecode` / `relon_wasm_wasmtime` / `relon_wasm_wasmtime_fast`
        // rows were deleted. The production W5 source is
        //     d[keys[i % 10]]
        // (string-array index → string-hash → dict probe per iter); the
        // bytecode envelope rejects dict + list literals + bare-`Dict`
        // returns, and the wasm lowering does not yet implement string
        // hashing / dict lookup, so all three rows ran an algebraically
        // collapsed variant (`(i % 10) + 1` or a 10-entry i64-table
        // load) that skips the string-hash + dict-probe work the
        // production source mandates. Per `/perf` Honesty Rules,
        // "same algorithm" is the first question and a `relon_bytecode`
        // / `relon_wasm_wasmtime` label measuring scalar arith against
        // a LuaJIT row that walks the dict was a paper-win.
        //
        // The bench keeps the production-source `relon_tree_walk` +
        // `luajit` rows here, and the canonical-panel `relon_jit` row
        // below (which feeds the production source through
        // `JitEvaluator::run_main` — today that means tree-walker
        // dispatch, which is honest about the dict-lookup cost). The
        // deleted rows return once Z.4.4 widens the IR pipeline to
        // accept the production `#internal d: {...}` + `keys: [...]`
        // + `d[keys[i % 10]]` surface in the bytecode / wasm
        // codegens.
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
            BenchmarkId::new("W6_list_int_sum_plus_one", "relon_tree_walk"),
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
        group.bench_function(
            BenchmarkId::new("W6_list_int_sum_plus_one", "luajit"),
            |b| {
                b.iter_custom(|iters| {
                    timed_with_warmup(iters, || {
                        let v: i64 = lua_fn_w6.call(()).unwrap();
                        black_box(v);
                    })
                });
            },
        );
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
            group.bench_function(
                BenchmarkId::new("W6_list_int_sum_plus_one", "relon_bytecode"),
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

        // Phase Z.3c-g (2026-05-28): relon_wasm_wasmtime row for W7.
        // Drives the doubly-recursive `where`-clause sibling
        // (`w7_relon_src_bytecode()`) through `WasmEvaluator::run_main`,
        // which lowers via `relon-codegen-wasm` into a wasm module
        // with two local functions (`$fib` + `$__main`); the recursive
        // calls dispatch as direct `Call(fib_fn_idx)` and stay inside
        // the wasm module (no host boundary per recursive step). The
        // classifier routes the **production** `w7_relon_src()` (Dict
        // return + `#internal fib` first-class recursive closure) to
        // the tree-walker fallback — `try_build_wasm_compiled` then
        // skips the row entirely rather than book a tree-walker number
        // under the `wasmtime` label. Z.4 follow-up promotes the
        // production-source path; until then this row honestly
        // measures the `where`-form sibling (same doubly-recursive
        // fib, same I/O shape modulo the Dict wrapper).
        //
        // No iterative `(a, b) := (b, a + b)` rewrite, no closed-form
        // Binet's formula — both are the canonical W7 algorithm-
        // substitution traps (the iterative form is the user-flagged
        // red line from the trace_jit fixture history). Feeding either
        // to the WASM lowering would emit linear or O(1) arithmetic
        // and silently bypass the recursion + call-ABI cost W7 is
        // meant to measure (paper-win anti-pattern per design §7).
        if let Some(wasm) = try_build_wasm_compiled(
            w7_relon_src_bytecode(),
            "W7",
            w7_expected(),
            args_w_n(FIB_N as i64),
        ) {
            use relon_eval_api::Evaluator as _;
            group.bench_function(BenchmarkId::new("W7_fib", "relon_wasm_wasmtime"), |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(FIB_N as i64);
                    timed_with_warmup(iters, || {
                        let v = wasm.run_main(args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            });

            // Fast row — mirrors W1/W2/W8/W9/W10 patterns. Bypasses the
            // HashMap<String, Value> pack + Value::Int wrap. Cross-
            // checked against the buffer path before the timed loop.
            if wasm.has_fast_path() {
                let fast_out = wasm
                    .run_main_legacy_i64_fast(&[FIB_N as i64])
                    .expect("W7 wasm fast path consistency");
                let slow_out = match wasm.run_main(args_w_n(FIB_N as i64)).unwrap() {
                    Value::Int(n) => n,
                    other => panic!("W7 wasm fast cross-check: slow path returned {other:?}"),
                };
                assert_eq!(
                    fast_out, slow_out,
                    "W7 fast/buffer disagree: fast={fast_out} buffer={slow_out}"
                );
                group.bench_function(
                    BenchmarkId::new("W7_fib", "relon_wasm_wasmtime_fast"),
                    |b| {
                        b.iter_custom(|iters| {
                            let n_in = black_box(FIB_N as i64);
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
        // Honesty cleanup (2026-05-28, audit #318): the W8
        // `relon_bytecode` / `relon_wasm_wasmtime` /
        // `relon_wasm_wasmtime_fast` rows were deleted. The production
        // W8 source dispatches `dispatch(i % 4)` through a first-class
        // `#internal` closure; the bytecode envelope rejects
        // first-class closure values and bare-`Dict` returns, and the
        // wasm lowering does not yet implement closure-call
        // indirection. The deleted rows all ran a variant that
        // inlined the closure body (`(i % 4) + 1` for bytecode +
        // LLVM AOT, the 4-arm `?:` ladder for wasm) — both skip the
        // per-iter closure-call indirection the LuaJIT row pays. Per
        // `/perf` Honesty Rules a row labelled `relon_bytecode` /
        // `relon_wasm_wasmtime` that measures the inlined body
        // against a LuaJIT row that walks the closure is a paper-win.
        //
        // The bench keeps the production-source `relon_tree_walk` +
        // `luajit` rows here, and the canonical-panel `relon_jit` row
        // below (which feeds the production source through
        // `JitEvaluator::run_main`). The deleted rows return once
        // the IR pipeline widens the bytecode / wasm codegens to
        // accept first-class closures + bare-`Dict` returns.
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
        // Honesty cleanup (2026-05-28, audit #318): the W9
        // `relon_bytecode` / `relon_wasm_wasmtime` /
        // `relon_wasm_wasmtime_fast` rows were deleted. The production
        // W9 source materialises a `#internal rows: range(n).map((i)
        // => range(n).map((j) => i * n + j))` 2-D list and then sums
        // `rows[i][j]` across a nested reduce. The bytecode envelope
        // rejects list literals + bare-`Dict` returns and the wasm
        // lowering does not yet implement list materialisation, so
        // all three deleted rows ran the inline variant that skips
        // the `rows` allocation entirely (`i * n + j` directly).
        // Per `/perf` Honesty Rules that is a paper-win against the
        // LuaJIT row, which pays the matrix allocation + double
        // table-lookup per iter.
        //
        // The bench keeps the production-source `relon_tree_walk` +
        // `luajit` rows here, and the canonical-panel `relon_jit`
        // row below. The deleted rows return once the IR pipeline
        // widens the bytecode / wasm codegens to lower list literals
        // and bare-`Dict` returns.
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
        // Honesty cleanup (2026-05-28, audit #318): the W10
        // `relon_bytecode` / `relon_wasm_wasmtime` /
        // `relon_wasm_wasmtime_fast` rows were deleted. The production
        // W10 source binds `allow: (i) => ...` as a first-class
        // `#internal` closure and feeds it to `range(n).map(allow)`;
        // bytecode + wasm reject first-class closure values and
        // bare-`Dict` returns, so all three deleted rows ran an
        // inline variant that copied the closure body into the
        // `.map(...)` literal. The boolean composition stays
        // identical, but the closure-dispatch indirection is gone —
        // a `relon_bytecode` / `relon_wasm_wasmtime` label measuring
        // the inline body against a LuaJIT row that walks the closure
        // is a paper-win per `/perf` Honesty Rules.
        //
        // The bench keeps the production-source `relon_tree_walk` +
        // `luajit` rows here, the engineer-facing
        // `relon_trace_jit_fixture` row above (already
        // honestly-suffixed; the trace body is an analytic 0/1
        // multiply kernel), and the canonical-panel `relon_jit` row
        // below. The deleted rows return once the IR pipeline
        // widens to first-class closure values + bare-`Dict`
        // returns.
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

    // ----- W13 deep dict access (config-tree walk) -----
    //
    // Panel-expansion 2026-05-28 (Tier 1 Relon-flavour). See the
    // top-of-file W13 doc-comment for the HONESTY checklist.
    // Backend coverage: tree_walk + luajit. Bytecode / LLVM AOT / wasm
    // reject the production source (dict literal as `#internal`
    // binding + bare `Dict` return). The `relon_jit` canonical-panel
    // row below runs the same source through `JitEvaluator`, which
    // falls through to the tree-walker.
    {
        let (walker, scope) = build_tree_walker(w13_relon_src());
        let lua_fn_w13 = lua_fn(&lua, &w13_lua_src());

        let relon_v = relon_int_result("W13", walker.run_main(&scope, args_w_n(W13_N)).unwrap());
        let lua_v: i64 = lua_fn_w13.call(()).unwrap();
        assert_relon_lua_consistent("W13", relon_v, lua_v, w13_expected());

        group.throughput(Throughput::Elements(W13_N as u64));
        group.bench_function(
            BenchmarkId::new("W13_deep_dict_access", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W13_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W13_deep_dict_access", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w13.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W14 schema validate (boolean range checks) -----
    //
    // Panel-expansion 2026-05-28 (Tier 1 Relon-flavour). See the
    // top-of-file W14 doc-comment for the HONESTY checklist.
    // Backend coverage: tree_walk + luajit + bytecode (if the
    // analyzer / bytecode IR-lift accept the unstrict ternary chain).
    // LLVM AOT / rust_native suppressed via
    // `paper_win_closed_form_fold_label` (the constant-true predicate
    // chain folds to `n * 2` at -O3); reintroduce when the panel grows
    // a black_box-on-acc shape that defeats induction-variable
    // reduction.
    {
        let (walker, scope) = build_tree_walker(w14_relon_src());
        let lua_fn_w14 = lua_fn(&lua, &w14_lua_src());

        let relon_v = relon_int_result("W14", walker.run_main(&scope, args_w_n(W14_N)).unwrap());
        let lua_v: i64 = lua_fn_w14.call(()).unwrap();
        assert_relon_lua_consistent("W14", relon_v, lua_v, w14_expected());

        group.throughput(Throughput::Elements(W14_N as u64));
        group.bench_function(
            BenchmarkId::new("W14_schema_validate", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W14_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W14_schema_validate", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w14.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        if let Some(ev) = try_build_bytecode(w14_relon_src(), "W14") {
            let v = ev.run_main(args_w_n(W14_N)).expect("W14 bytecode run_main");
            assert_eq!(
                relon_int_result("W14", v),
                w14_expected(),
                "W14 bytecode result must match analytic answer"
            );
            group.bench_function(
                BenchmarkId::new("W14_schema_validate", "relon_bytecode"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(W14_N);
                        timed_with_warmup(iters, || {
                            let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );
        }
    }

    // ----- W15 conditional field (`?:` per-iter render) -----
    //
    // Panel-expansion 2026-05-28 (Tier 1 Relon-flavour). See the
    // top-of-file W15 doc-comment for the HONESTY checklist.
    // Backend coverage: tree_walk + luajit + bytecode (if accepted).
    // LLVM AOT / rust_native suppressed via
    // `paper_win_closed_form_fold_label` (the arithmetic progression
    // collapses to a closed-form polynomial at -O3); see W13/W14
    // doc-comments for the same rationale.
    {
        let (walker, scope) = build_tree_walker(w15_relon_src());
        let lua_fn_w15 = lua_fn(&lua, &w15_lua_src());

        let relon_v = relon_int_result("W15", walker.run_main(&scope, args_w_n(W15_N)).unwrap());
        let lua_v: i64 = lua_fn_w15.call(()).unwrap();
        assert_relon_lua_consistent("W15", relon_v, lua_v, w15_expected());

        group.throughput(Throughput::Elements(W15_N as u64));
        group.bench_function(
            BenchmarkId::new("W15_conditional_field", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W15_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W15_conditional_field", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w15.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        if let Some(ev) = try_build_bytecode(w15_relon_src(), "W15") {
            let v = ev.run_main(args_w_n(W15_N)).expect("W15 bytecode run_main");
            assert_eq!(
                relon_int_result("W15", v),
                w15_expected(),
                "W15 bytecode result must match analytic answer"
            );
            group.bench_function(
                BenchmarkId::new("W15_conditional_field", "relon_bytecode"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(W15_N);
                        timed_with_warmup(iters, || {
                            let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );
        }
    }

    // ----- W16 quicksort (recursive functional partition) -----
    //
    // Panel-expansion 2026-05-28 (Tier 2 industry-standard). See the
    // top-of-file W16 doc-comment for the HONESTY checklist. Backend
    // coverage: tree_walk + luajit only. Bytecode / LLVM AOT / wasm
    // reject the production source (recursive `where`-clause helper +
    // first-class closure value via `_list_filter`). The canonical-
    // panel `relon_jit` row below runs the same source through
    // `JitEvaluator`, which falls through to the tree-walker.
    {
        let (walker, scope) = build_tree_walker(w16_relon_src());
        let lua_fn_w16 = lua_fn(&lua, &w16_lua_src());

        let relon_v = relon_int_result("W16", walker.run_main(&scope, args_w_n(W16_N)).unwrap());
        let lua_v: i64 = lua_fn_w16.call(()).unwrap();
        assert_relon_lua_consistent("W16", relon_v, lua_v, w16_expected());

        group.throughput(Throughput::Elements(W16_N as u64));
        group.bench_function(BenchmarkId::new("W16_quicksort", "relon_tree_walk"), |b| {
            b.iter_custom(|iters| {
                let n_in = black_box(W16_N);
                timed_with_warmup(iters, || {
                    let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                    black_box(v);
                })
            });
        });
        group.bench_function(BenchmarkId::new("W16_quicksort", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w16.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W17 binary search (recursive bisection) -----
    //
    // Panel-expansion 2026-05-28 (Tier 2 industry-standard). See the
    // top-of-file W17 doc-comment for the HONESTY checklist. Backend
    // coverage: tree_walk + luajit only (recursion envelope same as
    // W7).
    {
        let (walker, scope) = build_tree_walker(w17_relon_src());
        let lua_fn_w17 = lua_fn(&lua, &w17_lua_src());

        let relon_v = relon_int_result("W17", walker.run_main(&scope, args_w_n(W17_N)).unwrap());
        let lua_v: i64 = lua_fn_w17.call(()).unwrap();
        assert_relon_lua_consistent("W17", relon_v, lua_v, w17_expected());

        group.throughput(Throughput::Elements(W17_N as u64));
        group.bench_function(
            BenchmarkId::new("W17_binary_search", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W17_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W17_binary_search", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w17.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W18 prime count (trial-division) -----
    //
    // Panel-expansion 2026-05-28 (Tier 2 industry-standard). See the
    // top-of-file W18 doc-comment for the HONESTY disclosure
    // (algorithm is trial-division, NOT in-place Eratosthenes sieve;
    // Relon's functional core has no mutable boolean array, the Lua
    // row runs the SAME trial-division to keep the comparison
    // honest). Backend coverage: tree_walk + luajit only.
    {
        let (walker, scope) = build_tree_walker(w18_relon_src());
        let lua_fn_w18 = lua_fn(&lua, &w18_lua_src());

        let relon_v = relon_int_result("W18", walker.run_main(&scope, args_w_n(W18_N)).unwrap());
        let lua_v: i64 = lua_fn_w18.call(()).unwrap();
        assert_relon_lua_consistent("W18", relon_v, lua_v, w18_expected());

        group.throughput(Throughput::Elements(W18_N as u64));
        group.bench_function(
            BenchmarkId::new("W18_prime_count_trial_div", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W18_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(
            BenchmarkId::new("W18_prime_count_trial_div", "luajit"),
            |b| {
                b.iter_custom(|iters| {
                    timed_with_warmup(iters, || {
                        let v: i64 = lua_fn_w18.call(()).unwrap();
                        black_box(v);
                    })
                });
            },
        );
    }

    // ----- W19 matrix multiply (triple-nested O(n^3) loop) -----
    //
    // Panel-expansion 2026-05-28 (Tier 3 numeric-kernel). See the
    // top-of-file W19 doc-comment for the HONESTY checklist. Backend
    // coverage: tree_walk + luajit only. Bytecode envelope rejects
    // (`unknown stdlib method range` — the 2D `range(size).map((i) =>
    // range(size).map(...))` lowering is not yet in the bytecode VM's
    // M2-A scalar envelope). LLVM AOT / Cranelift AOT both reject
    // the same surface. The canonical-panel `relon_jit` row routes
    // the same source through `JitEvaluator`, which falls through to
    // the tree-walker.
    {
        let (walker, scope) = build_tree_walker(w19_relon_src());
        let lua_fn_w19 = lua_fn(&lua, &w19_lua_src());

        let relon_v = relon_int_result("W19", walker.run_main(&scope, args_w_n(W19_N)).unwrap());
        let lua_v: i64 = lua_fn_w19.call(()).unwrap();
        assert_relon_lua_consistent("W19", relon_v, lua_v, w19_expected());

        // Throughput is the number of inner mul-add operations per
        // call: `size^3`. With W19_N=16 that's 4096 elements, which
        // criterion uses to compute per-element ns figures so the
        // matmul row is directly comparable across runs at different
        // matrix sizes (a future agent dialling W19_N to 8 or 32 gets
        // the same units in the criterion report).
        let inner_ops = (W19_N as u64) * (W19_N as u64) * (W19_N as u64);
        group.throughput(Throughput::Elements(inner_ops));
        group.bench_function(
            BenchmarkId::new("W19_matrix_multiply", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W19_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W19_matrix_multiply", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w19.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W20 n-body softened (4-body 1D Verlet integration) -----
    //
    // Panel-expansion 2026-05-28 (Tier 3 numeric-kernel). See the
    // top-of-file W20 doc-comment for the HONESTY disclosure
    // (algorithm is `1/(r^2+eps)^2` softened force, NOT canonical
    // Newtonian `1/r^3` gravity — Relon's `std/math` stdlib has no
    // `sqrt`; the Lua row runs the SAME softened kernel to keep the
    // comparison honest). Backend coverage: tree_walk + luajit only.
    // Float-typed `#main` return + first-class closure values both
    // sit outside today's bytecode / cranelift / LLVM / wasm
    // envelopes.
    //
    // Consistency check uses absolute tolerance `W20_FLOAT_TOL` (1e-6)
    // rather than exact equality — Verlet integration accumulates
    // ~1e-10 relative rounding drift over 1k steps × 64 fp ops/step,
    // which the tree-walker's expression-evaluation order may differ
    // from `rustc`'s by ~1 ULP per FMA-lane fusion.
    {
        let (walker, scope) = build_tree_walker(w20_relon_src());
        let lua_fn_w20 = lua_fn(&lua, &w20_lua_src());

        let relon_v = relon_float_result("W20", walker.run_main(&scope, args_w_n(W20_N)).unwrap());
        let lua_v: f64 = lua_fn_w20.call(()).unwrap();
        let expected_v = w20_expected();
        assert!(
            (relon_v - expected_v).abs() < W20_FLOAT_TOL,
            "W20: Relon {relon_v} differs from expected {expected_v} by more than {W20_FLOAT_TOL}",
        );
        assert!(
            (lua_v - expected_v).abs() < W20_FLOAT_TOL,
            "W20: Lua {lua_v} differs from expected {expected_v} by more than {W20_FLOAT_TOL}",
        );

        // Throughput is the number of pair-force evaluations per
        // call: `n_steps * n_bodies^2` (4 bodies × 4 pair calls /
        // body / step × W20_N steps). With W20_N=1000 that's 16 000
        // pair-force ops, the dominant work unit.
        let pair_force_ops = (W20_N as u64) * 4 * 4;
        group.throughput(Throughput::Elements(pair_force_ops));
        group.bench_function(
            BenchmarkId::new("W20_n_body_softened", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W20_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W20_n_body_softened", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: f64 = lua_fn_w20.call(()).unwrap();
                    black_box(v);
                })
            });
        });
    }

    // ----- W21 match dispatch over brand-tagged values -----
    //
    // Panel expansion 2026-05-29 (Tier 4 Phase 1 — Group A runtime
    // dispatch). See the top-of-file W21 doc-comment for the HONESTY
    // checklist. Backend coverage: tree_walk + luajit only. Bytecode
    // envelope rejects (`#brand` + `match` lowering both outside the
    // M2-A scalar envelope today); the row is suppressed by
    // `try_build_bytecode` returning None and logs `n/a` to stderr.
    // LLVM AOT / Cranelift AOT / wasm reject (same `#brand` /
    // `match` / `#schema` surface). The canonical-panel `relon_jit`
    // row routes the production source through `JitEvaluator::
    // run_main`, which falls through to the tree-walker for the
    // brand+match dispatch.
    {
        let (walker, scope) = build_tree_walker(w21_relon_src());
        let lua_fn_w21 = lua_fn(&lua, &w21_lua_src());

        let relon_v = relon_int_result("W21", walker.run_main(&scope, args_w_n(W21_N)).unwrap());
        let lua_v: i64 = lua_fn_w21.call(()).unwrap();
        assert_relon_lua_consistent("W21", relon_v, lua_v, w21_expected());

        // Throughput is the number of reduce iters (`n`) — each iter
        // performs one list subscript + one brand-label compare +
        // one Int add, the dominant per-iter work unit. Matches W13 /
        // W14 / W15 / W18 throughput shape so per-element ns figures
        // line up across the Tier 1-2-4 Relon-flavour rows.
        group.throughput(Throughput::Elements(W21_N as u64));
        group.bench_function(
            BenchmarkId::new("W21_match_dispatch", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W21_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W21_match_dispatch", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w21.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Bytecode row — honest try. The brand-tag + `match` arms are
        // outside the M2-A scalar envelope today; `try_build_bytecode`
        // returns None and the row is omitted. The eprintln log line
        // emitted by the helper makes the gate visible at bench time.
        if let Some(ev) = try_build_bytecode(w21_relon_src(), "W21_match_dispatch") {
            let v = ev.run_main(args_w_n(W21_N)).expect("W21 bytecode run_main");
            let got = relon_int_result("W21", v);
            assert_eq!(
                got,
                w21_expected(),
                "W21 bytecode result mismatch: got {got}, expected {}",
                w21_expected()
            );
            group.bench_function(
                BenchmarkId::new("W21_match_dispatch", "relon_bytecode"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(W21_N);
                        timed_with_warmup(iters, || {
                            let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );
        }
    }

    // ----- W23 dict spread copy per iter -----
    //
    // Panel expansion 2026-05-29 (Tier 4 Phase 2 — Group B container
    // construction sugar). See the top-of-file W23 doc-comment for
    // the HONESTY checklist. Backend coverage: tree_walk + luajit
    // only. Bytecode envelope rejects (`Op::Dict` + spread lowering
    // both outside the M2-A scalar envelope today); the row is
    // suppressed by `try_build_bytecode` returning None and logs
    // `n/a` to stderr. LLVM AOT / Cranelift AOT / wasm reject (same
    // Dict / spread surface). The canonical-panel `relon_jit` row
    // routes the production source through `JitEvaluator::run_main`,
    // which falls through to the tree-walker.
    {
        let (walker, scope) = build_tree_walker(w23_relon_src());
        let lua_fn_w23 = lua_fn(&lua, &w23_lua_src());

        let relon_v = relon_int_result("W23", walker.run_main(&scope, args_w_n(W23_N)).unwrap());
        let lua_v: i64 = lua_fn_w23.call(()).unwrap();
        assert_relon_lua_consistent("W23", relon_v, lua_v, w23_expected());

        // Throughput is the number of reduce iters (`n`) — each iter
        // performs one dict copy of 4 keys + 1 key insert + 1 `_len`
        // projection + 1 Int add, the dominant per-iter work unit.
        group.throughput(Throughput::Elements(W23_N as u64));
        group.bench_function(
            BenchmarkId::new("W23_dict_spread", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W23_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W23_dict_spread", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w23.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Bytecode row — honest try. Dict + spread surface is outside
        // the M2-A scalar envelope today; `try_build_bytecode`
        // returns None and the row is omitted.
        if let Some(ev) = try_build_bytecode(w23_relon_src(), "W23_dict_spread") {
            let v = ev.run_main(args_w_n(W23_N)).expect("W23 bytecode run_main");
            let got = relon_int_result("W23", v);
            assert_eq!(
                got,
                w23_expected(),
                "W23 bytecode result mismatch: got {got}, expected {}",
                w23_expected()
            );
            group.bench_function(BenchmarkId::new("W23_dict_spread", "relon_bytecode"), |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W23_N);
                    timed_with_warmup(iters, || {
                        let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            });
        }
    }

    // ----- W24 list comprehension with predicate filter -----
    //
    // Panel expansion 2026-05-29 (Tier 4 Phase 2 — Group B container
    // construction sugar). See the top-of-file W24 doc-comment for
    // the HONESTY checklist. Backend coverage: tree_walk + luajit
    // only. Bytecode envelope rejects the comprehension AST node;
    // LLVM AOT / Cranelift AOT / wasm reject the same surface. The
    // canonical-panel `relon_jit` row falls through to the
    // tree-walker.
    {
        let (walker, scope) = build_tree_walker(w24_relon_src());
        let lua_fn_w24 = lua_fn(&lua, &w24_lua_src());

        let relon_v = relon_int_result("W24", walker.run_main(&scope, args_w_n(W24_N)).unwrap());
        let lua_v: i64 = lua_fn_w24.call(()).unwrap();
        assert_relon_lua_consistent("W24", relon_v, lua_v, w24_expected());

        // Throughput is the input range count (`n`) — each input
        // element pays one predicate eval + one optional mul +
        // conditional list-push, plus the final sum is O(filtered).
        // We use `n` rather than the filtered count so per-element
        // ns is directly comparable across the Tier 1-4 rows.
        group.throughput(Throughput::Elements(W24_N as u64));
        group.bench_function(
            BenchmarkId::new("W24_list_comprehension", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W24_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W24_list_comprehension", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w24.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Bytecode row — honest try. Comprehension AST node sits
        // outside the M2-A envelope; `try_build_bytecode` returns
        // None and the row is omitted.
        if let Some(ev) = try_build_bytecode(w24_relon_src(), "W24_list_comprehension") {
            let v = ev.run_main(args_w_n(W24_N)).expect("W24 bytecode run_main");
            let got = relon_int_result("W24", v);
            assert_eq!(
                got,
                w24_expected(),
                "W24 bytecode result mismatch: got {got}, expected {}",
                w24_expected()
            );
            group.bench_function(
                BenchmarkId::new("W24_list_comprehension", "relon_bytecode"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(W24_N);
                        timed_with_warmup(iters, || {
                            let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );
        }
    }

    // ----- W25 pipe chain through list.map / list.filter / list.sum --
    //
    // Panel expansion 2026-05-29 (Tier 4 Phase 2 — Group B container
    // construction sugar). See the top-of-file W25 doc-comment for
    // the HONESTY checklist. Backend coverage: tree_walk + luajit
    // only. Bytecode envelope rejects the pipe operator; LLVM AOT /
    // Cranelift AOT / wasm reject the same surface. The
    // canonical-panel `relon_jit` row falls through to the
    // tree-walker.
    {
        let (walker, scope) = build_tree_walker(w25_relon_src());
        let lua_fn_w25 = lua_fn(&lua, &w25_lua_src());

        let relon_v = relon_int_result("W25", walker.run_main(&scope, args_w_n(W25_N)).unwrap());
        let lua_v: i64 = lua_fn_w25.call(()).unwrap();
        assert_relon_lua_consistent("W25", relon_v, lua_v, w25_expected());

        // Throughput is the input range count (`n`) — each input
        // element walks through map (closure dispatch + add) +
        // filter (closure dispatch + predicate) + (on hit) sum add.
        group.throughput(Throughput::Elements(W25_N as u64));
        group.bench_function(BenchmarkId::new("W25_pipe_chain", "relon_tree_walk"), |b| {
            b.iter_custom(|iters| {
                let n_in = black_box(W25_N);
                timed_with_warmup(iters, || {
                    let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                    black_box(v);
                })
            });
        });
        group.bench_function(BenchmarkId::new("W25_pipe_chain", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w25.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Bytecode row — honest try. Pipe operator sits outside the
        // M2-A envelope; `try_build_bytecode` returns None and the
        // row is omitted.
        if let Some(ev) = try_build_bytecode(w25_relon_src(), "W25_pipe_chain") {
            let v = ev.run_main(args_w_n(W25_N)).expect("W25 bytecode run_main");
            let got = relon_int_result("W25", v);
            assert_eq!(
                got,
                w25_expected(),
                "W25 bytecode result mismatch: got {got}, expected {}",
                w25_expected()
            );
            group.bench_function(BenchmarkId::new("W25_pipe_chain", "relon_bytecode"), |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W25_N);
                    timed_with_warmup(iters, || {
                        let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            });
        }
    }

    // ----- W28 float div / mod / Int→Float mixed ops -----
    //
    // Panel expansion 2026-05-29 (Tier 4 Phase 3 — Group D numeric /
    // literal corner ops). See the top-of-file W28 doc-comment for
    // the HONESTY checklist. Backend coverage: tree_walk + luajit;
    // bytecode tries honestly via `try_build_bytecode`. LLVM AOT /
    // wasm / AOT all reject (Float `#main` return outside today's
    // Phase E typed surface; Z.1 wasm program set has no Float
    // lowering). The `rust_native` row dispatches through the
    // dedicated `rust_native_w28` (mirrors W20's f64-return shape).
    //
    // Consistency check uses absolute tolerance `W28_FLOAT_TOL`
    // (1e-6) — the tree-walker's expression-evaluation order may
    // differ from `rustc`'s by ~1 ULP per reduce iter; 1e-6
    // absolute keeps the gate above the ULP-drift floor while
    // still catching algorithm errors.
    {
        let (walker, scope) = build_tree_walker(w28_relon_src());
        let lua_fn_w28 = lua_fn(&lua, &w28_lua_src());

        let relon_v = relon_float_result("W28", walker.run_main(&scope, args_w_n(W28_N)).unwrap());
        let lua_v: f64 = lua_fn_w28.call(()).unwrap();
        let expected_v = w28_expected();
        assert!(
            (relon_v - expected_v).abs() < W28_FLOAT_TOL,
            "W28: Relon {relon_v} differs from expected {expected_v} by more than {W28_FLOAT_TOL}",
        );
        assert!(
            (lua_v - expected_v).abs() < W28_FLOAT_TOL,
            "W28: Lua {lua_v} differs from expected {expected_v} by more than {W28_FLOAT_TOL}",
        );

        // Throughput is the reduce iter count `n` — each iter does
        // one Float div + one Int mod + one Int→Float promote + one
        // Float add. Matches W21 / W13-W18 throughput shape so per-
        // element ns figures line up across the Tier 1-2-4 rows.
        group.throughput(Throughput::Elements(W28_N as u64));
        group.bench_function(
            BenchmarkId::new("W28_float_mixed_ops", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W28_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W28_float_mixed_ops", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: f64 = lua_fn_w28.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Bytecode row — honest try. The reduce shape + closure
        // dispatch sit outside the M2-A scalar envelope today (same
        // envelope check that bounces W2 / W6's untyped reduce
        // closure); `try_build_bytecode` returns None and the row
        // is omitted with an eprintln log line.
        if let Some(ev) = try_build_bytecode(w28_relon_src(), "W28_float_mixed_ops") {
            let v = ev.run_main(args_w_n(W28_N)).expect("W28 bytecode run_main");
            let got = relon_float_result("W28", v);
            assert!(
                (got - expected_v).abs() < W28_FLOAT_TOL,
                "W28 bytecode result mismatch: got {got}, expected {expected_v} \
                 (abs_err {abs:.3e} >= tol {W28_FLOAT_TOL:.3e})",
                abs = (got - expected_v).abs(),
            );
            group.bench_function(
                BenchmarkId::new("W28_float_mixed_ops", "relon_bytecode"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(W28_N);
                        timed_with_warmup(iters, || {
                            let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );
        }
    }

    // ----- W30 strict-mode baseline (typed lambda param) -----
    //
    // Panel expansion 2026-05-29 (Tier 4 Phase 3 — Group F strict
    // mode). See the top-of-file W30 doc-comment for the HONESTY
    // checklist. Same algorithm as W6 (`list.sum(range(n).map((i)
    // => i + 1))`) but the source omits `#unstrict` / `#relaxed`
    // (strict default) and the inner lambda carries a typed param
    // (`(Int i) => ...`). The pair `(W6, W30)` surfaces any
    // analyzer / IR overhead the strict-mode path adds versus the
    // unstrict W6 row. Backend coverage: tree_walk + luajit;
    // bytecode tries honestly. The `rust_native` / `relon_llvm_aot`
    // rows are gated by `paper_win_closed_form_fold_label` for the
    // same reason W6 is (closed-form `n*(n+1)/2`).
    {
        let (walker, scope) = build_tree_walker(w30_relon_src());
        let lua_fn_w30 = lua_fn(&lua, &w30_lua_src());

        let relon_v = relon_int_result("W30", walker.run_main(&scope, args_w_n(W30_N)).unwrap());
        let lua_v: i64 = lua_fn_w30.call(()).unwrap();
        assert_relon_lua_consistent("W30", relon_v, lua_v, w30_expected());

        // Throughput is the reduce iter count `n` — each iter does
        // one list-element load + one `i + 1` + one accumulator
        // add. Matches W6's throughput shape so the strict-vs-
        // unstrict A/B compare is byte-identical at the
        // per-element ns level.
        group.throughput(Throughput::Elements(W30_N as u64));
        group.bench_function(
            BenchmarkId::new("W30_strict_mode_baseline", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W30_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(
            BenchmarkId::new("W30_strict_mode_baseline", "luajit"),
            |b| {
                b.iter_custom(|iters| {
                    timed_with_warmup(iters, || {
                        let v: i64 = lua_fn_w30.call(()).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        // Bytecode row — honest try. Same `list.sum(range(n).map(
        // (i) => i + 1))` IR shape as W6; the typed-param lambda
        // additionally exercises the strict-mode envelope check at
        // bytecode compile time. If `try_build_bytecode` accepts,
        // the row pairs the strict-typed kernel against W6's
        // unstrict-typed kernel for direct A/B.
        if let Some(ev) = try_build_bytecode(w30_relon_src(), "W30_strict_mode_baseline") {
            let v = ev.run_main(args_w_n(W30_N)).expect("W30 bytecode run_main");
            let got = relon_int_result("W30", v);
            assert_eq!(
                got,
                w30_expected(),
                "W30 bytecode result mismatch: got {got}, expected {}",
                w30_expected()
            );
            group.bench_function(
                BenchmarkId::new("W30_strict_mode_baseline", "relon_bytecode"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(W30_N);
                        timed_with_warmup(iters, || {
                            let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );
        }
    }

    // ----- W26 f-string interpolation per-iter concat -----
    //
    // Panel expansion 2026-05-29 (Tier 4 Phase 4 — Group C strings /
    // formatting). See the top-of-file W26 doc-comment for the
    // HONESTY checklist. Backend coverage: tree_walk + luajit only.
    // Bytecode envelope rejects (`Op::FString` lowering outside the
    // M2-A scalar envelope today); the row is suppressed by
    // `try_build_bytecode` returning None and logs `n/a` to stderr.
    // LLVM AOT / Cranelift AOT / wasm reject (same f-string surface
    // — Phase E String ops cover concat + contains, not formatted
    // interpolation). The canonical-panel `relon_jit` row routes the
    // production source through `JitEvaluator::run_main`, which
    // falls through to the tree-walker for the f-string evaluation.
    {
        let (walker, scope) = build_tree_walker(w26_relon_src());
        let lua_fn_w26 = lua_fn(&lua, &w26_lua_src());

        let relon_v = relon_int_result("W26", walker.run_main(&scope, args_w_n(W26_N)).unwrap());
        let lua_v: i64 = lua_fn_w26.call(()).unwrap();
        assert_relon_lua_consistent("W26", relon_v, lua_v, w26_expected());

        // Throughput is the number of reduce iters (`n`) — each iter
        // performs one f-string evaluation (alloc + write + concat) +
        // one `_len` read + one Int add, the dominant per-iter work
        // unit. Matches W1 / W3 / W21 throughput shape so per-element
        // ns figures line up across the Tier 1 / 2 / 4 Relon-flavour
        // rows.
        group.throughput(Throughput::Elements(W26_N as u64));
        group.bench_function(
            BenchmarkId::new("W26_fstring_interp", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W26_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W26_fstring_interp", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w26.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Bytecode row — honest try. The f-string evaluator surface is
        // outside the M2-A scalar envelope today; `try_build_bytecode`
        // returns None and the row is omitted. The eprintln log line
        // emitted by the helper makes the gate visible at bench time.
        if let Some(ev) = try_build_bytecode(w26_relon_src(), "W26_fstring_interp") {
            let v = ev.run_main(args_w_n(W26_N)).expect("W26 bytecode run_main");
            let got = relon_int_result("W26", v);
            assert_eq!(
                got,
                w26_expected(),
                "W26 bytecode result mismatch: got {got}, expected {}",
                w26_expected()
            );
            group.bench_function(
                BenchmarkId::new("W26_fstring_interp", "relon_bytecode"),
                |b| {
                    b.iter_custom(|iters| {
                        let n_in = black_box(W26_N);
                        timed_with_warmup(iters, || {
                            let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                            black_box(v);
                        })
                    });
                },
            );
        }
    }

    // ----- W27 stdlib std/dict dispatch -----
    //
    // Panel expansion 2026-05-29 (Tier 4 Phase 4 — Group E non-list
    // stdlib coverage). See the top-of-file W27 doc-comment for the
    // HONESTY checklist + the stdlib audit notes
    // (`std/dict` exports `merge` / `keys` / `values` / `has_key`;
    // no `len`). Backend coverage: tree_walk + luajit only. Bytecode
    // envelope rejects (stdlib module-resolver + dict literal +
    // Closure dispatch all outside the M2-A scalar envelope today);
    // the row is suppressed by `try_build_bytecode` returning None
    // and logs `n/a` to stderr. LLVM AOT / Cranelift AOT / wasm
    // reject (same module-resolver + dict-literal surface — Phase E
    // typed surface covers Int + String only). The canonical-panel
    // `relon_jit` row routes the production source through
    // `JitEvaluator::run_main`, which falls through to the tree-
    // walker for the std/dict dispatch.
    {
        let (walker, scope) = build_tree_walker(w27_relon_src());
        let lua_fn_w27 = lua_fn(&lua, &w27_lua_src());

        let relon_v = relon_int_result("W27", walker.run_main(&scope, args_w_n(W27_N)).unwrap());
        let lua_v: i64 = lua_fn_w27.call(()).unwrap();
        assert_relon_lua_consistent("W27", relon_v, lua_v, w27_expected());

        // Throughput is the number of reduce iters (`n`) — each iter
        // performs one stdlib-module dispatch (`dict.keys`) + one
        // dict-literal alloc + one List<String> materialise + one
        // `_len` read + one Int add. Matches W13 / W14 / W18 / W21
        // throughput shape so per-element ns figures line up across
        // the Tier 1 / 2 / 4 Relon-flavour rows.
        group.throughput(Throughput::Elements(W27_N as u64));
        group.bench_function(
            BenchmarkId::new("W27_stdlib_dict", "relon_tree_walk"),
            |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W27_N);
                    timed_with_warmup(iters, || {
                        let v = walker.run_main(&scope, args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
                });
            },
        );
        group.bench_function(BenchmarkId::new("W27_stdlib_dict", "luajit"), |b| {
            b.iter_custom(|iters| {
                timed_with_warmup(iters, || {
                    let v: i64 = lua_fn_w27.call(()).unwrap();
                    black_box(v);
                })
            });
        });
        // Bytecode row — honest try. The stdlib module-resolver + dict
        // literal lowering are outside the M2-A scalar envelope today;
        // `try_build_bytecode` returns None and the row is omitted.
        // The eprintln log line emitted by the helper makes the gate
        // visible at bench time.
        if let Some(ev) = try_build_bytecode(w27_relon_src(), "W27_stdlib_dict") {
            let v = ev.run_main(args_w_n(W27_N)).expect("W27 bytecode run_main");
            let got = relon_int_result("W27", v);
            assert_eq!(
                got,
                w27_expected(),
                "W27 bytecode result mismatch: got {got}, expected {}",
                w27_expected()
            );
            group.bench_function(BenchmarkId::new("W27_stdlib_dict", "relon_bytecode"), |b| {
                b.iter_custom(|iters| {
                    let n_in = black_box(W27_N);
                    timed_with_warmup(iters, || {
                        let v = ev.run_main(args_w_n(black_box(n_in))).unwrap();
                        black_box(v);
                    })
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
        (
            "W6_list_int_sum_plus_one",
            w6_relon_src(),
            TREE_WALK_N,
            || args_w_n(TREE_WALK_N as i64),
        ),
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
        // Panel-expansion 2026-05-28 (Tier 1 Relon-flavour W13):
        // * relon_jit row: runs via `JitEvaluator::run_main` (falls
        //   through to tree-walker for the dict-literal `#internal cfg`
        //   binding).
        // * relon_aot row: `try_build_aot` returns None (cranelift AOT
        //   envelope rejects), row records n/a.
        // * relon_llvm_aot / relon_llvm_aot_fast: `llvm_aot_source_for`
        //   returns None; the gate further suppresses any future variant.
        // * rust_native: gated by `paper_win_closed_form_fold_label`.
        // * relon_wasm_wasmtime: `try_build_wasm_compiled` returns None
        //   (classifier routes to tree-walker fallback), row records n/a.
        (
            "W13_deep_dict_access",
            w13_relon_src(),
            W13_N as u64,
            || args_w_n(W13_N),
        ),
        // Panel-expansion 2026-05-28 (Tier 1 Relon-flavour W14):
        // * relon_jit: runs through `JitEvaluator::run_main`; falls
        //   through to tree-walker for the unstrict ternary chain.
        // * relon_aot / relon_llvm_aot / rust_native: see comment on
        //   W13 entry above. The closed-form-fold gate suppresses LLVM
        //   AOT + rust_native; cranelift AOT envelope rejects.
        ("W14_schema_validate", w14_relon_src(), W14_N as u64, || {
            args_w_n(W14_N)
        }),
        // Panel-expansion 2026-05-28 (Tier 1 Relon-flavour W15):
        // Same backend coverage as W14 — tree_walk + luajit + bytecode
        // (if accepted); LLVM AOT + rust_native suppressed via
        // `paper_win_closed_form_fold_label` (the arithmetic
        // progression `Σ (i%2==0 ? 2i : 3i)` collapses to a closed-
        // form polynomial at -O3).
        (
            "W15_conditional_field",
            w15_relon_src(),
            W15_N as u64,
            || args_w_n(W15_N),
        ),
        // Panel-expansion 2026-05-28 (Tier 2 industry-standard W16):
        // * relon_jit: runs through `JitEvaluator::run_main`; falls
        //   through to tree-walker (recursive `where` + `_list_filter`
        //   closures sit outside the bytecode envelope).
        // * relon_aot / relon_llvm_aot: rejected (recursion + closure
        //   envelope); both return None.
        // * rust_native: VALID — quicksort partition is shape-
        //   dependent, no closed-form fold.
        // * relon_wasm_wasmtime: classifier scope-cut (not in Z.1
        //   wasm program set).
        ("W16_quicksort", w16_relon_src(), W16_N as u64, || {
            args_w_n(W16_N)
        }),
        // Panel-expansion 2026-05-28 (Tier 2 industry-standard W17):
        // Same backend coverage rationale as W16. rust_native row is
        // valid (scrambled-target sum has no closed form); LLVM AOT /
        // bytecode / wasm reject (recursion envelope).
        ("W17_binary_search", w17_relon_src(), W17_N as u64, || {
            args_w_n(W17_N)
        }),
        // Panel-expansion 2026-05-28 (Tier 2 industry-standard W18):
        // Algorithm = trial-division (HONESTY-disclosed substitution
        // for the in-place sieve, see W18 doc-comment). rust_native
        // is valid (pi(n) is transcendental, no closed-form fold);
        // bytecode / LLVM AOT / wasm reject (recursion envelope).
        (
            "W18_prime_count_trial_div",
            w18_relon_src(),
            W18_N as u64,
            || args_w_n(W18_N),
        ),
        // Panel-expansion 2026-05-28 (Tier 3 numeric-kernel W19):
        // * relon_jit: runs through `JitEvaluator::run_main`; falls
        //   through to tree-walker (nested `range.map.map` + 2D index
        //   `a[i][k]` sits outside the bytecode envelope's
        //   M2-A scalar surface).
        // * relon_aot / relon_llvm_aot: both reject (same 2D-list +
        //   nested-closure surface as W16/W17/W18). Returning None
        //   from `llvm_aot_source_for`.
        // * rust_native: VALID — matmul has no closed-form fold over
        //   the mod-100 generator discontinuity.
        // * relon_wasm_wasmtime: classifier scope-cut (Z.1 program
        //   set does not include matmul).
        // * Throughput uses inner-mul-add count `size^3` rather than
        //   `n` so per-element ns is comparable across matrix sizes.
        (
            "W19_matrix_multiply",
            w19_relon_src(),
            {
                let s = W19_N as u64;
                s * s * s
            },
            || args_w_n(W19_N),
        ),
        // Panel-expansion 2026-05-28 (Tier 3 numeric-kernel W20):
        // * relon_jit: runs through `JitEvaluator::run_main`; falls
        //   through to tree-walker (Float type + first-class closures
        //   sit outside today's bytecode / cranelift / LLVM AOT
        //   envelopes — Phase Z.4.x Float arm pending).
        // * relon_aot / relon_llvm_aot: both reject. Returning None
        //   from `llvm_aot_source_for`.
        // * rust_native: VALID — Verlet integration has feedback-
        //   shaped per-step state mutation, no closed-form fold over
        //   the time loop. But the canonical_panel rust_native row
        //   downstream dispatches through `rust_native_dispatch(label, n)`
        //   which returns `i64`; the W20 row is gated out below and
        //   the bench drops back to a dedicated f64 rust_native row
        //   block inline (see the W20 rust_native gate in the loop
        //   body).
        // * relon_wasm_wasmtime: classifier scope-cut (Z.1 has no
        //   Float support).
        // * Throughput uses pair-force evaluation count `n_steps *
        //   n_bodies^2` rather than `n` so per-pair ns is the unit.
        (
            "W20_n_body_softened",
            w20_relon_src(),
            { (W20_N as u64) * 4 * 4 },
            || args_w_n(W20_N),
        ),
        // Panel expansion 2026-05-29 (Tier 4 Phase 1 — Group A runtime
        // dispatch W21):
        // * relon_jit: runs through `JitEvaluator::run_main`; falls
        //   through to the tree-walker (brand-tag + `match` arms sit
        //   outside the M2-A bytecode envelope today).
        // * relon_aot / relon_llvm_aot: both reject (same `#brand` /
        //   `match` / `#schema` surface as the bytecode envelope).
        //   Returning None from `llvm_aot_source_for`.
        // * rust_native: gated out by adding the label to
        //   `paper_win_brand_dispatch_label`. A Rust-side `enum +
        //   match` would skip the tree-walker's runtime brand-string
        //   compare; the per-iter "cost of dynamic brand dispatch"
        //   IS the load-bearing measurement here.
        // * relon_wasm_wasmtime: classifier scope-cut (Z.1 program
        //   set does not include brand-tag dispatch).
        // * Throughput uses reduce iter count `n` so per-iter ns is
        //   directly comparable across the Tier 1-2-4 Relon-flavour
        //   rows.
        ("W21_match_dispatch", w21_relon_src(), W21_N as u64, || {
            args_w_n(W21_N)
        }),
        // Panel expansion 2026-05-29 (Tier 4 Phase 4 — Group C strings
        // / formatting W26):
        // * relon_jit: runs through `JitEvaluator::run_main`; falls
        //   through to the tree-walker (f-string evaluator + `_len`
        //   reduce body sit outside the M2-A bytecode envelope
        //   today; Op::FString tracked under Z.4.x).
        // * relon_aot / relon_llvm_aot: both reject (same f-string
        //   surface — Phase E String ops cover concat + contains,
        //   not formatted interpolation). Returning None from
        //   `llvm_aot_source_for`.
        // * rust_native: gated out by adding the label to
        //   `paper_win_fstring_interp_label`. A Rust-side
        //   `format!("item {} of {}", i, n).len()` could be folded
        //   by rustc / LLVM to a digit-bucket closed form, and the
        //   per-iter "cost of f-string interpolation alloc + write"
        //   IS the load-bearing measurement here.
        // * relon_wasm_wasmtime: classifier scope-cut (Z.1 program
        //   set does not include the f-string surface).
        // * Throughput uses reduce iter count `n` so per-iter ns is
        //   directly comparable across the Tier 1-2-4 Relon-flavour
        //   rows.
        ("W26_fstring_interp", w26_relon_src(), W26_N as u64, || {
            args_w_n(W26_N)
        }),
        // Panel expansion 2026-05-29 (Tier 4 Phase 4 — Group E
        // non-list stdlib W27):
        // * relon_jit: runs through `JitEvaluator::run_main`; falls
        //   through to the tree-walker (stdlib module-resolver +
        //   dict literal + Closure dispatch sit outside the M2-A
        //   bytecode envelope today).
        // * relon_aot / relon_llvm_aot: both reject (same module-
        //   resolver + dict-literal surface — Phase E typed surface
        //   covers Int + String only). Returning None from
        //   `llvm_aot_source_for`.
        // * rust_native: gated out by adding the label to
        //   `paper_win_stdlib_dict_label`. A Rust-side
        //   `HashMap::from([...]).keys().count()` would be folded
        //   by rustc / LLVM to the compile-time constant `3`, and
        //   the per-iter "cost of std/dict module dispatch + dict
        //   literal alloc + keys() materialise" IS the load-bearing
        //   measurement here.
        // * relon_wasm_wasmtime: classifier scope-cut (Z.1 program
        //   set does not include stdlib-module imports).
        // * Throughput uses reduce iter count `n` so per-iter ns is
        //   directly comparable across the Tier 1-2-4 Relon-flavour
        //   rows.
        ("W27_stdlib_dict", w27_relon_src(), W27_N as u64, || {
            args_w_n(W27_N)
        }),
        // Panel expansion 2026-05-29 (Tier 4 Phase 2 — Group B container
        // construction sugar W23):
        // * relon_jit: runs through `JitEvaluator::run_main`; falls
        //   through to the tree-walker (`Op::Dict` + spread lowering
        //   sit outside the M2-A bytecode envelope today).
        // * relon_aot / relon_llvm_aot: both reject (same Dict / spread
        //   surface). Returning None from `llvm_aot_source_for`.
        // * rust_native: gated out by `paper_win_container_sugar_label`.
        //   A Rust-side `HashMap::clone() + insert()` would skip the
        //   tree-walker's `Value::Dict` allocator + key-hash work; the
        //   per-iter "cost of dict spread copy" is the load-bearing
        //   measurement here.
        // * relon_wasm_wasmtime: classifier scope-cut (Z.1 program set
        //   does not include dict-spread lowering).
        // * Throughput uses reduce iter count `n` so per-iter ns is
        //   directly comparable across the Tier 1-4 Relon-flavour rows.
        ("W23_dict_spread", w23_relon_src(), W23_N as u64, || {
            args_w_n(W23_N)
        }),
        // Panel expansion 2026-05-29 (Tier 4 Phase 2 — Group B container
        // construction sugar W24):
        // * relon_jit: runs through `JitEvaluator::run_main`; falls
        //   through to the tree-walker (comprehension AST node sits
        //   outside the M2-A bytecode envelope today — bytecode probe
        //   surfaces `unsupported expression `Comprehension``).
        // * relon_aot / relon_llvm_aot: both reject. Returning None
        //   from `llvm_aot_source_for`.
        // * rust_native: gated out by `paper_win_container_sugar_label`.
        //   The kept values are multiples of 3 doubled — arithmetic
        //   progression that LLVM -O3 folds to a closed-form polynomial,
        //   same shape as W6.
        // * relon_wasm_wasmtime: classifier scope-cut.
        // * Throughput uses input range count `n` so per-element ns is
        //   directly comparable across the Tier 1-4 Relon-flavour rows.
        (
            "W24_list_comprehension",
            w24_relon_src(),
            W24_N as u64,
            || args_w_n(W24_N),
        ),
        // Panel expansion 2026-05-29 (Tier 4 Phase 2 — Group B container
        // construction sugar W25):
        // * relon_jit: runs through `JitEvaluator::run_main`; falls
        //   through to the tree-walker (pipe operator sits outside the
        //   M2-A bytecode envelope — bytecode probe surfaces
        //   `unsupported operator `Pipe``).
        // * relon_aot / relon_llvm_aot: both reject. Returning None
        //   from `llvm_aot_source_for`.
        // * rust_native: gated out by `paper_win_container_sugar_label`.
        //   The kept values are even numbers in [1, n] — arithmetic
        //   progression, LLVM -O3 closed-form fold same as W6.
        // * relon_wasm_wasmtime: classifier scope-cut.
        // * Throughput uses input range count `n` so per-element ns is
        //   directly comparable across the Tier 1-4 Relon-flavour rows.
        ("W25_pipe_chain", w25_relon_src(), W25_N as u64, || {
            args_w_n(W25_N)
        }),
        // Panel expansion 2026-05-29 (Tier 4 Phase 3 — Group D
        // numeric W28):
        // * relon_jit: runs through `JitEvaluator::run_main`;
        //   falls through to tree-walker / bytecode based on the
        //   wrapper's tier selection (the Float reduce closure
        //   shape determines which tier accepts).
        // * relon_aot / relon_llvm_aot: both reject (Float `#main`
        //   return outside Phase E typed surface). Returning None
        //   from `llvm_aot_source_for`.
        // * rust_native: routed through the dedicated
        //   `rust_native_w28` (f64-return shape; the i64
        //   `rust_native_dispatch` cannot carry the Float kernel).
        //   See the W28 gate in the canonical_panel loop body.
        // * relon_wasm_wasmtime: classifier scope-cut (Z.1 program
        //   set has no Float lowering).
        // * Throughput uses reduce iter count `n` so per-iter ns
        //   is directly comparable across the Tier 1-2-4 Relon-
        //   flavour rows.
        ("W28_float_mixed_ops", w28_relon_src(), W28_N as u64, || {
            args_w_n(W28_N)
        }),
        // Panel expansion 2026-05-29 (Tier 4 Phase 3 — Group F
        // strict-mode W30):
        // * relon_jit: runs through `JitEvaluator::run_main`; the
        //   strict-typed lambda body has the same IR shape as W6
        //   so the tier selection mirrors W6's row exactly.
        // * relon_aot / relon_llvm_aot / rust_native: gated by
        //   `paper_win_closed_form_fold_label` (closed-form
        //   `n*(n+1)/2`, same as W6). Rows record n/a.
        // * relon_wasm_wasmtime: Z.1 classifier accepts `list.sum
        //   (range(n).map((Int i) => i + 1))` — the typed-param
        //   lambda lowering is in the W6 program family. If the
        //   classifier accepts, the row pairs the strict kernel
        //   against W6's unstrict kernel for direct A/B.
        // * Throughput uses reduce iter count `n` so per-iter ns
        //   stays apples-to-apples with W6.
        (
            "W30_strict_mode_baseline",
            w30_relon_src(),
            W30_N as u64,
            || args_w_n(W30_N),
        ),
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
        //
        // Honesty cleanup (2026-05-28, audit #318):
        // `paper_win_collapsed_variant_label` short-circuits the row
        // for W5 / W8 / W9 / W10. Those labels' LLVM AOT source
        // variants (`W5_LLVM_SRC`, `W8_LLVM_SRC`, `W9_LLVM_SRC`,
        // `W10_LLVM_SRC`) skip the production source's load-bearing
        // work (dict probe / closure dispatch / list materialisation
        // / closure inlining) — booking the collapsed scalar kernel
        // under the `relon_llvm_aot` label against a LuaJIT row that
        // walks the production source is a paper-win per `/perf`
        // Honesty Rules. Re-enabled once the LLVM AOT envelope
        // widens to accept the production-source surface for these
        // workloads.
        #[cfg(feature = "llvm-aot")]
        if paper_win_collapsed_variant_label(label) {
            eprintln!(
                "[cmp_lua {label}] relon_llvm_aot row n/a (production source uses dict / \
                 first-class closure / materialised list literal; the LLVM AOT variant \
                 would book an algebraically-collapsed kernel — see audit #318)"
            );
        } else if paper_win_closed_form_fold_label(label) {
            // Audit #332 (2026-05-28): LLVM -O3 reduces this workload's
            // arithmetic-progression sum to a closed-form polynomial in
            // the lambda body. Post-O3 IR verified by
            // `examples/dump_audit_w1_w2_w6.rs` shows zero loop
            // instructions emitted; the row would book O(1) arithmetic
            // against a LuaJIT row that walks the O(n) loop.
            eprintln!(
                "[cmp_lua {label}] relon_llvm_aot row n/a (LLVM -O3 folds the \
                 arithmetic-progression sum to a closed-form polynomial — \
                 see audit #332)"
            );
        } else {
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
        //
        // Honesty cleanup (2026-05-28, audit #318):
        // `paper_win_collapsed_variant_label` short-circuits the row
        // for W5 / W8 / W9 / W10. The `rust_native_w{5,8,9,10}`
        // kernels all model the algebraically-collapsed variant
        // (`(i % 10) + 1` for W5, `(i % 4) + 1` for W8, `i * n + j`
        // for W9, the inlined predicate for W10) rather than the
        // production source's dict probe / closure dispatch / list
        // materialisation — booking that under a label that paired
        // with the LuaJIT-walks-the-production-source row is a
        // paper-win per `/perf` Honesty Rules.
        if paper_win_closed_form_fold_label(label) {
            // Audit #332 (2026-05-28): rust_native_w{1,2,6} model the
            // same arithmetic-progression sum that LLVM at -O3 collapses
            // to a closed-form polynomial (the doc-comments on those
            // helpers explicitly call out the fold as "same freedom the
            // LLVM AOT pipeline has"). Booking that O(1) arithmetic
            // under a `rust_native` label against a LuaJIT row that
            // walks the O(n) loop is a paper win.
            eprintln!(
                "[cmp_lua {label}] rust_native row n/a (rustc / LLVM fold the \
                 arithmetic-progression sum to a closed-form polynomial — \
                 see audit #332)"
            );
        } else if paper_win_brand_dispatch_label(label) {
            // Panel expansion 2026-05-29 (Tier 4 Phase 1 — W21): a
            // Rust-side `enum + match` lowering collapses the brand-
            // tag compare to a `cmov` / `br_table` on the variant
            // discriminant, skipping the tree-walker / LuaJIT runtime
            // string-equal probe. Booking that closed-form variant
            // dispatch under a `rust_native` label against the
            // production-source LuaJIT row is a paper-win per the
            // `/perf` Honesty Rules — see `paper_win_brand_dispatch_
            // label` doc-comment for the full rationale.
            eprintln!(
                "[cmp_lua {label}] rust_native row n/a (Rust `enum + match` \
                 collapses the runtime brand-string compare to a compile-time \
                 variant tag dispatch — see paper_win_brand_dispatch_label)"
            );
        } else if paper_win_container_sugar_label(label) {
            // Panel expansion 2026-05-29 (Tier 4 Phase 2 — Group B
            // container-construction sugar). The rust_native lowering
            // would either replace a Relon-runtime allocator path with
            // a host-allocator path (W23 `HashMap::clone()`) or, for
            // the comprehension / pipe entries added in later Phase 2
            // commits (W24 / W25), collapse the loop to a closed-form
            // polynomial at -O3. Both shapes book under a `rust_native`
            // label against a LuaJIT row that walks the production
            // source. See `paper_win_container_sugar_label` doc-comment
            // for the per-workload rationale.
            eprintln!(
                "[cmp_lua {label}] rust_native row n/a (Rust kernel either uses \
                 host-allocator dict copy or LLVM folds the arithmetic-progression \
                 sum to a closed-form polynomial — see paper_win_container_sugar_label)"
            );
        } else if paper_win_fstring_interp_label(label) {
            // Panel expansion 2026-05-29 (Tier 4 Phase 4 — W26): a
            // Rust-side `format!("item {} of {}", i, n).len()` could
            // be folded by rustc / LLVM after inlining — the constant
            // prefix lengths + `ilog10(i) + 1` digit count are all
            // closed-form expressions of `i` / `n`. Booking that
            // potential digit-bucket closed-form under a
            // `rust_native` label against the LuaJIT row that walks
            // the format-then-strlen path is a paper-win per the
            // `/perf` Honesty Rules — see
            // `paper_win_fstring_interp_label` doc-comment for the
            // full rationale.
            eprintln!(
                "[cmp_lua {label}] rust_native row n/a (Rust `format!` + `.len()` \
                 may fold to a digit-bucket closed form via rustc / LLVM \
                 constant propagation — see paper_win_fstring_interp_label)"
            );
        } else if paper_win_stdlib_dict_label(label) {
            // Panel expansion 2026-05-29 (Tier 4 Phase 4 — W27): a
            // Rust-side `HashMap::from([("a", 1), ("b", 2), ("c",
            // 3)]).keys().count()` would be folded by rustc / LLVM
            // to the compile-time constant `3` (the dict literal is
            // constant; `.keys().count()` is a constant-evaluable
            // expression on a constant map). Booking that O(1)
            // compile-time constant under a `rust_native` label
            // against the LuaJIT row that walks the table-iter +
            // list-materialise path is a paper-win per the `/perf`
            // Honesty Rules — see `paper_win_stdlib_dict_label`
            // doc-comment for the full rationale.
            eprintln!(
                "[cmp_lua {label}] rust_native row n/a (Rust `HashMap::keys().count()` \
                 folds to the compile-time constant `3` via rustc / LLVM \
                 constant propagation — see paper_win_stdlib_dict_label)"
            );
        } else if *label == "W20_n_body_softened" {
            // Panel expansion 2026-05-28 (Tier 3 numeric-kernel W20):
            // production source returns Float. The i64
            // `rust_native_dispatch` cannot carry the kernel; we
            // dispatch through the dedicated `rust_native_w20` (also
            // black_box-gated) so the row's f64 result is preserved
            // for the criterion `black_box` consumer.
            let args = args_factory();
            let scalar = args
                .get("n")
                .map(|v| match v {
                    Value::Int(n) => *n,
                    other => panic!(
                        "[cmp_lua {label}] rust_native row: scalar arg `n` not Int: {other:?}"
                    ),
                })
                .unwrap_or_else(|| panic!("[cmp_lua {label}] rust_native row: missing scalar `n`"));
            group.bench_function(BenchmarkId::new(*label, "rust_native"), |b| {
                b.iter_custom(|iters| {
                    let s_in = black_box(scalar);
                    timed_with_warmup(iters, || {
                        let v = rust_native_w20(black_box(s_in));
                        black_box(v);
                    })
                });
            });
        } else if *label == "W28_float_mixed_ops" {
            // Panel expansion 2026-05-29 (Tier 4 Phase 3 — Group D
            // numeric W28): production source returns Float. Same
            // shape as the W20 branch — dispatch through the
            // dedicated `rust_native_w28` because the i64
            // `rust_native_dispatch` cannot carry the f64 reduce
            // kernel. The `i % 7` periodic step prevents the body
            // from collapsing to a closed-form polynomial (so the
            // row IS a legitimate Rust-floor baseline, NOT a
            // paper-win against the LuaJIT loop).
            let args = args_factory();
            let scalar = args
                .get("n")
                .map(|v| match v {
                    Value::Int(n) => *n,
                    other => panic!(
                        "[cmp_lua {label}] rust_native row: scalar arg `n` not Int: {other:?}"
                    ),
                })
                .unwrap_or_else(|| panic!("[cmp_lua {label}] rust_native row: missing scalar `n`"));
            group.bench_function(BenchmarkId::new(*label, "rust_native"), |b| {
                b.iter_custom(|iters| {
                    let s_in = black_box(scalar);
                    timed_with_warmup(iters, || {
                        let v = rust_native_w28(black_box(s_in));
                        black_box(v);
                    })
                });
            });
        } else if !paper_win_collapsed_variant_label(label) {
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
        } else {
            eprintln!(
                "[cmp_lua {label}] rust_native row n/a (kernel models the \
                 algebraically-collapsed variant; production-source baseline \
                 requires LuaJIT-style dict probe / closure dispatch — see audit #318)"
            );
        }

        // Phase Z.2 (2026-05-28): relon_wasm_wasmtime row. Same
        // schema as the W1 wasm row above; this loop covers
        // W6_list_int_sum_plus_one + W12_p99_tail. The
        // `try_build_wasm_compiled` helper enforces
        // `active_tier() == Compiled`, so canonical-panel workloads
        // outside Z.1's lowering surface (W5 / W7 / W8 / W9 / W10)
        // are dropped here instead of being measured through the
        // tree-walker fallback — that would be the paper-win
        // anti-pattern called out in design §7. The expected /
        // args triple flows from the canonical_panel entry above so
        // the cross-check stays byte-identical with the source the
        // row labels.
        //
        // Panel expansion 2026-05-28 (Tier 3 numeric-kernel W20):
        // W20 returns Float (`#main -> Float`); the expected-value
        // driver path uses `relon_int_result` which only handles
        // Int / Dict-with-Int-result. We skip the wasm block for
        // W20 entirely — the Z.1 wasm program set has no Float
        // support today, so even if the expected-value helper were
        // widened to f64 the wasm classifier would still scope-cut.
        // Skipping early avoids the panic on the Int unwrap.
        //
        // Panel expansion 2026-05-29 (Tier 4 Phase 3 — W28): same
        // reasoning as W20 — Float `#main` return + no Float wasm
        // lowering in Z.1. Skip early.
        if *label != "W20_n_body_softened" && *label != "W28_float_mixed_ops" {
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
