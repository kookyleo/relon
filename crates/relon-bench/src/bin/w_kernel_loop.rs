//! Kernel-vs-row separation harness for the W-series perf panel
//! (2026-06-10; see `docs/internal/perf-panel-w-series.md`).
//!
//! The criterion `cmp_lua` rows time `run_main(args_factory())` per
//! call — for the LLVM rows that includes the per-call host-boundary
//! marshalling (HashMap arg pack + buffer pack/unpack + arena
//! round-trip), and the criterion harness contributes its own cycles
//! to any whole-process `perf stat` profile. This binary drives ONE
//! workload × ONE path in a bare loop so `perf stat` / `perf record`
//! see (almost) nothing but the measured path:
//!
//! * `llvm`      — `LlvmAotEvaluator::run_main` with a fresh
//!   `HashMap<String, Value>` per call (row-equivalent scope:
//!   marshalling INCLUDED, criterion harness excluded).
//! * `llvm_fast` — `run_main_legacy_i64_fast` with the scalar arg
//!   hoisted out of the loop (kernel scope: marshalling EXCLUDED).
//!   Errors out for workloads whose return shape has no fast entry.
//! * `rust`      — the hand-written rust_native kernel (kernel
//!   scope; no marshalling exists on this path).
//!
//! Usage:
//!
//! ```sh
//! cargo run --release -p relon-bench --features llvm-aot \
//!     --bin w_kernel_loop -- <workload> <path> <iters>
//! # workload ∈ {w7, w12, w16, w17, w18, w19}
//! # path     ∈ {llvm, llvm_fast, rust}
//! ```
//!
//! Prints total wall time, per-call time and the (checked) result.
//!
//! The Relon sources and the rust kernels are KEPT IN SYNC with
//! `benches/cmp_lua.rs` (same convention as `w16_w17_w18_smoke.rs` —
//! the criterion bench target cannot be imported from a bin). Any
//! drift surfaces as a result-checksum mismatch at run time.

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

use relon_eval_api::{Evaluator as _, Value};

// ===== Workload scales (sync: cmp_lua.rs) ============================

const FIB_N: i64 = 22;
const W12_X: i64 = 7;
const W16_N: i64 = 1_000;
const W17_N: i64 = 100;
const W18_N: i64 = 10_000;
const W19_N: i64 = 16;

// ===== Relon production sources (sync: cmp_lua.rs) ===================

fn w7_src() -> &'static str {
    "#main(Int n) -> Dict\n\
     {\n\
       #internal\n\
       fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
       result: fib(n)\n\
     }"
}

fn w12_src() -> &'static str {
    "#main(Int x) -> Int\nx + 1"
}

fn w16_src() -> &'static str {
    "#unstrict\n\
     #import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     sum_qs(arr)\n\
     where {\n\
       arr: range(n).map((i) => (i * 1103515245 + 12345) % 2048),\n\
       sum_qs(xs): _len(xs) <= 1 ? (_len(xs) == 0 ? 0 : xs[0]) : (\n\
         sum_qs(_list_filter(xs, (x) => x < xs[0]))\n\
         + list.sum(_list_filter(xs, (x) => x == xs[0]))\n\
         + sum_qs(_list_filter(xs, (x) => x > xs[0]))\n\
       )\n\
     }"
}

fn w17_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Int\n\
     range(n).reduce(0, (acc, i) => acc + bs(0, n, (i * 31) % n))\n\
     where {\n\
       bs(lo, hi, t): hi - lo <= 1 ? lo : (\n\
         (lo + hi) / 2 <= t\n\
           ? bs((lo + hi) / 2, hi, t)\n\
           : bs(lo, (lo + hi) / 2, t)\n\
       )\n\
     }"
}

fn w18_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Int\n\
     _len(_list_filter(range(2, n), (k) => is_prime(k, 2)))\n\
     where {\n\
       is_prime(k, d): d * d > k ? true : (k % d == 0 ? false : is_prime(k, d + 1))\n\
     }"
}

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

// ===== rust_native kernels (sync: cmp_lua.rs) ========================

#[inline(never)]
fn rust_w7(n: i64) -> i64 {
    #[inline(never)]
    fn fib(k: i64) -> i64 {
        if k < 2 {
            k
        } else {
            fib(k - 1).wrapping_add(fib(k - 2))
        }
    }
    fib(black_box(n))
}

