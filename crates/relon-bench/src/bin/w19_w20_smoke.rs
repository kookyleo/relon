//! Quick smoke check for W19 / W20 workload sources (panel
//! expansion 2026-05-28, Tier 3 numeric-kernel).
//!
//! The bench helpers `wN_relon_src()` / `wN_lua_src()` /
//! `wN_expected()` live in `crates/relon-bench/benches/cmp_lua.rs`
//! (the Criterion `[[bench]]` binary, not the library), so this
//! smoke binary inlines the source strings rather than reaching
//! across the binary-vs-binary boundary. The constants here are
//! KEPT IN SYNC with the bench file at the const sites — any drift
//! between the two surfaces a test mismatch at smoke time.
//!
//! Run via:
//!
//! ```sh
//! cargo run -p relon-bench --bin w19_w20_smoke
//! ```
//!
//! W19 returns Int (sum-fold of the 16x16 result matrix; exact
//! equality check). W20 returns Float (asymmetric weighted
//! checksum after 1000 Verlet steps; absolute-tolerance check
//! because Verlet integration accumulates ~1e-10 relative rounding
//! drift over ~1M fp ops, which the tree-walker's expression-
//! evaluation order may differ from `rustc`'s by ~1 ULP per
//! FMA-lane fusion).

use relon::AutoEvaluator;
use relon_eval_api::{Evaluator as _, Value};
use std::collections::HashMap;

const W19_N: i64 = 16;
const W20_N: i64 = 1_000;
/// W20 absolute tolerance — see `cmp_lua.rs::W20_FLOAT_TOL`.
const W20_FLOAT_TOL: f64 = 1.0e-6;

fn w19_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Int\n\
     c.reduce(0, (row_acc, row) => row_acc + row.reduce(0, (cell_acc, cell) => cell_acc + cell))\n\
     where {\n\
       size: n,\n\
       a: range(size).map((i) => range(size).map((j) => (i * size + j) % 100)),\n\
       b: range(size).map((i) => range(size).map((j) => (i + j) % 100)),\n\
       c: range(size).map((i) => range(size).map((j) => range(size).reduce(0, (acc, k) => acc + a[i][k] * b[k][j])))\n\
     }"
}

fn w20_src() -> &'static str {
    "#unstrict\n\
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
     }"
}

fn run_int(label: &str, src: &str, n: i64) -> i64 {
    let jit = AutoEvaluator::new(src)
        .unwrap_or_else(|e| panic!("{label}: setup failed:\n{src}\nerr: {e}"));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match jit
        .run_main(args)
        .unwrap_or_else(|e| panic!("{label}: run_main failed: {e}"))
    {
        Value::Int(v) => v,
        other => panic!("{label}: non-Int return: {other:?}"),
    }
}

fn run_float(label: &str, src: &str, n: i64) -> f64 {
    let jit = AutoEvaluator::new(src)
        .unwrap_or_else(|e| panic!("{label}: setup failed:\n{src}\nerr: {e}"));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match jit
        .run_main(args)
        .unwrap_or_else(|e| panic!("{label}: run_main failed: {e}"))
    {
        Value::Float(f) => f.into_inner(),
        other => panic!("{label}: non-Float return: {other:?}"),
    }
}

fn w19_expected() -> i64 {
    let size: i64 = W19_N;
    let mut total: i64 = 0;
    let mut i: i64 = 0;
    while i < size {
        let mut j: i64 = 0;
        while j < size {
            let mut s: i64 = 0;
            let mut k: i64 = 0;
            while k < size {
                let aik = (i.wrapping_mul(size).wrapping_add(k)) % 100;
                let bkj = (k.wrapping_add(j)) % 100;
                s = s.wrapping_add(aik.wrapping_mul(bkj));
                k += 1;
            }
            total = total.wrapping_add(s);
            j += 1;
        }
        i += 1;
    }
    total
}

fn w20_expected() -> f64 {
    let n: i64 = W20_N;
    let dt: f64 = 0.01;
    let soft: f64 = 0.1;
    let m: [f64; 4] = [1.0, 2.0, 0.5, 3.0];
    let mut s: [f64; 8] = [0.0, 1.0, 2.5, 4.0, 0.1, 0.0, 0.0, 0.2];
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

fn main() {
    let v19 = run_int("W19", w19_src(), W19_N);
    let e19 = w19_expected();
    println!(
        "W19_matrix_multiply: got={v19} expected={e19} ok={}",
        v19 == e19
    );
    assert_eq!(v19, e19, "W19 mismatch");

    let v20 = run_float("W20", w20_src(), W20_N);
    let e20 = w20_expected();
    let abs = (v20 - e20).abs();
    println!(
        "W20_n_body_softened: got={v20} expected={e20} abs_err={abs:.3e} tol={W20_FLOAT_TOL:.3e} \
         ok={}",
        abs < W20_FLOAT_TOL
    );
    assert!(
        abs < W20_FLOAT_TOL,
        "W20 mismatch: {v20} vs expected {e20} (abs_err {abs:.3e} >= tol {W20_FLOAT_TOL:.3e})"
    );

    println!("\nALL W19/W20 smoke checks passed.");
}
