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

use relon_codegen_wasm::{AotCache, WasmAotEvaluator};
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

/// v3+ b-1 user-fn DCE scenario payload. Same shape as `arith_args`
/// minus the unused `y` slot — the entry only reads `x` so the
/// host side keeps the args minimal. The schema declared on the
/// source has five methods that DCE prunes; the `args` here mirror
/// only what `#main(Int x)` actually consumes.
fn arith_single_int_args() -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert("x".to_string(), Value::Int(7));
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

/// v3+ a-4 `stdlib_upper` payload. Longer + ASCII-only so the
/// bench surfaces the UTF-8 decode/encode loop overhead per byte
/// without spending most cycles in the case-folding-table binary
/// search (which fast-returns on a miss for non-letter ASCII). The
/// input is exactly 100 bytes — the wasm body's per-codepoint
/// overhead times 100 gives a stable signal in the microsecond
/// range that comfortably differentiates the new pipeline from
/// the Phase 4.c-2 byte-flip fast path.
fn string_upper_args() -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    // 100-byte ASCII payload — five repeats of a 20-byte sentence.
    // Keeps the worst-case scratch growth at 4x = 400 bytes which
    // is well inside the default test out_cap.
    let s = "the quick brown fox ".repeat(5);
    debug_assert_eq!(s.len(), 100);
    m.insert("s".to_string(), Value::String(s));
    m
}

/// v3++ b-4 `stdlib_title` payload. Exercises the three new code
/// paths the title body adds on top of the v3+ a-4 case-fold
/// pipeline:
///
///   * Latin segment with a combining mark: the wasm body must
///     binary-search the combining-mark range table for every
///     codepoint and skip the case fold on a hit. The `cafe + U+0301`
///     fragment lands a mark on the per-codepoint hot path.
///   * CJK + ideographic space (U+3000): non-ASCII whitespace pushes
///     the title body through its non-ASCII-whitespace branch
///     instead of the ASCII fast path, so the bench surfaces both
///     paths.
///   * Emoji ZWJ sequence: the family-emoji `MAN + ZWJ + WOMAN +
///     ZWJ + GIRL + ZWJ + BOY` covers the 4-byte UTF-8 decode arm
///     plus the ZWJ (U+200D Format char, not Mark) passthrough.
///
/// Total byte length stays under 256 so the wasm scratch alloc
/// (4 + len * 4 = ~1 KB worst case) sits well inside the default
/// out_cap.
fn string_title_args() -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    // ASCII multi-word + combining mark + CJK + ideographic-space +
    // ZWJ family emoji. Stays UTF-8 valid; the wasm body's invalid-
    // UTF-8 trap path is exercised by the smoke tests, not bench.
    let s = "the quick brown fox cafe\u{0301} bar \
             \u{4F60}\u{597D}\u{3000}hello \
             \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}"
        .to_string();
    m.insert("s".to_string(), Value::String(s));
    m
}

/// v3++ b-5 `string_normalization` payload. Exercises the four UAX
/// #15 stdlib bodies (`nfc` / `nfd` / `nfkc` / `nfkd`) on a mixed
/// input that lights up every code path the bodies care about:
///
///   * Latin segment with **pre-composed** combining-mark accents
///     (cafe + U+0301 stays decomposable for NFD; NFC must compose
///     it back when fed already-decomposed input).
///   * **Hangul** syllables that take the algorithmic decompose +
///     compose fast path instead of a table hit (the syllable block
///     is left out of the data tables on purpose to save ~88 KB).
///   * **Half-width fraction** (U+00BD) that only NFKD / NFKC touch
///     via the compatibility table - drives the NFKD pool which is
///     larger than the NFD pool.
///   * **Emoji ZWJ sequence** that round-trips unchanged in all
///     forms (cps are non-decomposable, non-composable).
///
/// Input length stays under 256 bytes so the per-cp 4-byte scratch
/// allocations stay well inside one wasm page.
fn string_normalization_args() -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    let s = "cafe\u{0301} bar \u{D55C}\u{AD6D}\u{C5B4} \u{00BD} \
             \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}"
        .to_string();
    m.insert("s".to_string(), Value::String(s));
    m
}