#[inline(never)]
fn rust_w12(x: i64) -> i64 {
    black_box(x).wrapping_add(1)
}

#[inline(never)]
fn rust_w16(n: i64) -> i64 {
    fn filter_to_vec(xs: &[i64], pred: impl Fn(i64) -> bool) -> Vec<i64> {
        let mut out: Vec<i64> = Vec::with_capacity(xs.len());
        for &v in xs {
            if pred(v) {
                out.push(v);
            }
        }
        out
    }
    fn sum_qs(xs: Vec<i64>) -> i64 {
        let len = xs.len();
        if len == 0 {
            return 0;
        }
        if len == 1 {
            return xs[0];
        }
        let p = xs[0];
        let lt = filter_to_vec(&xs, |v| v < p);
        let eq = filter_to_vec(&xs, |v| v == p);
        let gt = filter_to_vec(&xs, |v| v > p);
        let mut eq_sum: i64 = 0;
        for &v in &eq {
            eq_sum = eq_sum.wrapping_add(v);
        }
        sum_qs(lt).wrapping_add(eq_sum).wrapping_add(sum_qs(gt))
    }
    let n = black_box(n);
    let range: Vec<i64> = (0..n).collect();
    let arr: Vec<i64> = range
        .iter()
        .map(|&i| (i.wrapping_mul(1103515245).wrapping_add(12345)) % 2048)
        .collect();
    sum_qs(arr)
}

#[inline(never)]
fn rust_w17(n: i64) -> i64 {
    fn bs(lo: i64, hi: i64, t: i64) -> i64 {
        if hi - lo <= 1 {
            return lo;
        }
        let mid = (lo + hi) / 2;
        if mid <= t {
            bs(mid, hi, t)
        } else {
            bs(lo, mid, t)
        }
    }
    let n = black_box(n);
    let mut acc: i64 = 0;
    let mut i: i64 = 0;
    while i < n {
        acc = acc.wrapping_add(bs(0, n, (i.wrapping_mul(31)) % n));
        i += 1;
    }
    acc
}

#[inline(never)]
fn rust_w18(n: i64) -> i64 {
    fn is_prime(k: i64, d: i64) -> bool {
        if d.wrapping_mul(d) > k {
            return true;
        }
        if k % d == 0 {
            return false;
        }
        is_prime(k, d + 1)
    }
    let n = black_box(n);
    let mut count: i64 = 0;
    let mut k: i64 = 2;
    while k < n {
        if is_prime(k, 2) {
            count = count.wrapping_add(1);
        }
        k += 1;
    }
    count
}

#[inline(never)]
fn rust_w19(n: i64) -> i64 {
    let size = black_box(n);
    let usize_s = size as usize;
    let mut a: Vec<Vec<i64>> = Vec::with_capacity(usize_s);
    let mut b: Vec<Vec<i64>> = Vec::with_capacity(usize_s);
    for i in 0..size {
        let mut row_a: Vec<i64> = Vec::with_capacity(usize_s);
        let mut row_b: Vec<i64> = Vec::with_capacity(usize_s);
        for j in 0..size {
            row_a.push((i.wrapping_mul(size).wrapping_add(j)) % 100);
            row_b.push((i.wrapping_add(j)) % 100);
        }
        a.push(row_a);
        b.push(row_b);
    }
    #[allow(clippy::needless_range_loop)]
    {
        let mut total: i64 = 0;
        for i in 0..usize_s {
            for j in 0..usize_s {
                let mut s: i64 = 0;
                for k in 0..usize_s {
                    s = s.wrapping_add(a[i][k].wrapping_mul(b[k][j]));
                }
                total = total.wrapping_add(s);
            }
        }
        total
    }
}

// ===== Harness =======================================================

struct Workload {
    src: &'static str,
    /// `#main` parameter name ("n" for everything except W12's "x").
    arg_name: &'static str,
    arg: i64,
    rust: fn(i64) -> i64,
}

fn workload(name: &str) -> Workload {
    match name {
        "w7" => Workload {
            src: w7_src(),
            arg_name: "n",
            arg: FIB_N,
            rust: rust_w7,
        },
        "w12" => Workload {
            src: w12_src(),
            arg_name: "x",
            arg: W12_X,
            rust: rust_w12,
        },
        "w16" => Workload {
            src: w16_src(),
            arg_name: "n",
            arg: W16_N,
            rust: rust_w16,
        },
        "w17" => Workload {
            src: w17_src(),
            arg_name: "n",
            arg: W17_N,
            rust: rust_w17,
        },
        "w18" => Workload {
            src: w18_src(),
            arg_name: "n",
            arg: W18_N,
            rust: rust_w18,
        },
        "w19" => Workload {
            src: w19_src(),
            arg_name: "n",
            arg: W19_N,
            rust: rust_w19,
        },
        other => {
            eprintln!("unknown workload `{other}` (expected w7|w12|w16|w17|w18|w19)");
            std::process::exit(2);
        }
    }
}

