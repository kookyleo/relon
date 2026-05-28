//! Phase A.4 — Side-by-side performance smoke for the
//! `#main(Int x) -> Int : x + 1` bootstrap fixture.
//!
//! Compares LLVM JIT against:
//! - Cranelift AOT (existing `relon-codegen-cranelift::AotEvaluator`,
//!   same hand-built IR fed through `from_ir_direct`).
//! - Hand-rolled native Rust closure (the floor / target the LLVM
//!   path is chasing).
//!
//! Tree-walker is intentionally omitted here: its `Evaluator::run_main`
//! drives parse / analyze, so a synthetic-IR fixture cannot reach it.
//! The Phase B `from_source` widening will let us run a four-way
//! comparison through the canonical relon facade.
//!
//! Not gated `#[bench]` because nightly is not on the workspace; we
//! ship a `#[test]` that does the timing in-process and prints a
//! summary line so a developer eyeballing the CI log can spot
//! regressions.

use std::collections::HashMap;
use std::time::Instant;

use relon_codegen_cranelift::{AotEvaluator, SandboxConfig};
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::TokenRange;

/// `#main(Int x) -> Int : x + 1`
fn build_x_plus_one_ir() -> IrModule {
    let body = vec![
        TaggedOp {
            op: Op::LocalGet(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::ConstI64(1),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Add(IrType::I64),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Return,
            range: TokenRange::default(),
        },
    ];
    let func = Func {
        name: "run_main".to_string(),
        params: vec![IrType::I64],
        ret: IrType::I64,
        body,
        range: TokenRange::default(),
    };
    IrModule {
        imports: vec![],
        funcs: vec![func],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

/// Time `iters` calls of `f(args)`. Returns the median-of-3
/// nanoseconds/call, rounded to nearest. Three rounds is enough for a
/// smoke comparison; the bench harness in `crates/relon-bench` is
/// where the proper variance-aware A/B lives.
fn time_loop_ns<F>(label: &str, iters: u64, mut f: F) -> f64
where
    F: FnMut(),
{
    let mut samples = [0f64; 3];
    for s in &mut samples {
        // Warm the path.
        for _ in 0..1024 {
            f();
        }
        let start = Instant::now();
        for _ in 0..iters {
            f();
        }
        let elapsed = start.elapsed();
        *s = elapsed.as_secs_f64() * 1.0e9 / (iters as f64);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[1];
    println!(
        "  [{label:>22}] {median:>9.2} ns/call  (min {:>9.2}, max {:>9.2})",
        samples[0], samples[2]
    );
    median
}

#[test]
#[ignore = "Phase A.4 perf smoke — gated behind --ignored so CI does not measure wall-clock"]
fn llvm_vs_cranelift_vs_native_x_plus_one() {
    const ITERS: u64 = 1_000_000;

    println!("\n=== Phase A.4 — `#main(Int x) -> Int : x + 1` × {ITERS} iters ===");

    // 1. Native Rust closure — the floor.
    //
    // We route input + output through `std::hint::black_box` so the
    // optimizer cannot fold the entire loop into a closed-form
    // arithmetic progression. Without it LLVM inlines `x + 1` into
    // the loop counter and the timer measures DCE rather than an
    // actual call.
    #[inline(never)]
    fn native_add_one(x: i64) -> i64 {
        std::hint::black_box(x).wrapping_add(1)
    }
    let mut acc: i64 = 0;
    let native_ns = time_loop_ns("native rust fn", ITERS, || {
        acc = native_add_one(acc);
    });
    std::hint::black_box(acc);

    // 2. Cranelift AOT.
    let ir_cl = build_x_plus_one_ir();
    let cl = AotEvaluator::from_ir_direct(ir_cl, SandboxConfig::default(), vec!["x".to_string()])
        .expect("cranelift build");
    let mut x: i64 = 0;
    let cranelift_ns = time_loop_ns("cranelift AOT", ITERS, || {
        x = cl.run_main_legacy_i64(&[x]).expect("cranelift call");
    });
    std::hint::black_box(x);

    // 3. LLVM AOT (MCJIT).
    let ir_llvm = build_x_plus_one_ir();
    let llvm =
        LlvmAotEvaluator::from_ir_direct(ir_llvm, vec!["x".to_string()]).expect("llvm build");
    let mut x: i64 = 0;
    let llvm_ns = time_loop_ns("llvm AOT (MCJIT)", ITERS, || {
        x = llvm.run_main_legacy_i64(&[x]).expect("llvm call");
    });
    std::hint::black_box(x);

    // 4. LLVM AOT through the trait-object Evaluator surface — same
    //    HashMap-keyed entry the relon facade's `new_evaluator` would
    //    hand back to a downstream host. Slower than the legacy fast
    //    path because of the per-call HashMap lookup.
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(0));
    let llvm_trait_ns = time_loop_ns("llvm AOT (trait)", ITERS / 10, || {
        let v = llvm.run_main(args.clone()).expect("llvm trait call");
        if let Value::Int(n) = v {
            args.insert("x".to_string(), Value::Int(n));
        }
    });

    println!(
        "\nSummary (lower = better, native = 1.00x):\n  \
         native rust closure  = {:>6.2}x ({:.2} ns)\n  \
         cranelift AOT        = {:>6.2}x ({:.2} ns)\n  \
         llvm AOT (MCJIT)     = {:>6.2}x ({:.2} ns)\n  \
         llvm AOT (trait)     = {:>6.2}x ({:.2} ns)",
        native_ns / native_ns,
        native_ns,
        cranelift_ns / native_ns,
        cranelift_ns,
        llvm_ns / native_ns,
        llvm_ns,
        llvm_trait_ns / native_ns,
        llvm_trait_ns,
    );

    // Sanity: every backend must compute `x + 1` byte-identical for
    // a fixed input. This is the byte-identical guarantee from the
    // Phase A task spec, restated here so the perf test would loudly
    // refuse a miscompile even if the timing numbers looked clean.
    assert_eq!(cl.run_main_legacy_i64(&[41]).unwrap(), 42);
    assert_eq!(llvm.run_main_legacy_i64(&[41]).unwrap(), 42);
    assert_eq!(native_add_one(41), 42);
}