/// Phase 10-a scenarios: a 32-element `List<Int>` driving the
/// higher-order `map` / `filter` / `fold` stdlib bodies. The list
/// length is chosen large enough that per-iteration `call_indirect`
/// overhead dominates the per-iter cost (so the bench surfaces the
/// closure-dispatch path) but small enough to keep criterion's
/// sample-size budget reasonable.
fn list_int_args() -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    let xs: Vec<Value> = (1..=32).map(Value::Int).collect();
    m.insert("xs".to_string(), Value::List(xs.into()));
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
    // v3+ a-4: drives the Unicode-aware `upper` stdlib body. The
    // wasm side decodes each UTF-8 codepoint, binary-searches the
    // simple upper-folding table, and re-encodes — so the bench
    // surfaces per-codepoint UTF-8 overhead, the table-lookup cost,
    // and the cold-start data-section bytes (~12 KB upper table).
    // The 100-byte ASCII payload keeps the steady-state byte count
    // predictable and avoids tripping the multi-byte UTF-8 paths so
    // the cold-start numbers measure only the table-emit overhead,
    // not also per-codepoint multi-byte work.
    Scenario {
        name: "stdlib_upper",
        wasm_source: "#main(String s) -> String\ns.upper()",
        tree_source: "#main(String s) -> String\ns.upper()",
        args: string_upper_args,
    },
    // v3++ b-4: drives the new title-case body. The payload mixes
    // ASCII whitespace, a combining mark, an ideographic space
    // (U+3000), and an emoji ZWJ sequence so all three new code
    // paths the title body adds on top of upper/lower (combining
    // mark skip, non-ASCII whitespace branch, word-boundary state
    // machine) light up under the per-iter measurement. The
    // cold-start number also captures the two new data-section
    // tables (combining marks + non-ASCII whitespace ranges) the
    // wasm module now embeds.
    Scenario {
        name: "stdlib_title",
        wasm_source: "#main(String s) -> String\ns.title()",
        tree_source: "#main(String s) -> String\ns.title()",
        args: string_title_args,
    },
    // v3++ b-5: drives the four UAX #15 normalization bodies. The
    // wasm side walks the input through decompose -> canonical
    // reorder -> (NFC/NFKC only) compose -> re-encode, against the
    // embedded UCD 14.0.0 tables. Hangul syllables in the payload
    // exercise the algorithmic decompose / compose path; the
    // half-width fraction (U+00BD) drives the NFKD compatibility
    // pool which is roughly 4x the size of the NFD pool. Cold-start
    // numbers also capture the four new data-section tables
    // (NFD index/pool, NFKD index/pool, CCC, composition pairs)
    // the wasm module now embeds.
    Scenario {
        name: "stdlib_nfc",
        wasm_source: "#main(String s) -> String\ns.nfc()",
        tree_source: "#main(String s) -> String\ns.nfc()",
        args: string_normalization_args,
    },
    Scenario {
        name: "stdlib_nfd",
        wasm_source: "#main(String s) -> String\ns.nfd()",
        tree_source: "#main(String s) -> String\ns.nfd()",
        args: string_normalization_args,
    },
    Scenario {
        name: "stdlib_nfkc",
        wasm_source: "#main(String s) -> String\ns.nfkc()",
        tree_source: "#main(String s) -> String\ns.nfkc()",
        args: string_normalization_args,
    },
    Scenario {
        name: "stdlib_nfkd",
        wasm_source: "#main(String s) -> String\ns.nfkd()",
        tree_source: "#main(String s) -> String\ns.nfkd()",
        args: string_normalization_args,
    },
    // Phase 10-a closure surfaces. Each scenario runs the same
    // higher-order body on both backends; the wasm side exercises
    // the freshly added `list_int_map` / `_filter` / `_fold`
    // stdlib bodies through the closure conversion + `call_indirect`
    // path. The tree-walker's bundled `List<Int>` schema accepts
    // `map` / `filter` / `fold` natively, so the surface stays
    // identical between the two backends.
    Scenario {
        name: "list_int_map",
        wasm_source: "#main(List<Int> xs) -> List<Int>\nxs.map((Int x) => x * 2)",
        tree_source: "#main(List<Int> xs) -> List<Int>\nxs.map((Int x) => x * 2)",
        args: list_int_args,
    },
    Scenario {
        name: "list_int_filter",
        wasm_source: "#main(List<Int> xs) -> List<Int>\nxs.filter((Int x) => x > 16)",
        tree_source: "#main(List<Int> xs) -> List<Int>\nxs.filter((Int x) => x > 16)",
        args: list_int_args,
    },
    // `fold` is the wasm-AOT IR's name for the left-fold reducer;
    // the tree-walker's bundled `List<T>` schema (see
    // `crates/relon-analyzer/src/core/list.relon`) exports it as
    // `reduce`. Same semantics, surface-name drift documented for
    // the existing `length` / `len` pair just below.
    Scenario {
        name: "list_int_fold",
        wasm_source: "#main(List<Int> xs) -> Int\nxs.fold(0, (Int acc, Int x) => acc + x)",
        tree_source: "#main(List<Int> xs) -> Int\nxs.reduce(0, (Int acc, Int x) => acc + x)",
        args: list_int_args,
    },
    // v3+ b-1 user-fn DCE surface. Five schema methods declared,
    // none referenced from the entry. wasm-AOT prunes all five
    // method bodies post-DCE so the resulting module is the same
    // size as the `arithmetic` baseline; the bench picks up the
    // cold-start reduction relative to a build that kept every
    // declared method. The tree-walker pays a tiny per-iter
    // overhead from parsing the larger source but does not call
    // any of the pruned methods either.
    Scenario {
        name: "unused_methods",
        wasm_source: "#schema U { Int x: * } with {\n  \
            a() -> Int: self.x\n  \
            b() -> Int: self.x * 2\n  \
            c() -> Int: self.x + 1\n  \
            d() -> Int: self.x - 1\n  \
            e() -> Int: self.x * self.x\n\
         }\n\
         #main(Int x) -> Int\nx * 2",
        tree_source: "#schema U { Int x: * } with {\n  \
            a() -> Int: self.x\n  \
            b() -> Int: self.x * 2\n  \
            c() -> Int: self.x + 1\n  \
            d() -> Int: self.x - 1\n  \
            e() -> Int: self.x * self.x\n\
         }\n\
         #main(Int x) -> Int\nx * 2",
        args: arith_single_int_args,
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
// Cold start with disk cache: wasm-AOT only — re-build from cache per iter.
// ---------------------------------------------------------------------------

fn bench_cold_start_cached(c: &mut Criterion) {
    let mut group = c.benchmark_group("wasm_aot_cold_start_cached");
    // Same measurement budget as the bare cold-start group: we still
    // pay `wasmtime::Module::new` (cranelift JIT) on every iter, just
    // skip the parse / analyze / lower / codegen steps via the cache.
    // The expected reduction is the codegen pipeline cost; target
    // landing zone is in the 1-1.5 ms range vs the ~2.2 ms uncached
    // baseline.
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(8));

    // Phase 9.b-3 cache rebuild path: each scenario gets its own temp
    // cache directory (so reruns of the bench start fresh). Prime the
    // cache once outside the timed region so the per-iter measurement
    // is a pure hit-path workload.
    let temp_root = std::env::temp_dir().join(format!(
        "relon-bench-aot-cache-{pid}-{nanos}",
        pid = std::process::id(),
        nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&temp_root).expect("create bench cache root");

    for sc in SCENARIOS {
        let dir = temp_root.join(sc.name);
        let cache = AotCache::open(&dir).expect("open bench cache");
        // Prime: first call populates the cache so every measured iter
        // hits the fast path.
        let _primed =
            WasmAotEvaluator::from_source_with_cache(sc.wasm_source, &cache).expect("prime cache");
        group.bench_function(BenchmarkId::new("scenario", sc.name), |b| {
            b.iter_with_large_drop(|| {
                WasmAotEvaluator::from_source_with_cache(
                    black_box(sc.wasm_source),
                    black_box(&cache),
                )
                .expect("wasm-aot from_source_with_cache")
            });
        });
    }

    group.finish();
    // Best-effort cleanup. Failures are silent because criterion has
    // already finished reporting and the cache lives in /tmp.
    let _ = std::fs::remove_dir_all(&temp_root);
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
    bench_cold_start_cached,
    bench_warm_invoke,
    bench_tree_walk_total,
    bench_tree_walk_warm,
);
criterion_main!(benches);
