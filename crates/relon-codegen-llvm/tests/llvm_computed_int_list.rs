//! AOT completeness: a COMPUTED `List<Int>` literal `[e0, e1, ..]`
//! (non-const, non-range-map Int expressions) materialised through the
//! LLVM AOT path — symmetric to the computed `List<Float>` literal that
//! #359 Part B added (W20, `llvm_w20_n_body.rs`).
//!
//! Before this lane the computed-list materialiser only supported the
//! Float element shape; a computed Int list literal was rejected at IR
//! lowering with `List(computed element of type I64 — only Float ...)`.
//! This lane lands the Int branch: the same `AllocScratchDyn` + i32
//! length header + payload-at-`(base+11)&-8` record, but with i64
//! element stores (`StoreI64AtAbsolute`, the op the const / range-map
//! `List<Int>` materialisers already use) instead of `StoreF64`.
//!
//! KERNEL SHAPE: the computed list is where-bound (`xs: [n, n+1, ..]`)
//! and passed to a where-bound closure that indexes / lengths it. The
//! closure-parameter shape is deliberate: the analyzer infers a bare
//! list literal as a positional `Tuple`, and indexing-then-adding the
//! literal directly (`xs[0] + xs[1]`) trips a tuple-`Add` static
//! mismatch in the inference pass; routing the list through a closure
//! param keeps the analyzer happy while the AOT lowering still sees the
//! where-bound `Expr::List` and materialises it. This isolates THIS
//! lane's change (the Int materialiser) from the unrelated analyzer
//! tuple-index inference and exercises the exact materialise + 1D index
//! / `_len` paths the work item asks for. (The AOT `list.sum(handle)`
//! peephole only matches a `range(..)`-rooted or filter-handle receiver,
//! not a closure-param variable, so the full-element-sum coverage is
//! spelled with explicit indexed adds instead of `list.sum`.)
//!
//! HONESTY: each AOT result is pinned EXACTLY (Int equality is exact;
//! the kernels return `Int`) to the `TreeWalkEvaluator` on the SAME
//! source — no algorithm substitution, no parity fudge. Edge n incl
//! `n = 0` and negative `n` are covered (`n % 3 + 7` carries the host's
//! truncated-toward-zero modulo for negative `n`, so the oracle and AOT
//! must agree on the sign).

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// Index-mix kernel: build a computed `List<Int>` `xs` in a where-block
/// and read three of its elements through a closure param. Each element
/// is a distinct Int expression over the argument `n` (add / mul /
/// modulo), so the list cannot be interned as a `ConstListInt` and must
/// materialise into a scratch arena record at runtime.
const INDEX_MIX_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     f(xs) where {\n\
       xs: [n, n + 1, n * 2, n % 3 + 7],\n\
       f(ys): ys[0] + ys[1] + ys[3]\n\
     }";

/// Full-sum kernel: read EVERY element of the computed list through the
/// closure param and add them, so a single mis-stored element (wrong
/// offset, wrong store op, dropped element) diverges from the oracle.
/// `_len(ys)` is mixed in to keep the materialised i32 length header
/// honest alongside the payload.
const SUM_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     total(xs) where {\n\
       xs: [n, n + 1, n * 2, n % 3 + 7],\n\
       total(ys): _len(ys) * 1000 + ys[0] + ys[1] + ys[2] + ys[3]\n\
     }";

/// A 6-element computed list whose elements reference an EARLIER
/// where-binding (`base`), so the per-element lowering must resolve a
/// captured let, not just the argument. Mixes index + `_len`.
const CAPTURE_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     pick(xs) where {\n\
       base: n * 10,\n\
       xs: [base, base + n, base - n, base * 2, n + 1, base % 7],\n\
       pick(ys): _len(ys) + ys[0] + ys[2] + ys[5]\n\
     }";

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        other => panic!("expected Int result, got {other:?}"),
    }
}

fn oracle(src: &str, n: i64) -> i64 {
    let node = parse_document(src).expect("parse oracle source");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope = Arc::new(Scope::default());
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    as_i64(&walker.run_main(&scope, args).expect("tree-walker run_main"))
}

fn aot(ev: &LlvmAotEvaluator, n: i64) -> i64 {
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    as_i64(&ev.run_main(args).expect("LLVM run_main"))
}

/// `ys[0] + ys[1] + ys[3]` over the computed list must equal the
/// tree-walker for every `n`, including `n = 0` and negative `n`.
#[test]
fn computed_int_list_index_mix_matches_oracle() {
    let ev = LlvmAotEvaluator::from_source(INDEX_MIX_SRC)
        .unwrap_or_else(|e| panic!("LLVM AOT from_source (index-mix) failed: {e:?}"));
    for &n in &[0i64, 1, 2, 3, 5, 10, 50, 100, 1000, -1, -2, -3, -7, -100] {
        let got = aot(&ev, n);
        let want = oracle(INDEX_MIX_SRC, n);
        assert_eq!(
            got, want,
            "computed List<Int> index-mix AOT diverged from tree-walker at n={n}: \
             aot={got} oracle={want}",
        );
    }
}

/// Reading every element + the length over the same computed list must
/// equal the oracle (full payload + header coverage).
#[test]
fn computed_int_list_sum_matches_oracle() {
    let ev = LlvmAotEvaluator::from_source(SUM_SRC)
        .unwrap_or_else(|e| panic!("LLVM AOT from_source (sum) failed: {e:?}"));
    for &n in &[0i64, 1, 2, 3, 5, 10, 50, 100, 1000, -1, -2, -3, -7, -100] {
        let got = aot(&ev, n);
        let want = oracle(SUM_SRC, n);
        assert_eq!(
            got, want,
            "computed List<Int> sum AOT diverged from tree-walker at n={n}: \
             aot={got} oracle={want}",
        );
    }
}

/// A 6-element computed list whose elements close over an earlier
/// where-binding, indexed + lengthed, must equal the oracle for every
/// `n`.
#[test]
fn computed_int_list_capture_matches_oracle() {
    let ev = LlvmAotEvaluator::from_source(CAPTURE_SRC)
        .unwrap_or_else(|e| panic!("LLVM AOT from_source (capture) failed: {e:?}"));
    for &n in &[0i64, 1, 2, 3, 7, 13, 100, -1, -2, -7, -50] {
        let got = aot(&ev, n);
        let want = oracle(CAPTURE_SRC, n);
        assert_eq!(
            got, want,
            "computed List<Int> capture AOT diverged from tree-walker at n={n}: \
             aot={got} oracle={want}",
        );
    }
}

/// Explicit n=0 spot-check with a hand-computed expected value so the
/// test fails loudly if both the AOT and the oracle silently agreed on a
/// wrong materialise layout. xs = [0, 1, 0, 0%3+7=7]; ys[0]+ys[1]+ys[3]
/// = 0 + 1 + 7 = 8.
#[test]
fn computed_int_list_index_mix_n_zero_hand_value() {
    let ev = LlvmAotEvaluator::from_source(INDEX_MIX_SRC)
        .unwrap_or_else(|e| panic!("LLVM AOT from_source (index-mix) failed: {e:?}"));
    let got = aot(&ev, 0);
    assert_eq!(got, 8, "n=0 index-mix expected 0 + 1 + 7 = 8, got {got}");
    assert_eq!(got, oracle(INDEX_MIX_SRC, 0));
}
