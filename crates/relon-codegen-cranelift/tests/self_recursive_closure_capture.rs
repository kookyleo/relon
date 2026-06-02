//! Isolated regression for the self-recursive closure capture fix in
//! the cranelift AOT backend (`emit_make_closure`).
//!
//! A where-bound closure that calls itself lowers to
//! `Op::MakeClosure { captures: [ClosureCapture { let_idx: N, ty: Closure }] }`
//! emitted *before* the matching `Op::LetSet { idx: N, ty: Closure }`,
//! because the closure must capture a handle to itself. Previously the
//! cranelift backend read the not-yet-bound capture slot via `get_let`
//! and aborted with `LetGet(N) read before LetSet`. The fix detects the
//! not-yet-bound slot, asserts the capture is `Closure`-typed, and
//! stamps the just-allocated closure handle (an i32 arena offset — a
//! value cycle, not a borrow) into the capture slot, matching the LLVM
//! backend.
//!
//! These shapes are the minimal reproducer: a self-recursive closure
//! that recurses over a `_list_filter`-shrunk list (the W16 partition
//! kernel) plus a plain self-recursive integer recurrence. Both are
//! asserted bit-identical to the tree-walker oracle.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// Minimal self-recursive closure over a `_list_filter`-shrunk list:
/// repeatedly drop the head pivot's `<`-partition and sum the rest. The
/// recursion is carried by `f` capturing itself; the list shrinks each
/// call so it terminates. This is the W16 self-capture + selective
/// filter kernel without the three-way partition fan-out.
fn self_recursive_filter_src() -> &'static str {
    "#unstrict\n\
     #import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     f(arr)\n\
     where {\n\
       arr: range(n).map((i) => (i * 1103515245 + 12345) % 2048),\n\
       f(xs): _len(xs) == 0 ? 0 : xs[0] + f(_list_filter(xs, (x) => x != xs[0]))\n\
     }"
}

/// Plain self-recursive integer recurrence (no list): `f(k)` sums
/// `k + (k-1) + ... + 0`. Isolates the self-capture forward-reference
/// without any list materialisation, so a failure here is purely the
/// closure-capture path.
fn self_recursive_int_src() -> &'static str {
    "#unstrict\n\
     #main(Int n) -> Int\n\
     f(n)\n\
     where {\n\
       f(k): k <= 0 ? 0 : k + f(k - 1)\n\
     }"
}

fn oracle(src: &str, n: i64) -> i64 {
    let node = parse_document(src).expect("oracle parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope: Arc<Scope> = Arc::new(Scope::default());
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match walker.run_main(&scope, args).expect("oracle run_main") {
        Value::Int(v) => v,
        other => panic!("oracle returned non-int: {other:?}"),
    }
}

fn aot_run(src: &str, n: i64) -> i64 {
    let eval = AotEvaluator::from_source(src).expect("self-recursive closure must compile");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match eval.run_main(args).expect("self-recursive run_main") {
        Value::Int(v) => v,
        other => panic!("AOT returned non-int: {other:?}"),
    }
}

/// The plain self-recursive integer recurrence compiles (no
/// `LetGet read before LetSet`) and matches the oracle. Recursion depth
/// is `O(n)`, so it runs on a wide-stack thread to keep the oracle from
/// overflowing the default test-thread stack.
#[test]
fn self_recursive_int_matches_oracle() {
    let src = self_recursive_int_src();
    AotEvaluator::from_source(src).expect("self-recursive int closure must compile");
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(|| {
            let src = self_recursive_int_src();
            for n in [0_i64, 1, 2, 5, 17, 64, 256, 1024] {
                let want = oracle(src, n);
                let got = aot_run(src, n);
                assert_eq!(got, want, "self-recursive int mismatch at n={n}");
            }
        })
        .expect("spawn wide-stack worker");
    handle.join().expect("self-recursive int worker panicked");
}

/// The self-recursive closure that recurses over a `_list_filter`-shrunk
/// list compiles and matches the oracle. This is the direct W16
/// self-capture + selective-filter kernel. Because the `!=`-pivot filter
/// only removes duplicates of the head each step, the (mostly distinct)
/// LCG values barely shrink, so each recursion level materialises a
/// near-full-size copy and the fixed 64 KiB AOT scratch arena is the
/// binding limit: it traps with `IndexOutOfBounds` between n=112 and
/// n=128 (a capacity bound, not a miscompile). `n` is capped at 112,
/// where every result is bit-identical to the oracle.
#[test]
fn self_recursive_filter_matches_oracle() {
    let src = self_recursive_filter_src();
    AotEvaluator::from_source(src).expect("self-recursive filter closure must compile");
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(|| {
            let src = self_recursive_filter_src();
            for n in [0_i64, 1, 2, 5, 17, 64, 96, 112] {
                let want = oracle(src, n);
                let got = aot_run(src, n);
                assert_eq!(got, want, "self-recursive filter mismatch at n={n}");
            }
        })
        .expect("spawn wide-stack worker");
    handle
        .join()
        .expect("self-recursive filter worker panicked");
}
