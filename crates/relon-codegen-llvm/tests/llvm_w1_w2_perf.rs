//! Phase B perf smoke: side-by-side wall-clock comparison of LLVM AOT
//! vs the equivalent native Rust loop for the cmp_lua W1 / W2
//! production sources. Gated behind `--ignored` so CI does not pay
//! the measurement cost on every PR.
//!
//! Run with:
//! ```sh
//! LLVM_SYS_181_PREFIX=/usr/lib/llvm-18 \
//!   cargo test -p relon-codegen-llvm --test llvm_w1_w2_perf \
//!   --release -- --ignored --nocapture
//! ```
//!
//! The harness loops the JIT entry `ITERS` times and reports the
//! mean wall-clock cost per invocation. The numbers are noisy on a
//! shared machine — treat the LLVM-vs-native ratio as the headline.
//! The cranelift AOT row would go here too but Phase B does not pull
//! the cranelift backend in as a hard dep; the cmp_lua bench panel
//! is the canonical A/B/C source.

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

const W1_SRC: &str = "#import list from \"std/list\"\n\
                      #main(Int n) -> Int\n\
                      list.sum(range(n))";

const W2_SRC: &str = "#unstrict\n\
                      #import list from \"std/list\"\n\
                      #main(Int n) -> Int\n\
                      list.sum(range(n).map((i) => (i + 1) * (i + 2)))";

const N: i64 = 1_000;
const ITERS: usize = 10_000;

#[inline(never)]
fn native_w1(n: i64) -> i64 {
    let mut acc = 0i64;
    let mut i = 0i64;
    while i < n {
        acc = acc.wrapping_add(i);
        i += 1;
    }
    acc
}

#[inline(never)]
fn native_w2(n: i64) -> i64 {
    let mut acc = 0i64;
    let mut i = 0i64;
    while i < n {
        acc = acc.wrapping_add((i + 1).wrapping_mul(i + 2));
        i += 1;
    }
    acc
}

/// Helper: run `iters` invocations of `f` and return mean ns/call.
fn time_loop<F: FnMut()>(iters: usize, mut f: F) -> u128 {
    let t0 = Instant::now();
    for _ in 0..iters {
        f();
    }
    t0.elapsed().as_nanos() / iters as u128
}

#[test]
#[ignore]
fn w1_llvm_vs_native_perf() {
    let ev = LlvmAotEvaluator::from_source(W1_SRC).expect("from_source");
    eprintln!("--- W1 LLVM IR dump ---\n{}", ev.emit_ir_dump());

    for _ in 0..16 {
        let mut a = HashMap::new();
        a.insert("n".into(), Value::Int(N));
        let _ = black_box(ev.run_main(a).unwrap());
        let _ = black_box(native_w1(N));
    }

    let llvm_ns = time_loop(ITERS, || {
        let mut a = HashMap::new();
        a.insert("n".into(), Value::Int(black_box(N)));
        black_box(ev.run_main(a).unwrap());
    });

    let native_ns = time_loop(ITERS, || {
        black_box(native_w1(black_box(N)));
    });

    eprintln!(
        "[W1 N={N}] LLVM={llvm_ns} ns/call native={native_ns} ns/call ratio={:.2}x \
         (both backends closed-form to n*(n-1)/2; LLVM cost is dispatch boundary, not loop body)",
        llvm_ns as f64 / native_ns.max(1) as f64
    );
}

#[test]
#[ignore]
fn w2_llvm_vs_native_perf() {
    let ev = LlvmAotEvaluator::from_source(W2_SRC).expect("from_source");
    eprintln!("--- W2 LLVM IR dump ---\n{}", ev.emit_ir_dump());

    for _ in 0..16 {
        let mut a = HashMap::new();
        a.insert("n".into(), Value::Int(N));
        let _ = black_box(ev.run_main(a).unwrap());
        let _ = black_box(native_w2(N));
    }

    let llvm_ns = time_loop(ITERS, || {
        let mut a = HashMap::new();
        a.insert("n".into(), Value::Int(black_box(N)));
        black_box(ev.run_main(a).unwrap());
    });

    let native_ns = time_loop(ITERS, || {
        black_box(native_w2(black_box(N)));
    });

    eprintln!(
        "[W2 N={N}] LLVM={llvm_ns} ns/call native={native_ns} ns/call ratio={:.2}x \
         (both backends closed-form; LLVM cost is dispatch boundary, not loop body)",
        llvm_ns as f64 / native_ns.max(1) as f64
    );
}

// Bigger N to verify the loop body itself does optimise. With
// `N = 1_000_000` the closed-form fold still wins for both, but
// dispatch overhead is amortised across enough work to read a
// closer-to-steady ratio.
#[test]
#[ignore]
fn w1_llvm_vs_native_perf_big_n() {
    let ev = LlvmAotEvaluator::from_source(W1_SRC).expect("from_source");
    let big_n: i64 = 1_000_000;
    let iters = 1_000usize;

    for _ in 0..16 {
        let mut a = HashMap::new();
        a.insert("n".into(), Value::Int(big_n));
        let _ = black_box(ev.run_main(a).unwrap());
        let _ = black_box(native_w1(big_n));
    }

    let llvm_ns = time_loop(iters, || {
        let mut a = HashMap::new();
        a.insert("n".into(), Value::Int(black_box(big_n)));
        black_box(ev.run_main(a).unwrap());
    });

    let native_ns = time_loop(iters, || {
        black_box(native_w1(black_box(big_n)));
    });

    eprintln!(
        "[W1 BigN N={big_n}] LLVM={llvm_ns} ns/call native={native_ns} ns/call \
         ratio={:.2}x",
        llvm_ns as f64 / native_ns.max(1) as f64
    );
}
