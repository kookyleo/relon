//! Phase 9 closeout: criterion comparison between the wasm-AOT backend
//! (`WasmAotEvaluator`) and the tree-walking interpreter
//! (`TreeWalkEvaluator`).
//!
//! Each scenario probes three points along the cost curve:
//!
//! * `cold_start` — wasm-AOT only. Builds `WasmAotEvaluator::from_source`
//!   from scratch every iter; covers parse + analyze + lower + codegen +
//!   `wasmtime::Module::new` (cranelift compile of the host module).
//!   The cost we'd pay on first call before any caching.
//! * `warm_invoke` — wasm-AOT only. Reuses a single preassembled
//!   evaluator and only times `run_main(args)`; covers `wasmtime::Store`
//!   creation, `Linker::instantiate`, buffer marshal in, wasm execution,
//!   buffer marshal out. The cost per call once the module is JIT-ready.
//! * `tree_walk_total` — tree-walker baseline. Every iter rebuilds the
//!   `Context` + `TreeWalkEvaluator` and runs `run_main`. The
//!   tree-walker doesn't have a separable "compile" phase the way the
//!   AOT backend does; this number is the apples-to-apples competitor
//!   for `warm_invoke` on the assumption a host re-evaluates a fresh
//!   source per call (the typical configuration-reload workload).
//!
//! Scenarios (all three exercise the high-level
//! `WasmAotEvaluator::from_source` + `Evaluator::run_main` surface so
//! the comparison is apples-to-apples with the tree-walker's
//! `Evaluator` trait impl):
//!
//! * `arithmetic` — `#main(Int x, Int y) -> Int : x * y + 1`. Tightest
//!   loop the backend can express; cold-start is dominated by codegen
//!   overhead, warm invoke by buffer marshal + wasm dispatch.
//! * `dict_literal` — `#main(Int x) -> U` returning a 2-field user
//!   schema. Exercises the BufferReader sub-record decode path on
//!   the wasm-AOT side; tree-walker pays for Dict construction +
//!   schema validation.
//! * `stdlib_length` — `#main(String s) -> Int : s.length()`. Drives
//!   the wasm-AOT pointer-indirect tail record (String input) plus
//!   the stdlib `String.length` schema-rooted method dispatch path.
//!
//! A "method_dispatch" scenario (`#schema V with { doubled() ... }`
//! taking `V` as a `#main` arg) is _not_ included: the wasm-AOT
//! input bridge currently rejects Schema-typed `#main` parameters
//! (`evaluator.rs::write_value_into_builder` returns Unsupported),
//! and the parser doesn't yet accept the inline
//! `V { x: n }.doubled()` form that would side-step the host-input
//! limitation. `method_dispatch_smoke.rs` covers the buffer-level
//! dispatch path; future bench work should wire the host-input
//! side once Schema-typed `#main` args land.
//!
//! All groups are pinned to `sample_size(50)` + `measurement_time(8s)`
//! so the full bench finishes in well under 10 minutes even on a
//! laptop. Criterion's default 100×3s would balloon the runtime
//! without improving the signal — the per-iter cost here ranges
//! from O(μs) to O(ms) so 50 samples is plenty.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use relon_codegen_wasm::WasmAotEvaluator;
use relon_eval_api::Evaluator;
use relon_evaluator::{Context, Scope, StdModuleResolver, TreeWalkEvaluator, Value};

// `relon_eval_api` is wired as a regular dep (not via `relon` facade)
// so the bench can call the `Evaluator` trait method directly without
// turning on the facade's `wasm-aot` cargo feature for every other
// crate in the workspace.

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Each scenario carries the wasm-AOT source plus the equivalent
/// tree-walker source (same semantics, possibly different stdlib
/// name — `s.length()` vs `s.len()` etc., since the two backends'
/// stdlib registries were authored independently and not yet
/// harmonised). The `args` closure returns the canonical run_main
/// payload; we reconstruct it every iter so list / string allocations
/// don't accidentally amortize across samples.
struct Scenario {
    name: &'static str,
    wasm_source: &'static str,
    tree_source: &'static str,
    args: fn() -> HashMap<String, Value>,
}

fn arith_args() -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(2);
    m.insert("x".to_string(), Value::Int(7));
    m.insert("y".to_string(), Value::Int(6));
    m
}

fn dict_args() -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert("x".to_string(), Value::Int(42));
    m
}

