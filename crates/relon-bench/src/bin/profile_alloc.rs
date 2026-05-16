#![forbid(unsafe_code)]

//! Workload driver for the Relon parser + evaluator hot paths.
//!
//! Used for two distinct profiling jobs:
//!
//! 1. Heap allocation profiling (dhat) — build with `--features dhat-heap`.
//!    Drops a `dhat-heap.json` in the current working directory on exit;
//!    load it into <https://nnethercote.github.io/dh_view/dh_view.html>.
//! 2. CPU profiling (perf / flamegraph) — build without the feature so the
//!    profiler does not interpose on every allocation. Iteration counts can
//!    be inflated via the `PROFILE_ALLOC_SCALE` env var so perf has enough
//!    samples to draw a meaningful flamegraph. See `scripts/perf-flamegraph.sh`.
//!
//! CLI:
//!   - `simple`               — arithmetic workload, fresh `Context` per iteration
//!   - `simple-pooled`        — arithmetic workload, single `Context` reused across iterations
//!   - `comprehension`        — list-comprehension workload, fresh `Context` per iteration
//!   - `comprehension-pooled` — list-comprehension workload, single `Context` reused
//!   - `all` (default)        — run all four back-to-back
//!
//! Pooled modes parse the AST once and reuse a single `Context` + `Evaluator`,
//! relying on `eval_root` to reset `step_counter` / `path_cache` /
//! `iter_cursors` between calls. `module_cache` is deliberately retained.
//!
//! Env vars:
//!   - `PROFILE_ALLOC_SCALE` — multiplies base iteration counts (default 1).
//!     Set higher (e.g. 200) for CPU profiling so the bin runs long enough
//!     for perf to collect a useful number of samples.

use std::sync::Arc;

use relon_evaluator::{Context, Evaluator, Scope};
use relon_parser::parse_document;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Base iteration count for the "simple" workload (constant-folded
/// arithmetic). Roughly tens of microseconds per round-trip; 1000 is enough
/// for dhat. CPU profiling typically scales this up via `PROFILE_ALLOC_SCALE`.
const SIMPLE_ITERATIONS: usize = 1000;

/// Base iteration count for the "comprehension" workload (1000-element list
/// comprehension plus a sibling reference). Each round-trip is in the
/// low-millisecond range post-P2; 100 is enough for dhat.
const COMPREHENSION_ITERATIONS: usize = 100;

const SIMPLE_SOURCE: &str = "{ val: 1 + 2 * 3 / 4.0 }";

const COMPREHENSION_SOURCE: &str = r#"{
    "list": [x * 2 for x in range(1000) if x % 2 == 0],
    "check": &sibling.list
}"#;

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());
    let selection = arg.as_str();

    let scale: usize = std::env::var("PROFILE_ALLOC_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(1);

    let simple_iters = SIMPLE_ITERATIONS.saturating_mul(scale);
    let comp_iters = COMPREHENSION_ITERATIONS.saturating_mul(scale);

    // Profiler must live for the whole main; on drop it writes the JSON.
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    println!("--- Relon profile_alloc ---");
    println!("workload selection: {}", selection);
    #[cfg(feature = "dhat-heap")]
    println!("(dhat-heap ON — dhat-heap.json will be written on drop)");
    #[cfg(not(feature = "dhat-heap"))]
    println!("(dhat-heap OFF — running workload only; suitable for CPU profiling)");
    if scale > 1 {
        println!(
            "PROFILE_ALLOC_SCALE={} -> simple x{}, comprehension x{}",
            scale, simple_iters, comp_iters
        );
    }

    match selection {
        "simple" => {
            println!("simple workload x{}: {}", simple_iters, SIMPLE_SOURCE);
            run_workload_oneshot("simple", SIMPLE_SOURCE, simple_iters);
        }
        "simple-pooled" => {
            println!("simple-pooled workload x{}: {}", simple_iters, SIMPLE_SOURCE);
            run_workload_pooled("simple-pooled", SIMPLE_SOURCE, simple_iters);
        }
        "comprehension" => {
            println!(
                "comprehension workload x{}: {}",
                comp_iters,
                COMPREHENSION_SOURCE.replace('\n', " ")
            );
            run_workload_oneshot("comprehension", COMPREHENSION_SOURCE, comp_iters);
        }
        "comprehension-pooled" => {
            println!(
                "comprehension-pooled workload x{}: {}",
                comp_iters,
                COMPREHENSION_SOURCE.replace('\n', " ")
            );
            run_workload_pooled("comprehension-pooled", COMPREHENSION_SOURCE, comp_iters);
        }
        _ => {
            run_workload_oneshot("simple", SIMPLE_SOURCE, simple_iters);
            run_workload_pooled("simple-pooled", SIMPLE_SOURCE, simple_iters);
            run_workload_oneshot("comprehension", COMPREHENSION_SOURCE, comp_iters);
            run_workload_pooled("comprehension-pooled", COMPREHENSION_SOURCE, comp_iters);
        }
    }

    #[cfg(feature = "dhat-heap")]
    println!("done. dhat-heap.json will be written on profiler drop.");
}

/// One-shot mode: re-parse the AST and rebuild the `Context` + `Evaluator`
/// for every iteration. Mirrors what an embedder doing one-off evaluation
/// pays — boot cost is in scope, AST / context pooling is out of scope.
fn run_workload_oneshot(label: &str, source: &str, iterations: usize) {
    for _ in 0..iterations {
        let ast = parse_document(source).expect("parse failed");
        let ctx = {
            let mut ctx = Context::new().with_root(ast.clone());
            ctx.prepend_module_resolver(Arc::new(relon_evaluator::StdModuleResolver));
            Arc::new(ctx)
        };
        let eval = Evaluator::new(Arc::clone(&ctx));
        let _result = eval
            .eval(&ast, &Arc::new(Scope::default()))
            .expect("eval failed");
    }
    println!(
        "  workload `{}` (one-shot) completed: {} iterations",
        label, iterations
    );
}

/// Pooled mode: parse the AST once, build a single `Context` + `Evaluator`,
/// and drive `eval_root` repeatedly against a fresh root scope per call.
///
/// `eval_root` resets `step_counter`, `path_cache`, and `iter_cursors`
/// between invocations (see `eval.rs::Evaluator::eval_root`), so consecutive
/// runs are isolated except for the deliberately-retained `module_cache`.
/// This matches the "long-running host, repeated evaluation" pattern and
/// surfaces the pure per-eval cost with the one-time boot cost amortized.
fn run_workload_pooled(label: &str, source: &str, iterations: usize) {
    let ast = parse_document(source).expect("parse failed");
    let ctx = {
        let mut ctx = Context::new().with_root(ast.clone());
        ctx.prepend_module_resolver(Arc::new(relon_evaluator::StdModuleResolver));
        Arc::new(ctx)
    };
    let eval = Evaluator::new(Arc::clone(&ctx));
    for _ in 0..iterations {
        let scope = Arc::new(Scope::default());
        let _result = eval.eval_root(&scope).expect("eval_root failed");
    }
    println!(
        "  workload `{}` (pooled) completed: {} iterations",
        label, iterations
    );
}
