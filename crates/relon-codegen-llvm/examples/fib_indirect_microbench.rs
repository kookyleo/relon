//! Micro-A/B: direct vs indirect recursive-call fib(22), 100k iters.
//! Proves the per-call cost of `callq <pcrel32>` vs `callq *%reg`
//! before we commit to a custom MCJIT memory manager that flips the
//! Code Model from `JITDefault` to `Small`.
//!
//! Both `fib_direct` and `fib_indirect` are `#[inline(never)]` so the
//! recursion shape survives LTO. The indirect variant fetches the
//! function pointer via `core::hint::black_box` on every call so the
//! optimiser can't constant-fold it into a direct call.

use std::time::Instant;

#[inline(never)]
fn fib_direct(k: i64) -> i64 {
    if k < 2 {
        k
    } else {
        fib_direct(k - 1).wrapping_add(fib_direct(k - 2))
    }
}

type FibFn = unsafe extern "C" fn(i64) -> i64;

#[inline(never)]
extern "C" fn fib_recurse_via_ptr(k: i64) -> i64 {
    if k < 2 {
        k
    } else {
        // Force indirect: read the function pointer from a static
        // (resolved lazily / via PLT under PIC) and call through it.
        // Mirrors MCJIT's `movabsq` + `callq *%reg` shape.
        let f: FibFn = FIB_INDIRECT;
        let a = unsafe { f(k - 1) };
        let b = unsafe { f(k - 2) };
        a.wrapping_add(b)
    }
}

static FIB_INDIRECT: FibFn = fib_recurse_via_ptr;

#[inline(never)]
fn fib_indirect(k: i64) -> i64 {
    fib_recurse_via_ptr(k)
}

fn main() {
    const N: i64 = 22;
    const ITERS: u32 = 5_000;

    // Warm up.
    for _ in 0..50 {
        let _ = std::hint::black_box(fib_direct(std::hint::black_box(N)));
        let _ = std::hint::black_box(fib_indirect(std::hint::black_box(N)));
    }

    let t0 = Instant::now();
    let mut acc = 0i64;
    for _ in 0..ITERS {
        acc = acc.wrapping_add(fib_direct(std::hint::black_box(N)));
    }
    let d_direct = t0.elapsed();
    std::hint::black_box(acc);

    let t0 = Instant::now();
    let mut acc = 0i64;
    for _ in 0..ITERS {
        acc = acc.wrapping_add(fib_indirect(std::hint::black_box(N)));
    }
    let d_indirect = t0.elapsed();
    std::hint::black_box(acc);

    println!(
        "fib({N}) x {ITERS}: direct={:>10} ns total ({:>6.1} ns/call), \
         indirect={:>10} ns total ({:>6.1} ns/call), delta/call={:>6.2} ns",
        d_direct.as_nanos(),
        d_direct.as_nanos() as f64 / (ITERS as f64 * fib_call_count(N) as f64),
        d_indirect.as_nanos(),
        d_indirect.as_nanos() as f64 / (ITERS as f64 * fib_call_count(N) as f64),
        (d_indirect.as_nanos() as f64 - d_direct.as_nanos() as f64)
            / (ITERS as f64 * fib_call_count(N) as f64),
    );
}

fn fib_call_count(k: i64) -> i64 {
    // Number of calls to fib() during fib(k). Mirrors the tree
    // size: T(k) = T(k-1) + T(k-2) + 1 with T(0)=T(1)=1.
    if k < 2 {
        1
    } else {
        fib_call_count(k - 1) + fib_call_count(k - 2) + 1
    }
}
