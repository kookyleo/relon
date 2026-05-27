//! Phase I perf smoke: wall-clock side-by-side of the LLVM AOT W3
//! row (`range(n).map((i) => "a").reduce("", (acc, s) => acc + s)`)
//! against a hand-written Rust `String::push_str` accumulator after
//! the in-place append fast path lands.
//!
//! Gated behind `--ignored` so CI does not measure wall-clock. Run
//! with:
//!
//! ```sh
//! LLVM_SYS_181_PREFIX=/usr/lib/llvm-18 \
//!   cargo test -p relon-codegen-llvm --test llvm_w3_perf \
//!   --release -- --ignored --nocapture
//! ```
//!
//! The headline number for each row is `ns/call`. The Phase E.1
//! baseline (inlined `concat` stdlib body, fresh alloc per iter)
//! measured ~60 µs/call at N=1000 and SEGV'd at N=2000 because the
//! O(N²) intermediate records overflowed the 1 MiB scratch arena.
//! Phase I closes that gap: at N=2000 the row tracks rust_native
//! within ≤ 2× — same shape as W1 / W2 / W4 post-Phase-F.1.

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

const W3_SRC: &str = "#unstrict\n\
                      #import list from \"std/list\"\n\
                      #main(Int n) -> String\n\
                      range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";

const ITERS: usize = 1_000;
const NATIVE_ITERS: usize = 10_000;

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

fn time_llvm_aot(ev: &LlvmAotEvaluator, n: i64) -> u128 {
    // Warm-up: prime the JIT's internal caches + arena pool.
    for _ in 0..16 {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let _ = ev.run_main(args).unwrap();
    }
    let t0 = Instant::now();
    for _ in 0..ITERS {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let v = ev.run_main(args).unwrap();
        black_box(v);
    }
    t0.elapsed().as_nanos() / ITERS as u128
}

fn time_rust_native(n: i64) -> u128 {
    let t0 = Instant::now();
    let mut acc: i64 = 0;
    for _ in 0..NATIVE_ITERS {
        acc = acc.wrapping_add(rust_native_w3(black_box(n)));
    }
    black_box(acc);
    t0.elapsed().as_nanos() / NATIVE_ITERS as u128
}

#[test]
#[ignore]
fn w3_llvm_perf_panel() {
    let ev = LlvmAotEvaluator::from_source(W3_SRC).expect("from_source");
    for n in [500_i64, 1000, 2000] {
        let llvm_ns = time_llvm_aot(&ev, n);
        let native_ns = time_rust_native(n);
        let ratio = llvm_ns as f64 / native_ns as f64;
        eprintln!(
            "W3 N={n}: llvm_aot={llvm_ns} ns/call  rust_native={native_ns} ns/call  ratio={ratio:.2}x"
        );
    }
}