/// Unwrap a `run_main` result to the scalar the rust kernel returns.
/// W7 returns a single-Int-field anon Dict (`{ result: Int }`); all
/// the other panel workloads return a bare Int.
fn scalar_of(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        Value::Dict(d) if d.map.len() == 1 => match d.map.values().next() {
            Some(Value::Int(n)) => *n,
            other => panic!("single-field dict has non-Int value {other:?}"),
        },
        other => panic!("non-Int return: {other:?}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: w_kernel_loop <w7|w12|w16|w17|w18|w19> <llvm|llvm_fast|rust> <iters>");
        std::process::exit(2);
    }
    let wl = workload(&args[1]);
    let path = args[2].as_str();
    let iters: u64 = args[3].parse().expect("iters must be a u64");

    let expected = (wl.rust)(wl.arg);
    let mut checksum: i64 = 0;

    let elapsed = match path {
        "rust" => {
            // Warmup pass (page-in + branch-predictor settle).
            checksum = checksum.wrapping_add(black_box((wl.rust)(black_box(wl.arg))));
            let start = Instant::now();
            for _ in 0..iters {
                checksum = checksum.wrapping_add(black_box((wl.rust)(black_box(wl.arg))));
            }
            start.elapsed()
        }
        "llvm" | "llvm_fast" => {
            let ev = relon_codegen_llvm::LlvmAotEvaluator::from_source(wl.src)
                .unwrap_or_else(|e| panic!("LLVM AOT setup failed for {}: {e}", args[1]));
            let make_args = || -> HashMap<String, Value> {
                let mut m = HashMap::with_capacity(1);
                m.insert(wl.arg_name.to_string(), Value::Int(wl.arg));
                m
            };
            // Consistency gate: compiled output must match the rust
            // kernel before any timing is booked.
            let got = scalar_of(&ev.run_main(make_args()).expect("consistency run failed"));
            assert_eq!(got, expected, "{}: llvm/rust result mismatch", args[1]);
            if path == "llvm" {
                // Row-equivalent scope: fresh HashMap per call, buffer
                // protocol marshalling included (matches the criterion
                // `relon_llvm_aot` row's timed closure).
                checksum = checksum.wrapping_add(got);
                let start = Instant::now();
                for _ in 0..iters {
                    let v = ev.run_main(make_args()).unwrap();
                    checksum = checksum.wrapping_add(black_box(scalar_of(&v)));
                }
                start.elapsed()
            } else {
                // Kernel scope: legacy-i64 fast entry, no marshalling.
                if !ev.has_fast_path() {
                    eprintln!(
                        "{}: no legacy-i64 fast entry for this return shape; \
                         use `llvm` (buffer path) instead",
                        args[1]
                    );
                    std::process::exit(3);
                }
                let fast = ev
                    .run_main_legacy_i64_fast(&[wl.arg])
                    .expect("fast consistency run failed");
                assert_eq!(fast, expected, "{}: fast/rust result mismatch", args[1]);
                let a = black_box(wl.arg);
                checksum = checksum.wrapping_add(fast);
                let start = Instant::now();
                for _ in 0..iters {
                    let v = ev.run_main_legacy_i64_fast(&[a]).unwrap();
                    checksum = checksum.wrapping_add(black_box(v));
                }
                start.elapsed()
            }
        }
        other => {
            eprintln!("unknown path `{other}` (expected llvm|llvm_fast|rust)");
            std::process::exit(2);
        }
    };

    let per_call_ns = elapsed.as_nanos() as f64 / iters as f64;
    println!(
        "{} {} iters={} total={:?} per_call={:.2}ns expected={} checksum_ok={}",
        args[1],
        path,
        iters,
        elapsed,
        per_call_ns,
        expected,
        // checksum = (iters + 1) * expected when every call agreed.
        checksum == expected.wrapping_mul(iters.wrapping_add(1) as i64)
    );
}