fn string_args() -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert(
        "s".to_string(),
        Value::String("the quick brown fox jumps".to_string()),
    );
    m
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "arithmetic",
        wasm_source: "#main(Int x, Int y) -> Int\nx * y + 1",
        tree_source: "#main(Int x, Int y) -> Int\nx * y + 1",
        args: arith_args,
    },
    Scenario {
        name: "dict_literal",
        // Branded user schema return — wasm-AOT emits the sub-record
        // layout, tree-walker constructs a branded Dict + validates.
        wasm_source: "#schema U { Int age: *, Int birth: * }\n\
                      #main(Int x) -> U\n\
                      { age: x, birth: 2026 - x }",
        tree_source: "#schema U { Int age: *, Int birth: * }\n\
                      #main(Int x) -> U\n\
                      { age: x, birth: 2026 - x }",
        args: dict_args,
    },
    Scenario {
        name: "stdlib_length",
        // Phase 4.a names the byte-length intrinsic `length` on the
        // wasm-AOT IR side; the tree-walker's bundled `String` schema
        // (`crates/relon-analyzer/src/core/string.relon`) exports it as
        // `len()` (Decision 21' carrier model). Same semantics, different
        // surface name — keeping both wired here lets us run each
        // backend with its native vocabulary instead of biasing the
        // numbers by stripping the call to the lowest common syntactic
        // denominator. Unifying the names is on the future-work list.
        wasm_source: "#main(String s) -> Int\ns.length()",
        tree_source: "#main(String s) -> Int\ns.len()",
        args: string_args,
    },
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a tree-walker evaluator from `source`. Mirrors what
/// `relon::new_evaluator(_, Backend::TreeWalk)` does — kept local so the
/// bench doesn't need to depend on the facade crate (which would pull
/// the whole `wasm-aot` cargo feature in transitively and confuse the
/// dep graph).
fn build_tree_walker(source: &str) -> TreeWalkEvaluator {
    let node = relon_parser::parse_document(source).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    assert!(
        !analyzed.has_errors(),
        "tree-walker analyzer errors: {:?}",
        analyzed.diagnostics
    );
    // `StdModuleResolver` must be prepended _before_ `with_analyzed`
    // so the schema-rooted method dispatch path can resolve
    // `String.length()` against the bundled stdlib `String` schema.
    // Mirrors the setup `harness_v1::bench_method_dispatch` uses.
    let mut ctx = Context::new().with_root(node);
    ctx.prepend_module_resolver(Arc::new(StdModuleResolver));
    let mut ctx = ctx.with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    TreeWalkEvaluator::new(Arc::new(ctx))
}

/// Run the tree-walker once via the `Evaluator` trait surface so the
/// number is directly comparable with the wasm-AOT warm-invoke path
/// (which uses the same trait method). Uses an empty `Scope` like the
/// trait impl does internally.
fn tree_walk_run(evaluator: &TreeWalkEvaluator, args: HashMap<String, Value>) -> Value {
    let scope = Arc::new(Scope::default());
    evaluator
        .run_main(&scope, args)
        .expect("tree-walk run_main")
}

// ---------------------------------------------------------------------------
// Cold start: wasm-AOT only — full pipeline per iter.
// ---------------------------------------------------------------------------

fn bench_cold_start(c: &mut Criterion) {
    let mut group = c.benchmark_group("wasm_aot_cold_start");
    // Cold-start covers `wasmtime::Module::new` which invokes cranelift;
    // that step is the slowest single piece in the pipeline at ~ ms
    // scale. Trim the sample count + measurement window so the bench
    // group finishes in roughly 60 s even on a quiet laptop.
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(8));

    for sc in SCENARIOS {
        group.bench_function(BenchmarkId::new("scenario", sc.name), |b| {
            b.iter_with_large_drop(|| {
                WasmAotEvaluator::from_source(black_box(sc.wasm_source))
                    .expect("wasm-aot from_source")
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Warm invoke: wasm-AOT only — reuse compiled evaluator across iters.
// ---------------------------------------------------------------------------

fn bench_warm_invoke(c: &mut Criterion) {
    let mut group = c.benchmark_group("wasm_aot_warm_invoke");
    // Warm invoke is the per-call cost we care about for high-frequency
    // callers (configuration evaluation in request handlers, etc).
    // Per-iter cost ranges from low μs to ~100 μs; 50 samples is
    // sufficient for criterion's stddev estimator.
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(8));

    for sc in SCENARIOS {
        // Compile once outside the timed region. `WasmAotEvaluator` is
        // Send + Sync (no interior mutability on the hot path beyond
        // wasmtime's own engine machinery), so we can borrow it across
        // the `b.iter` closure.
        let aot = WasmAotEvaluator::from_source(sc.wasm_source).expect("wasm-aot from_source");
        group.bench_function(BenchmarkId::new("scenario", sc.name), |b| {
            b.iter_with_large_drop(|| {
                let args = (sc.args)();
                aot.run_main(black_box(args)).expect("wasm-aot run_main")
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Tree-walker single-shot baseline: parse + analyze + assemble + eval.
// ---------------------------------------------------------------------------

fn bench_tree_walk_total(c: &mut Criterion) {
    let mut group = c.benchmark_group("tree_walk_total");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(8));

    for sc in SCENARIOS {
        group.bench_function(BenchmarkId::new("scenario", sc.name), |b| {
            b.iter_with_large_drop(|| {
                let evaluator = build_tree_walker(black_box(sc.tree_source));
                let args = (sc.args)();
                tree_walk_run(&evaluator, args)
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Tree-walker warm baseline: reuse the assembled evaluator, time only
// `run_main`. Lets a reader compare the wasm-AOT warm path against the
// equivalent tree-walker warm path on the same source.
// ---------------------------------------------------------------------------

fn bench_tree_walk_warm(c: &mut Criterion) {
    let mut group = c.benchmark_group("tree_walk_warm_invoke");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(8));

    for sc in SCENARIOS {
        let evaluator = build_tree_walker(sc.tree_source);
        group.bench_function(BenchmarkId::new("scenario", sc.name), |b| {
            b.iter_with_large_drop(|| {
                let args = (sc.args)();
                tree_walk_run(&evaluator, args)
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// criterion entry
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_cold_start,
    bench_warm_invoke,
    bench_tree_walk_total,
    bench_tree_walk_warm,
);
criterion_main!(benches);
