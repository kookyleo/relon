#![forbid(unsafe_code)]

//! Heap-allocation profiler for the Relon parser + evaluator hot paths.
//!
//! Build & run:
//!   `cargo run --release -p relon-bench --bin profile_alloc --features dhat-heap`
//!
//! Optional CLI argument selects which workload(s) to profile:
//!   - `simple`               — arithmetic workload, fresh `Context` per iteration (1000×)
//!   - `simple-pooled`        — arithmetic workload, single `Context` reused across 1000 evals
//!   - `comprehension`        — list-comprehension workload, fresh `Context` per iteration (100×)
//!   - `comprehension-pooled` — list-comprehension workload, single `Context` reused across 100 evals
//!   - `all` (default)        — run all four back-to-back
//!
//! The pooled modes exercise the "host boots once, evaluates many times"
//! pattern: the AST is parsed once and a single `Context` + `Evaluator` are
//! reused. `eval_root` is responsible for resetting `step_counter`,
//! `path_cache`, and `iter_cursors` between runs; `module_cache` is
//! deliberately retained (module loads are genuinely cross-run shareable).
//!
//! Each run drops a `dhat-heap.json` in the current working directory; load
//! it into the dhat web viewer (https://nnethercote.github.io/dh_view/dh_view.html)
//! to inspect alloc sites. When the feature is off the binary still
//! compiles (so the workspace stays buildable without the profiler), but
//! emits a hint instead of running.

#[cfg(feature = "dhat-heap")]
use std::sync::Arc;

#[cfg(feature = "dhat-heap")]
use relon_evaluator::{Context, Evaluator, Scope};
#[cfg(feature = "dhat-heap")]
use relon_parser::parse_document;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Iteration count for the "simple" workload: a constant-folded arithmetic
/// expression. The full parse + eval round-trip costs roughly tens of
/// microseconds; 1000 iterations are enough for dhat to register a
/// stable allocation profile without dominating the report size.
#[cfg(feature = "dhat-heap")]
const SIMPLE_ITERATIONS: usize = 1000;

/// Iteration count for the "comprehension" workload: a 1000-element list
/// comprehension plus a sibling reference. Each round-trip is in the
/// millisecond range, so 100 iterations is plenty for dhat to see every
/// hot alloc site at least an order of magnitude more than profiler noise.
#[cfg(feature = "dhat-heap")]
const COMPREHENSION_ITERATIONS: usize = 100;

#[cfg(feature = "dhat-heap")]
const SIMPLE_SOURCE: &str = "{ val: 1 + 2 * 3 / 4.0 }";

#[cfg(feature = "dhat-heap")]
const COMPREHENSION_SOURCE: &str = r#"{
    "list": [x * 2 for x in range(1000) if x % 2 == 0],
    "check": &sibling.list
}"#;

fn main() {
    #[cfg(not(feature = "dhat-heap"))]
    {
        println!(
            "profile_alloc: dhat feature is OFF. \
             Rebuild with `--features dhat-heap` to capture an allocation profile."
        );
    }

    #[cfg(feature = "dhat-heap")]
    {
        // Pick workload from argv[1]; default to "all".
        let arg = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());
        let selection = arg.as_str();

        // Profiler lives for the whole `main`; on drop it writes
        // `dhat-heap.json` into the current working directory.
        let _profiler = dhat::Profiler::new_heap();

        println!("--- Relon Heap Allocation Profile ---");
        println!("workload selection: {}", selection);

        match selection {
            "simple" => {
                println!("simple workload x{}: {}", SIMPLE_ITERATIONS, SIMPLE_SOURCE);
                run_workload_oneshot("simple", SIMPLE_SOURCE, SIMPLE_ITERATIONS);
            }
            "simple-pooled" => {
                println!(
                    "simple-pooled workload x{}: {}",
                    SIMPLE_ITERATIONS, SIMPLE_SOURCE
                );
                run_workload_pooled("simple-pooled", SIMPLE_SOURCE, SIMPLE_ITERATIONS);
            }
            "comprehension" => {
                println!(
                    "comprehension workload x{}: {}",
                    COMPREHENSION_ITERATIONS,
                    COMPREHENSION_SOURCE.replace('\n', " ")
                );
                run_workload_oneshot(
                    "comprehension",
                    COMPREHENSION_SOURCE,
                    COMPREHENSION_ITERATIONS,
                );
            }
            "comprehension-pooled" => {
                println!(
                    "comprehension-pooled workload x{}: {}",
                    COMPREHENSION_ITERATIONS,
                    COMPREHENSION_SOURCE.replace('\n', " ")
                );
                run_workload_pooled(
                    "comprehension-pooled",
                    COMPREHENSION_SOURCE,
                    COMPREHENSION_ITERATIONS,
                );
            }
            _ => {
                println!("simple workload x{}: {}", SIMPLE_ITERATIONS, SIMPLE_SOURCE);
                println!(
                    "comprehension workload x{}: {}",
                    COMPREHENSION_ITERATIONS,
                    COMPREHENSION_SOURCE.replace('\n', " ")
                );
                run_workload_oneshot("simple", SIMPLE_SOURCE, SIMPLE_ITERATIONS);
                run_workload_pooled("simple-pooled", SIMPLE_SOURCE, SIMPLE_ITERATIONS);
                run_workload_oneshot(
                    "comprehension",
                    COMPREHENSION_SOURCE,
                    COMPREHENSION_ITERATIONS,
                );
                run_workload_pooled(
                    "comprehension-pooled",
                    COMPREHENSION_SOURCE,
                    COMPREHENSION_ITERATIONS,
                );
            }
        }

        println!("done. dhat-heap.json will be written on profiler drop.");
    }
}

/// One-shot mode: re-parse the AST and rebuild the `Context` + `Evaluator`
/// for every iteration. Mirrors what an embedder doing one-off evaluation
/// pays — boot cost is in scope, AST / context pooling is out of scope.
#[cfg(feature = "dhat-heap")]
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
/// lets dhat surface the pure per-eval cost with the one-time boot cost
/// amortized.
#[cfg(feature = "dhat-heap")]
fn run_workload_pooled(label: &str, source: &str, iterations: usize) {
    let ast = parse_document(source).expect("parse failed");
    let ctx = {
        let mut ctx = Context::new().with_root(ast.clone());
        ctx.prepend_module_resolver(Arc::new(relon_evaluator::StdModuleResolver));
        Arc::new(ctx)
    };
    let eval = Evaluator::new(Arc::clone(&ctx));
    for _ in 0..iterations {
        // `eval_root` walks the root node attached via `with_root` above,
        // resetting the per-run state inside `Context` before the walk.
        let scope = Arc::new(Scope::default());
        let _result = eval.eval_root(&scope).expect("eval_root failed");
    }
    println!(
        "  workload `{}` (pooled) completed: {} iterations",
        label, iterations
    );
}
