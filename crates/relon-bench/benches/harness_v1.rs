//! Relon performance bench harness — stage P0 measurement infra.
//!
//! Seven dimension-specific `BenchmarkGroup`s, each emitting criterion's
//! statistical report (mean / median / stddev / outliers):
//!
//! * `parse` — three corpus sizes (simple / medium / large list).
//! * `eval_cold` — full per-iteration assembly cost: build context, build
//!   evaluator, parse, eval. Captures one-shot module-resolver +
//!   allocator warm-up overhead.
//! * `eval_steady` — reuse pre-built context + parsed node across every
//!   iter so the sample isolates pure eval dispatch throughput on a
//!   tiny arithmetic body.
//! * `comprehension` — `[x*2 for x in range(N) if x%2==0]` at three
//!   scales (100 / 1000 / 5000) to verify the interpreter stays linear
//!   (no quadratic blow-up).
//! * `reference` — `&sibling` resolution: shallow (1-hop) vs nested
//!   5-layer chain, in the same dict.
//! * `schema_validate` — `#schema User { String name: *, Int age: * }`
//!   applied to a dict literal of that shape.
//! * `method_dispatch` — stdlib schema-rooted methods: `s.upper()` and
//!   `xs.map((n) => n * 2)`, repeated, with receivers pushed via
//!   `run_main(...)` so analyzer-supplied receiver types make the
//!   schema-rooted dispatch path resolve at validation time.
//!
//! Conventions:
//! * Inputs are wrapped in `black_box(...)` at the eval site so the
//!   optimizer can't constant-fold the corpus into the bench shell.
//! * Steady-state groups hoist parse + context build into the `iter_with_setup`
//!   closure preamble so the measured region only covers the timed phase.
//! * All groups run on the workspace `[profile.release]` (fat LTO + 1
//!   codegen unit) when invoked via `cargo bench`.

use std::collections::HashMap;
use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use relon_evaluator::{Context, Scope, StdModuleResolver, TreeWalkEvaluator, Value};
use relon_parser::{parse_document, Node};

// ---------------------------------------------------------------------------
// Corpora
// ---------------------------------------------------------------------------

/// Simple arithmetic: matches the legacy `simple` source used in the
/// 2026-05-12 baseline so criterion numbers stay comparable.
const SIMPLE_SRC: &str = "{ val: 1 + 2 * 3 / 4.0 }";

/// Medium corpus: one `#schema` declaration + one decorator on a field,
/// exercising analyzer-visible shape without ballooning source size.
const MEDIUM_SRC: &str = r#"{
    #schema User: {
        String name: *,
        #default 0 Int age: *
    },
    User alice: { name: "Alice", age: 30 }
}"#;

/// Schema corpus for the dedicated `schema_validate` group. Kept separate
/// from MEDIUM so each group's source describes exactly one perf axis.
const SCHEMA_VALIDATE_SRC: &str = r#"{
    #schema User: {
        String name: *,
        Int age: *
    },
    User alice: { name: "Alice", age: 30 }
}"#;

/// Reference corpora: shallow vs nested. Built lazily so the source size
/// is visible in code review rather than hidden behind a generator.
const REFERENCE_SHALLOW_SRC: &str = r#"{
    "base": { "value": 42 },
    "view": &sibling.base
}"#;

/// Nested-5: every step takes one fresh `&sibling` hop within the same
/// dict scope (sibling references resolve against the enclosing dict's
/// other fields, not the parent dict). Final `view` reads through a
/// 5-link chain `l5 -> l4 -> l3 -> l2 -> l1 -> 1`.
const REFERENCE_NESTED_5_SRC: &str = r#"{
    "l1": 1,
    "l2": &sibling.l1,
    "l3": &sibling.l2,
    "l4": &sibling.l3,
    "l5": &sibling.l4,
    "view": &sibling.l5
}"#;

