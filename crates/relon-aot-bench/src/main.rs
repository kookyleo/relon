//! relon LLVM-AOT native binary vs literal-equivalent hand-written Rust.
//!
//! Each workload: a `.relon` `#main` (AOT-compiled to a native .o,
//! linked in) paired with a byte-identical-algorithm Rust fn. We verify
//! equality first, then time both with taskset/warmup/reps (median).

use std::hint::black_box;
use std::time::Instant;

relon_rs_macro::include_relon!("src/wa.relon");
relon_rs_macro::include_relon!("src/wb.relon");
relon_rs_macro::include_relon!("src/wc.relon");
relon_rs_macro::include_relon!("src/wd.relon");
relon_rs_macro::include_relon!("src/wf.relon");
relon_rs_macro::include_relon!("src/wg.relon");
relon_rs_macro::include_relon!("src/we.relon");

// ---- hand-written Rust equivalents (literal algorithm match) ----

#[inline(never)]
fn rust_wa(n: i64) -> i64 {
    let mut acc: i64 = 0;
    let mut i: i64 = 0;
    while i < n {
        acc = acc.wrapping_add((i % 7).wrapping_mul(i % 13));
        i += 1;
    }
    acc
}

#[inline(never)]
fn rust_wb(n: i64) -> i64 {
    let mut acc: i64 = 7;
    let mut i: i64 = 0;
    while i < n {
        acc = acc
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407)
            .wrapping_add(i);
        i += 1;
    }
    acc
}

#[inline(never)]
fn rust_wc(n: i64) -> i64 {
    let mut acc: i64 = 0;
    let mut i: i64 = 0;
    while i < n {
        let m = i % 3;
        acc = if m == 0 {
            acc.wrapping_add(i.wrapping_mul(i))
        } else if m == 1 {
            acc.wrapping_add(i)
        } else {
            acc.wrapping_sub(i)
        };
        i += 1;
    }
    acc
}

#[inline(never)]
fn rust_wd(xs: &[i64]) -> i64 {
    let mut acc: i64 = 0;
    let mut idx = 0;
    while idx < xs.len() {
        acc = acc.wrapping_add(xs[idx]);
        idx += 1;
    }
    acc
}

#[inline(never)]
fn rust_wf(x: f64, n: i64) -> f64 {
    let mut acc = x;
    let mut i: i64 = 0;
    while i < n {
        acc += 1.5;
        i += 1;
    }
    acc
}

#[inline(never)]
fn rust_wg(n: i64) -> i64 {
    let mut acc: i64 = 0;
    let mut i: i64 = 0;
    while i < n {
        let x = (i % 11).wrapping_mul(i % 17).wrapping_add(i % 5);
        acc = acc.wrapping_add(x);
        i += 1;
    }
    acc
}

// ---- workload (e): inlinable vs opaque-boundary host fn ----

#[inline(always)]
fn mix_inlinable(x: i64) -> i64 {
    x.wrapping_mul(2654435761).wrapping_add(x >> 3)
}

// Same body, forced behind a non-inlinable boundary to model an FFI /
// open-world dispatch cost the closed-world inline avoids.
#[inline(never)]
fn mix_opaque(x: i64) -> i64 {
    x.wrapping_mul(2654435761).wrapping_add(x >> 3)
}

#[inline(never)]
fn rust_we_inline(n: i64) -> i64 {
    let mut acc: i64 = 0;
    let mut i: i64 = 0;
    while i < n {
        acc = acc.wrapping_add(mix_inlinable(i));
        i += 1;
    }
    acc
}

#[inline(never)]
fn rust_we_opaque(n: i64) -> i64 {
    let mut acc: i64 = 0;
    let mut i: i64 = 0;
    while i < n {
        // black_box on the fn pointer prevents devirtualization, so each
        // iteration pays a real, non-inlined call (open-world analogue).
        let f: fn(i64) -> i64 = black_box(mix_opaque);
        acc = acc.wrapping_add(f(black_box(i)));
        i += 1;
    }
    acc
}

// ---- timing harness ----

fn bench<F: FnMut() -> i64>(label: &str, reps: usize, inner: u64, mut f: F) -> (f64, i64) {
    // warmup
    let mut last = 0i64;
    for _ in 0..3 {
        last = black_box(f());
    }
    let mut samples: Vec<f64> = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        for _ in 0..inner {
            last = black_box(f());
        }
        let ns = t.elapsed().as_nanos() as f64 / inner as f64;
        samples.push(ns);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];
    let min = samples[0];
    let max = samples[samples.len() - 1];
    println!(
        "{label:<28} median={median:>12.2} ns  min={min:>12.2}  max={max:>12.2}  (result={last})"
    );
    (median, last)
}

