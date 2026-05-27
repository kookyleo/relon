//! Phase F.1 perf smoke: wall-clock side-by-side of the LLVM AOT
//! W4 (short haystack) and W4_long (256-byte haystack with tail
//! needle) workloads after the `relon_llvm_str_contains_arena` extern
//! interception lands.
//!
//! Gated behind `--ignored` so CI does not measure wall-clock. Run
//! with:
//! ```sh
//! LLVM_SYS_181_PREFIX=/usr/lib/llvm-18 \
//!   cargo test -p relon-codegen-llvm --test llvm_w4_perf \
//!   --release -- --ignored --nocapture
//! ```
//!
//! The headline number for each row is `ns/call`. The Phase E.1
//! baseline (naive inlined `contains_string_body`) measured ~49 µs /
//! call for W4 (N=1000) and ~3.7 ms / call for W4_long — both wildly
//! off LuaJIT's ~14.5 µs floor. Phase F.1 should drop both rows close
//! to LuaJIT's mark by routing the byte scan through `core::str::contains`
//! (SIMD `memchr` for the single-byte 'x' needle).

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

const W4_SRC: &str = "#import list from \"std/list\"\n\
                      #main(Int n) -> Int\n\
                      range(n).map((i) => \"axb\").filter((s) => s.contains(\"x\")).len()";

const W4_LONG_SRC: &str = concat!(
    "#import list from \"std/list\"\n",
    "#main(Int n) -> Int\n",
    "range(n)\n",
    "  .map((i) => \"",
    "loremipsumdolorsitametconsecteturadipiscingelitseddoeiusmodtemporincididuntutlaboreetdoloremagnaaliquautenimadminimveniamquisnostrudezercitationullamcolaborisnisiutaliquipezeacommodoconsequatduisauteiruredolorinreprehenderitinvoluptatevelitessecillumaaaaax",
    "\")\n",
    "  .filter((s) => s.contains(\"x\"))\n",
    "  .len()",
);

const N: i64 = 1_000;
const ITERS: usize = 200;

fn time_loop<F: FnMut()>(iters: usize, mut f: F) -> u128 {
    let t0 = Instant::now();
    for _ in 0..iters {
        f();
    }
    t0.elapsed().as_nanos() / iters as u128
}

#[test]
#[ignore]
fn w4_llvm_perf() {
    let ev = LlvmAotEvaluator::from_source(W4_SRC).expect("from_source");
    // Warm-up: prime the JIT's internal caches.
    for _ in 0..16 {
        let mut a = HashMap::new();
        a.insert("n".into(), Value::Int(N));
        let _ = black_box(ev.run_main(a).unwrap());
    }
    let ns = time_loop(ITERS, || {
        let mut a = HashMap::new();
        a.insert("n".into(), Value::Int(black_box(N)));
        black_box(ev.run_main(a).unwrap());
    });
    eprintln!(
        "[W4 N={N}] LLVM AOT={ns} ns/call (~{:.2} us); LuaJIT baseline ~14.5 us",
        ns as f64 / 1000.0
    );
}

#[test]
#[ignore]
fn w4_long_llvm_perf() {
    let ev = LlvmAotEvaluator::from_source(W4_LONG_SRC).expect("from_source");
    for _ in 0..16 {
        let mut a = HashMap::new();
        a.insert("n".into(), Value::Int(N));
        let _ = black_box(ev.run_main(a).unwrap());
    }
    let ns = time_loop(ITERS, || {
        let mut a = HashMap::new();
        a.insert("n".into(), Value::Int(black_box(N)));
        black_box(ev.run_main(a).unwrap());
    });
    eprintln!(
        "[W4_long N={N}] LLVM AOT={ns} ns/call (~{:.2} us); LuaJIT baseline ~14.55 us \
         (256-byte haystack with tail 'x')",
        ns as f64 / 1000.0
    );
}
