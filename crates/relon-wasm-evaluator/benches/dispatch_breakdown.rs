//! Phase Z.3a profile-first breakdown: ns/op for each layer of the
//! `WasmEvaluator` W12 `run_main` boundary, so the fast-path landing
//! has a measurable target (HashMap pack / typed-func resolve /
//! `TypedFunc::call` itself / `Value::Int` wrap).
//!
//! Run with:
//!     cargo bench -p relon-wasm-evaluator --bench dispatch_breakdown
//!
//! Two rows are measured (W12 `x + 1`, 1_000_000 iters each):
//!
//!     L2_fast_full            — `run_main_legacy_i64_fast`
//!                               (mutex lock + arena reset + cached
//!                               `TypedFunc<i64,i64>::call` + tier write)
//!     L4_run_main_buffer      — `Evaluator::run_main`
//!                               (L2 + `HashMap<String,Value>` get +
//!                               `extract_named_int` + `Value::Int` wrap)
//!
//! `L4 - L2` is the HashMap/Value boundary tax this Phase Z.3a fast
//! path bypasses. Reference numbers from the worktree dev host
//! (release profile, `lto=fat`, opt-level=3):
//!
//!     L2_fast_full            ~ 82.9 ns/iter
//!     L4_run_main_buffer      ~278.5 ns/iter
//!     delta (HashMap+Value)   ~195.6 ns/iter
//!
//! For the wasmtime side the remaining ~82 ns are: `Mutex::lock`
//! (~5 ns uncontended), `HostState::reset` (single u32 write),
//! `validate_sync_call` + `vm_func_ref` (~10-20 ns each), and the
//! `invoke_wasm_and_catch_traps` / array_call C++ shim that wraps the
//! actual JIT body — the bulk of the floor for any wasmtime call.
//! W12's wasm body itself is ~5 ns (`i64.const 1; i64.add`).
//!
//! The bench is W12-only because the inner kernel is ~5 ns so the
//! boundary overhead dominates each row's mean — that's the whole
//! point of profiling Z.3a.

use std::collections::HashMap;
use std::time::Instant;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::WasmEvaluator;

const W12_SRC: &str = "#main(Int x) -> Int\nx + 1";
const WARMUP_ITERS: u64 = 50_000;
const MEASURE_ITERS: u64 = 1_000_000;

fn bench<F: FnMut()>(label: &str, iters: u64, mut f: F) {
    // Warm caches first.
    for _ in 0..WARMUP_ITERS {
        f();
    }
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    let elapsed = start.elapsed();
    let per = elapsed.as_nanos() as f64 / iters as f64;
    println!("{label:38} {per:>8.1} ns/iter");
}

fn main() {
    println!("# Phase Z.3a dispatch breakdown (W12 = `x + 1`)");
    println!("# {MEASURE_ITERS} iters per row, warmup {WARMUP_ITERS}");
    println!();

    let ev = WasmEvaluator::new(W12_SRC).expect("WasmEvaluator::new(W12)");
    // Drive once so tier == Compiled before timing.
    let mut warm_args = HashMap::new();
    warm_args.insert("x".to_string(), Value::Int(0));
    let _ = ev.run_main(warm_args).unwrap();

    // ----- L2 fast path (production hot loop shape) -----
    bench("L2_fast_full", MEASURE_ITERS, || {
        let v = ev
            .run_main_legacy_i64_fast(&[std::hint::black_box(41i64)])
            .unwrap();
        std::hint::black_box(v);
    });

    // ----- L4 buffer-protocol Evaluator::run_main -----
    bench("L4_run_main_buffer", MEASURE_ITERS, || {
        let mut args = HashMap::new();
        args.insert("x".to_string(), Value::Int(std::hint::black_box(41)));
        let v = ev.run_main(args).unwrap();
        std::hint::black_box(v);
    });

    // L4 minus L2 ~= HashMap pack + extract_named_int + Value::Int
    // wrap on return. Reported by the math at the bottom.
}