/// Build a large-list literal source `[0, 1, 2, ..., N-1]`. Used by the
/// `parse` group's "large" sample to stress the lexer/parser allocation
/// path without bringing comprehension / iter machinery into the picture.
fn build_large_list_src(n: usize) -> String {
    let mut s = String::with_capacity(8 * n + 16);
    s.push_str("{ items: [");
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&i.to_string());
    }
    s.push_str("] }");
    s
}

/// `[x*2 for x in range(N) if x%2==0]` — comprehension corpus generator.
fn build_comprehension_src(n: usize) -> String {
    format!("{{ out: [x * 2 for x in range({n}) if x % 2 == 0] }}")
}

/// `s.upper()` repeated N times into a record. Wrapped in `#main(String s)`
/// so the analyzer knows the receiver is `String` and method dispatch
/// resolves at validation time. Each output field walks one method site;
/// at N >= ~50 the dict-pair overhead is amortised and the dispatch cost
/// dominates the per-iter cost.
fn build_method_upper_src(n: usize) -> String {
    let mut s = String::from("#relaxed\n#main(String s)\n{");
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(" f{i}: s.upper()"));
    }
    s.push('}');
    s
}

/// `xs.map((n) => n * 2)` invoked N times against a host-pushed `List<Int>`.
/// Each invocation walks the closure adapter inside the stdlib schema-rooted
/// `List.map` impl. The receiver list is passed via `run_main` so we
/// allocate it once per iter rather than re-parsing a literal each time.
fn build_method_map_src(n: usize) -> String {
    let mut s = String::from("#relaxed\n#main(List<Int> xs)\n{");
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(" List<Int> m{i}: xs.map((x) => x * 2)"));
    }
    s.push('}');
    s
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a fresh root-mounted context. Mirrors what `relon` facade does
/// for a fresh script: register the std module resolver, mount root,
/// and wrap in `Arc`. Pure helper — no measurement happens here.
fn fresh_context_with_root(node: Node) -> Arc<Context> {
    let mut ctx = Context::new().with_root(node);
    ctx.prepend_module_resolver(Arc::new(StdModuleResolver));
    Arc::new({
        let mut ctx = ctx;
        relon_evaluator::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    })
}

/// Re-parse + rebuild a fresh `(Arc<Context>, root Node, fresh Scope)` for
/// one bench iteration. Used by `eval_cold` so each sample observes the
/// full first-call cost (parse + assembly + eval).
fn full_setup(source: &str) -> (Arc<Context>, Node, Arc<Scope>) {
    let ast = parse_document(source).expect("parse failed");
    let ctx = fresh_context_with_root(ast.clone());
    (ctx, ast, Arc::new(Scope::default()))
}

// ---------------------------------------------------------------------------
// Group 1: parse
// ---------------------------------------------------------------------------

fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse");

    // Simple: short arithmetic source — measures fixed parser overhead.
    group.bench_function(BenchmarkId::new("source", "simple"), |b| {
        b.iter(|| {
            let n = parse_document(black_box(SIMPLE_SRC)).expect("parse");
            black_box(n)
        });
    });

    // Medium: one #schema + one decorator. Adds analyzer-visible nodes
    // (schema decl, default decorator) without touching loops.
    group.bench_function(BenchmarkId::new("source", "medium"), |b| {
        b.iter(|| {
            let n = parse_document(black_box(MEDIUM_SRC)).expect("parse");
            black_box(n)
        });
    });

    // Large: 1000-elem inline list literal. Exercises lexer alloc + AST
    // vec growth for List nodes without bringing eval into the picture.
    let large_src = build_large_list_src(1000);
    group.throughput(Throughput::Elements(1000));
    group.bench_function(BenchmarkId::new("source", "large_list_1000"), |b| {
        b.iter(|| {
            let n = parse_document(black_box(large_src.as_str())).expect("parse");
            black_box(n)
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 2: eval_cold
// ---------------------------------------------------------------------------

fn bench_eval_cold(c: &mut Criterion) {
    let mut group = c.benchmark_group("eval_cold");

    // Cold means "every sample reassembles the whole stack". Use
    // `iter_with_large_drop` so the dropped artefacts don't pollute the
    // timed region with deallocation cost.
    group.bench_function(BenchmarkId::new("source", "simple"), |b| {
        b.iter_with_large_drop(|| {
            let (ctx, node, scope) = full_setup(SIMPLE_SRC);
            let eval = TreeWalkEvaluator::new(Arc::clone(&ctx));
            let result = eval.eval(&node, &scope).expect("eval");
            black_box((ctx, result))
        });
    });

    group.bench_function(BenchmarkId::new("source", "medium"), |b| {
        b.iter_with_large_drop(|| {
            let (ctx, node, scope) = full_setup(MEDIUM_SRC);
            let eval = TreeWalkEvaluator::new(Arc::clone(&ctx));
            let result = eval.eval(&node, &scope).expect("eval");
            black_box((ctx, result))
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 3: eval_steady
// ---------------------------------------------------------------------------

fn bench_eval_steady(c: &mut Criterion) {
    let mut group = c.benchmark_group("eval_steady");

    // Hoist parse + context build out of the measured region. The
    // closure body covers only `eval` invocation + fresh scope alloc
    // (cheap, dwarfed by interpreter dispatch).
    let ast = parse_document(SIMPLE_SRC).expect("parse");
    let ctx = fresh_context_with_root(ast.clone());
    let eval = TreeWalkEvaluator::new(Arc::clone(&ctx));

    group.bench_function(BenchmarkId::new("source", "simple"), |b| {
        b.iter(|| {
            let scope = Arc::new(Scope::default());
            let r = eval.eval(black_box(&ast), &scope).expect("eval");
            black_box(r)
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 4: comprehension
// ---------------------------------------------------------------------------

fn bench_comprehension(c: &mut Criterion) {
    let mut group = c.benchmark_group("comprehension");

    // Each parameter pre-parses once; the timed region rebuilds ctx +
    // evaluator + runs eval. We mark element-throughput so criterion
    // reports per-element cost, making linearity inspection trivial.
    for &n in &[100usize, 1000, 5000] {
        let src = build_comprehension_src(n);
        let ast = parse_document(&src).expect("parse");
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("elements", n), &ast, |b, ast| {
            b.iter_with_large_drop(|| {
                let ctx = fresh_context_with_root(ast.clone());
                let eval = TreeWalkEvaluator::new(Arc::clone(&ctx));
                let scope = Arc::new(Scope::default());
                let r = eval.eval(black_box(ast), &scope).expect("eval");
                black_box((ctx, r))
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 5: reference
// ---------------------------------------------------------------------------

fn bench_reference(c: &mut Criterion) {
    let mut group = c.benchmark_group("reference");

    // Two probes: 1-hop sibling vs 5-layer nested chain. The
    // measurement should be roughly proportional to chain depth if
    // reference resolution is linear in chain length.
    let shallow = parse_document(REFERENCE_SHALLOW_SRC).expect("parse");
    let nested = parse_document(REFERENCE_NESTED_5_SRC).expect("parse");

    group.bench_function(BenchmarkId::new("depth", "shallow_1"), |b| {
        b.iter_with_large_drop(|| {
            let ctx = fresh_context_with_root(shallow.clone());
            let eval = TreeWalkEvaluator::new(Arc::clone(&ctx));
            let scope = Arc::new(Scope::default());
            let r = eval.eval(black_box(&shallow), &scope).expect("eval");
            black_box((ctx, r))
        });
    });

    group.bench_function(BenchmarkId::new("depth", "nested_5"), |b| {
        b.iter_with_large_drop(|| {
            let ctx = fresh_context_with_root(nested.clone());
            let eval = TreeWalkEvaluator::new(Arc::clone(&ctx));
            let scope = Arc::new(Scope::default());
            let r = eval.eval(black_box(&nested), &scope).expect("eval");
            black_box((ctx, r))
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 6: schema_validate
// ---------------------------------------------------------------------------

fn bench_schema_validate(c: &mut Criterion) {
    let mut group = c.benchmark_group("schema_validate");

    // Parses once; each iter rebuilds ctx + runs eval — schema
    // validation fires as part of the dict literal binding pass on
    // the typed declaration `User alice: { ... }`, so the timed region
    // includes that whole validation path.
    let ast = parse_document(SCHEMA_VALIDATE_SRC).expect("parse");

    group.bench_function(BenchmarkId::new("schema", "user_2fields"), |b| {
        b.iter_with_large_drop(|| {
            let ctx = fresh_context_with_root(ast.clone());
            let eval = TreeWalkEvaluator::new(Arc::clone(&ctx));
            let scope = Arc::new(Scope::default());
            let r = eval.eval(black_box(&ast), &scope).expect("eval");
            black_box((ctx, r))
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 7: method_dispatch
// ---------------------------------------------------------------------------

fn bench_method_dispatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("method_dispatch");

    // Both probes use `#main(...)` so the analyzer attributes static
    // receiver types — that's the trigger condition for the schema-rooted
    // method dispatch fast path. Without it, `s.upper()` / `xs.map(...)`
    // would fail with `FunctionNotFound`. We pre-parse + pre-analyze once;
    // the timed region rebuilds context and runs `run_main`.

    // String.upper × 100. Host pushes `s = "abc"`; each field site fires
    // one method dispatch to the stdlib `String.upper` pure method.
    let upper_src = build_method_upper_src(100);
    let upper_ast = parse_document(&upper_src).expect("parse upper corpus");
    let upper_analyzed = Arc::new(relon_analyzer::analyze(&upper_ast));
    group.throughput(Throughput::Elements(100));
    group.bench_function(BenchmarkId::new("op", "string_upper_x100"), |b| {
        b.iter_with_large_drop(|| {
            let mut ctx = Context::new().with_root(upper_ast.clone());
            ctx.prepend_module_resolver(Arc::new(StdModuleResolver));
            let ctx = Arc::new(ctx.with_analyzed(Arc::clone(&upper_analyzed)));
            let eval = TreeWalkEvaluator::new(Arc::clone(&ctx));
            let scope = Arc::new(Scope::default());
            let mut args = HashMap::with_capacity(1);
            args.insert("s".to_string(), Value::String("abc".to_string()));
            let r = eval.run_main(&scope, black_box(args)).expect("run_main");
            black_box((ctx, r))
        });
    });

    // List.map × 100. Host pushes a 10-elem `List<Int>`; each field site
    // walks the closure adapter path inside the stdlib `List.map` impl.
    let map_src = build_method_map_src(100);
    let map_ast = parse_document(&map_src).expect("parse map corpus");
    let map_analyzed = Arc::new(relon_analyzer::analyze(&map_ast));
    let xs_payload: Vec<Value> = (0i64..10).map(Value::Int).collect();
    group.throughput(Throughput::Elements(100));
    group.bench_function(BenchmarkId::new("op", "list_map_x100"), |b| {
        b.iter_with_large_drop(|| {
            let mut ctx = Context::new().with_root(map_ast.clone());
            ctx.prepend_module_resolver(Arc::new(StdModuleResolver));
            let ctx = Arc::new(ctx.with_analyzed(Arc::clone(&map_analyzed)));
            let eval = TreeWalkEvaluator::new(Arc::clone(&ctx));
            let scope = Arc::new(Scope::default());
            let mut args = HashMap::with_capacity(1);
            args.insert("xs".to_string(), Value::list(xs_payload.clone()));
            let r = eval.run_main(&scope, black_box(args)).expect("run_main");
            black_box((ctx, r))
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// criterion entry
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_parse,
    bench_eval_cold,
    bench_eval_steady,
    bench_comprehension,
    bench_reference,
    bench_schema_validate,
    bench_method_dispatch,
);
criterion_main!(benches);
