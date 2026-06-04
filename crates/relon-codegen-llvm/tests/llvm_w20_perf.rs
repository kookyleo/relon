//! W20 perf smoke: wall-clock side-by-side of the LLVM AOT W20 row
//! (softened 4-body 1D Verlet integration, `List<Float>` reduce
//! accumulator) against the hand-written Rust kernel. Used to measure
//! the #359 W20 container-perf work (scalar where-bound constant
//! inlining) on the s90 bench host.
//!
//! Gated behind `--ignored` so CI does not measure wall-clock. Run on
//! s90 with:
//!
//! ```sh
//! taskset -c 2 cargo test -p relon-codegen-llvm --test llvm_w20_perf \
//!   --release -- --ignored --nocapture
//! ```
//!
//! Headline number is `ns/call`; `ratio` is `llvm_aot / rust_native`.

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

const W20_SRC: &str = "#unstrict\n\
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
     }";

const ITERS: usize = 2_000;

#[inline(never)]
fn rust_native_w20(n: i64) -> f64 {
    let dt: f64 = 0.01;
    let soft: f64 = 0.1;
    let m: [f64; 4] = [1.0, 2.0, 0.5, 3.0];
    let mut s: [f64; 8] = [0.0, 1.0, 2.5, 4.0, 0.1, 0.0, 0.0, 0.2];
    let n = black_box(n);
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

fn time_llvm_aot(ev: &LlvmAotEvaluator, n: i64) -> u128 {
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
    let mut acc: f64 = 0.0;
    for _ in 0..ITERS {
        acc += rust_native_w20(black_box(n));
    }
    black_box(acc);
    t0.elapsed().as_nanos() / ITERS as u128
}

#[test]
#[ignore]
fn w20_llvm_perf_panel() {
    let ev = LlvmAotEvaluator::from_source(W20_SRC).expect("from_source");
    for n in [500_i64, 1000, 2000] {
        let llvm_ns = time_llvm_aot(&ev, n);
        let native_ns = time_rust_native(n);
        let ratio = llvm_ns as f64 / native_ns as f64;
        eprintln!(
            "W20 N={n}: llvm_aot={llvm_ns} ns/call  rust_native={native_ns} ns/call  ratio={ratio:.2}x"
        );
    }
}