fn bench_f<F: FnMut() -> f64>(label: &str, reps: usize, inner: u64, mut f: F) -> (f64, f64) {
    let mut last = 0f64;
    for _ in 0..3 {
        last = black_box(f());
    }
    let mut samples: Vec<f64> = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        for _ in 0..inner {
            last = black_box(f());
        }
        let ns = t.elapsed().as_nanos() as f64 / inner as f64;
        samples.push(ns);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];
    println!("{label:<28} median={median:>12.2} ns  (result={last})");
    (median, last)
}

fn main() {
    let state = relon_rs_shims::SandboxState::default();
    let reps = 21usize;

    // Per-workload n + inner-iteration count (so each timed sample runs
    // long enough to dominate timer noise but the kernel stays L1-hot).
    let n_int: i64 = 100_000;
    let inner_int: u64 = 200;
    let n_chain: i64 = 50_000;
    let inner_chain: u64 = 200;
    let list_len = 100_000usize;
    let xs: Vec<i64> = (0..list_len as i64)
        .map(|i| i.wrapping_mul(2654435761))
        .collect();
    let n_float: i64 = 100_000;

    println!("=== correctness check (relon AOT vs Rust) ===");
    macro_rules! ck {
        ($name:expr, $a:expr, $b:expr) => {{
            let a = $a;
            let b = $b;
            assert_eq!(a, b, "MISMATCH {}: relon={:?} rust={:?}", $name, a, b);
            println!("OK   {:<6} relon == rust  ({:?})", $name, a);
        }};
    }
    ck!("wa", wa::main(&state, n_int), rust_wa(n_int));
    ck!("wb", wb::main(&state, n_chain), rust_wb(n_chain));
    ck!("wc", wc::main(&state, n_int), rust_wc(n_int));
    ck!("wd", wd::main(&state, &xs), rust_wd(&xs));
    ck!("wg", wg::main(&state, n_int), rust_wg(n_int));
    ck!("we_inline", we::main(&state, n_int), rust_we_inline(n_int));
    ck!("we_opaque", we::main(&state, n_int), rust_we_opaque(n_int));
    {
        let a = wf::main(&state, 0.0, n_float);
        let b = rust_wf(0.0, n_float);
        assert!((a - b).abs() < 1e-6, "wf mismatch {a} {b}");
        println!("OK   wf     relon == rust  ({a})");
    }

    // black_box every loop-bound / input so the Rust side cannot
    // constant-fold the whole computation away (the relon .o is opaque
    // to LLVM, so this keeps both sides doing the real work).
    println!("\n=== timings (median of {reps} reps) ===");
    println!("-- (a) vectorizable reduce  n={n_int} --");
    bench("a_relon_aot", reps, inner_int, || {
        wa::main(&state, black_box(n_int))
    });
    bench("a_rust", reps, inner_int, || rust_wa(black_box(n_int)));

    println!("-- (b) scalar dependency chain  n={n_chain} --");
    bench("b_relon_aot", reps, inner_chain, || {
        wb::main(&state, black_box(n_chain))
    });
    bench("b_rust", reps, inner_chain, || rust_wb(black_box(n_chain)));

    println!("-- (c) branch-dense  n={n_int} --");
    bench("c_relon_aot", reps, inner_int, || {
        wc::main(&state, black_box(n_int))
    });
    bench("c_rust", reps, inner_int, || rust_wc(black_box(n_int)));

    println!("-- (d) memory-bound ListInt sum  len={list_len} --");
    bench("d_relon_aot", reps, inner_int, || {
        wd::main(&state, black_box(&xs))
    });
    bench("d_rust", reps, inner_int, || rust_wd(black_box(&xs)));

    println!("-- (f) float reduce  n={n_float} --");
    bench_f("f_relon_aot", reps, inner_int, || {
        wf::main(&state, black_box(0.0), black_box(n_float))
    });
    bench_f("f_rust", reps, inner_int, || {
        rust_wf(black_box(0.0), black_box(n_float))
    });

    println!("-- (g) map.reduce fused  n={n_int} --");
    bench("g_relon_aot", reps, inner_int, || {
        wg::main(&state, black_box(n_int))
    });
    bench("g_rust", reps, inner_int, || rust_wg(black_box(n_int)));

    println!("-- (e) host fn in hot loop  n={n_int} --");
    bench("e_relon_closed_inline", reps, inner_int, || {
        we::main(&state, black_box(n_int))
    });
    bench("e_rust_direct_inline", reps, inner_int, || {
        rust_we_inline(black_box(n_int))
    });
    bench("e_rust_opaque_boundary", reps, inner_int, || {
        rust_we_opaque(black_box(n_int))
    });
}
