//! 2026-05-22 P0 fix: multi-thread `run_main` safety regression test.
//!
//! History: `AotEvaluator` previously declared
//! `unsafe impl Sync`, but every invoke re-pointed the shared
//! `Arc<SandboxState>`'s `arena_base` / `arena_len` / `tail_cursor` /
//! `scratch_*` `UnsafeCell<_>` fields. Two threads calling
//! `run_main` concurrently raced on those writes — a data race / UB
//! under the Rust memory model.
//!
//! The stage-5 fix switches to a per-call `Box<SandboxState>` derived
//! from the evaluator's immutable `SandboxShared` template; each
//! dispatch owns its own sandbox. This test spawns multiple threads
//! that dispatch against the same evaluator and asserts the results
//! never cross-contaminate — if the shared-state regression returns,
//! the racing arena pointer writes corrupt one thread's return
//! record from the other's dispatch.
//!
//! The buffer-protocol path is the more sensitive one (it writes six
//! arena / scratch fields per invoke), so both tests below take that
//! path. The legacy `run_main_legacy_i64` path only writes
//! `trap_code`, but benefits from the same per-call ownership.

use std::sync::Arc;
use std::thread;

use relon_codegen_cranelift::AotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// Trivial `#main(Int a, Int b) -> Int` returning `a * 31 + b`. Each
/// thread feeds a unique `(a, b)` pair and expects the matching
/// product back; if sandbox state is shared, racing arena pointer
/// writes garble the output or panic on a corrupted return record.
const PROGRAM: &str = "#main(Int a, Int b) -> Int\na * 31 + b";

#[test]
fn run_main_does_not_race_across_threads_on_shared_evaluator() {
    let eval = Arc::new(AotEvaluator::from_source(PROGRAM).expect("compile #main(a, b) -> Int"));

    // Four threads, each running 256 dispatches. Thread count exceeds
    // the typical hyper-thread multiplier so any data race surfaces
    // under high dispatch density. 256 dispatches dominates the
    // Box-alloc / drop cost (~10 us total per thread) so the racing
    // window lands on each dispatch's hot path.
    const THREADS: usize = 4;
    const ITERS_PER_THREAD: usize = 256;

    let mut handles = Vec::with_capacity(THREADS);
    for t in 0..THREADS {
        let eval = Arc::clone(&eval);
        handles.push(thread::spawn(move || {
            for i in 0..ITERS_PER_THREAD {
                // Each thread's (a, b) pair is unique:
                // a = t * 1000 + i, b = t * 100.
                let a = (t * 1000 + i) as i64;
                let b = (t * 100) as i64;

                let mut args = std::collections::HashMap::new();
                args.insert("a".to_string(), Value::Int(a));
                args.insert("b".to_string(), Value::Int(b));

                let result = eval.run_main(args).expect("run_main");
                match result {
                    Value::Int(v) => assert_eq!(
                        v,
                        a * 31 + b,
                        "thread {t} iter {i}: expected {} got {v} \
                         (cross-thread sandbox state corruption?)",
                        a * 31 + b
                    ),
                    other => panic!("expected Int, got {other:?}"),
                }
            }
        }));
    }

    for h in handles {
        h.join().expect("worker panicked");
    }
}

/// Same buffer-protocol path but with a single-arg `#main(Int x) -> Int`
/// to cover the small-arity edge. In historic regressions, the
/// `BufferBuilder` root_size for arity=1 is smaller, making
/// `scratch_base` math more sensitive, so the single-arg path tends
/// to trip a shared-state race sooner than the two-arg variant.
#[test]
fn run_main_single_arg_no_race_under_contention() {
    const PROGRAM: &str = "#main(Int x) -> Int\nx * 2 + 7";
    let eval = Arc::new(AotEvaluator::from_source(PROGRAM).expect("compile #main(x) -> Int"));

    const THREADS: usize = 4;
    const ITERS_PER_THREAD: usize = 256;

    let mut handles = Vec::with_capacity(THREADS);
    for t in 0..THREADS {
        let eval = Arc::clone(&eval);
        handles.push(thread::spawn(move || {
            for i in 0..ITERS_PER_THREAD {
                let x = (t * 10_000 + i) as i64;
                let mut args = std::collections::HashMap::new();
                args.insert("x".to_string(), Value::Int(x));
                let result = eval.run_main(args).expect("run_main");
                match result {
                    Value::Int(v) => assert_eq!(
                        v,
                        x * 2 + 7,
                        "thread {t} iter {i}: arg={x}, expected {} got {v}",
                        x * 2 + 7
                    ),
                    other => panic!("expected Int, got {other:?}"),
                }
            }
        }));
    }

    for h in handles {
        h.join().expect("worker panicked");
    }
}
