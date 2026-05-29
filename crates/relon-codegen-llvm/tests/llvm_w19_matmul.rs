//! AOT-4 (W19 slice): end-to-end proof that the W19 matrix-multiply
//! kernel — two `where`-bound `List<List<Int>>` matrices `a` / `b`
//! built by nested `range(size).map((i) => range(size).map((j) =>
//! <entry>))`, a result matrix `c` whose every cell is `Σ_k a[i][k] *
//! b[k][j]` (random cross-row double-index), and a checksum reduce
//! over `c` — compiles and runs through the real LLVM-18 AOT pipeline
//! and agrees with the tree-walker oracle (`relon-evaluator`, the
//! SOURCE OF TRUTH).
//!
//! ## Why this is distinct from `llvm_nested_matmul_smoke`
//! That smoke covers the *reduce-only fused* shape
//! `range(n).map((i) => range(n).map((j) => <cell>)).reduce(...)`,
//! which the AOT-2 eliding peephole collapses into a doubly-nested
//! accumulator loop with **NO list ever allocated**. W19 cannot use
//! that path: the cells of `c` read `a[i][k]` and `b[k][j]` — a random
//! cross-row access into two *separately materialised* matrices. The
//! eliding peephole only ever sees the loop counter `i` of a single
//! pass; it cannot serve a value stored in a different row of a matrix
//! built earlier. So the matrices `a` / `b` MUST be genuinely
//! materialised in the arena (an outer `List<Int>` record whose i64
//! elements are i32 arena offsets of inner `List<Int>` rows) and the
//! `a[i][k]` / `b[k][j]` reads MUST be real double-indexed loads.
//!
//! AOT-4 (this slice) adds, in `relon-ir` lowering:
//!   * `emit_list_value_materialize`: a where-bound `range(a, b).map((p)
//!     => <inner>)` materialises an outer `List<Int>` record whose i-th
//!     i64 element is the materialised inner row's i32 arena handle
//!     (when `<inner>` is itself a `range().map(...)` row) or the
//!     `<inner>` scalar cell value (when `<inner>` is `Int`-valued) —
//!     so a `List<List<Int>>` (or `List<Int>`) lands in the arena;
//!   * a generalised N-D index `a[i][k]`: the outer `a[i]` loads the
//!     inner row's i64 handle, which is retagged `ListInt` (the i64 is
//!     truncated to the i32 handle) so the next `[k]` indexes it —
//!     inline payload addressing throughout (`payload = (base + 11) &
//!     -8`, element at `payload + idx*8`, `Op::LoadI64AtAbsolute`), NO
//!     `Op::ListGetByIntIdx`, NO bounds branch (every W19 index is
//!     provably within `range(size)`);
//!   * `try_lower_materialized_list_reduce`: `<list>.reduce(init, (acc,
//!     elem) => body)` over a where-bound `List<Int>` /
//!     `List<List<Int>>` handle — the W19 `#main` outer fold
//!     `c.reduce(...)` whose `row.reduce(...)` re-reduces each inner row
//!     handle.
//!
//! Runtime `n` (the matrix size) is supplied at call time, so the
//! optimiser cannot const-fold the result; every assertion is against
//! the tree-walker oracle, never a hand-written closed form.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator as _, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// The full W19 production shape (verbatim structure of
/// `crates/relon-bench/benches/cmp_lua.rs::w19_relon_src`, reference
/// only — the bench file is orchestrator-owned). `size` is the runtime
/// matrix size `n`; `a` / `b` are materialised `List<List<Int>>`; `c`
/// is the materialised result matrix; the `#main` body folds `c` to a
/// scalar checksum. The mod-100 cell generators defeat any closed-form
/// fold.
const W19_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     c.reduce(0, (row_acc, row) => row_acc + row.reduce(0, (cell_acc, cell) => cell_acc + cell))\n\
     where {\n\
       size: n,\n\
       a: range(size).map((i) => range(size).map((j) => (i * size + j) % 100)),\n\
       b: range(size).map((i) => range(size).map((j) => (i + j) % 100)),\n\
       c: range(size).map((i) => range(size).map((j) => range(size).reduce(0, (acc, k) => acc + a[i][k] * b[k][j])))\n\
     }";

/// The same matmul checksum (identical Σ_i Σ_j Σ_k a[i][k] * b[k][j]),
/// but the i / j / checksum folds use `range(...).reduce` directly
/// instead of materialising `c` and re-folding it. This isolates the
/// load-bearing capability — 2D materialise of `a` / `b` + cross-row
/// double-index — from the reduce-over-materialised-list path, so a
/// regression in either localises cleanly.
const W19_KERNEL_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     range(size).reduce(0, (oi, i) => oi + range(size).reduce(0, (oj, j) => \
       oj + range(size).reduce(0, (acc, k) => acc + a[i][k] * b[k][j])))\n\
     where {\n\
       size: n,\n\
       a: range(size).map((i) => range(size).map((j) => (i * size + j) % 100)),\n\
       b: range(size).map((i) => range(size).map((j) => (i + j) % 100))\n\
     }";

/// A narrow pin on the 2D materialise + double-index path in isolation:
/// build a single `List<List<Int>>` and read one cross-row cell
/// `m[i][j]` selected by a runtime `n` (so the optimiser cannot
/// const-fold). The where-bound matrix is materialised and the two
/// index steps run real inline loads.
const W19_DOUBLE_INDEX_SRC: &str = "#unstrict\n\
     #main(Int n) -> Int\n\
     m[n][n]\n\
     where {\n\
       size: 5,\n\
       m: range(size).map((i) => range(size).map((j) => i * 10 + j))\n\
     }";

fn build_tree_walker(src: &str) -> (TreeWalkEvaluator, Arc<Scope>) {
    let node = parse_document(src)
        .unwrap_or_else(|e| panic!("parse failed for source:\n{src}\nerror: {e:?}"));
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    (
        TreeWalkEvaluator::new(Arc::new(ctx)),
        Arc::new(Scope::default()),
    )
}

fn extract_int(v: Value) -> i64 {
    match v {
        Value::Int(i) => i,
        other => panic!("W19 return expected Int, got {other:?}"),
    }
}

/// Run `src` on the tree-walker oracle (`relon-evaluator`) with a single
/// runtime `Int n` argument — the SOURCE OF TRUTH the AOT must match.
fn oracle_n(src: &str, n: i64) -> i64 {
    let (walker, scope) = build_tree_walker(src);
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    extract_int(
        walker
            .run_main(&scope, args)
            .unwrap_or_else(|e| panic!("tree-walker run_main failed for n={n}: {e:?}")),
    )
}

/// Run `src` on the LLVM-18 AOT JIT with a single runtime `Int n`.
fn llvm_n(ev: &LlvmAotEvaluator, n: i64) -> i64 {
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    extract_int(
        ev.run_main(args)
            .unwrap_or_else(|e| panic!("LLVM run_main failed for n={n}: {e:?}")),
    )
}

#[test]
fn w19_matmul_double_index_matches_oracle() {
    let ev = LlvmAotEvaluator::from_source(W19_DOUBLE_INDEX_SRC)
        .expect("W19 2D materialise + double-index compiles via LLVM AOT");
    // `m` is a fixed 5x5; the runtime `n` picks the cell `m[n][n]` so the
    // value is not const-foldable. Every n in [0,5) is in-bounds.
    for n in [0i64, 1, 2, 3, 4] {
        let got = llvm_n(&ev, n);
        let want = oracle_n(W19_DOUBLE_INDEX_SRC, n);
        assert_eq!(
            got, want,
            "W19 double-index m[n][n] LLVM AOT mismatches tree-walker oracle for n={n}"
        );
    }
}

#[test]
fn w19_matmul_kernel_matches_oracle() {
    let ev = LlvmAotEvaluator::from_source(W19_KERNEL_SRC).expect(
        "W19 matmul kernel compiles via LLVM AOT \
         (2D materialise of a/b + cross-row double-index + nested range-reduce checksum)",
    );
    // n=0 empty matrices -> 0; n=1 a single cell; the larger sizes drive
    // the full Σ_i Σ_j Σ_k cross-row double-index. n=16 is the production
    // W19 size.
    for n in [0i64, 1, 2, 3, 8, 16] {
        let got = llvm_n(&ev, n);
        let want = oracle_n(W19_KERNEL_SRC, n);
        assert_eq!(
            got, want,
            "W19 matmul kernel LLVM AOT mismatches tree-walker oracle for n={n}"
        );
    }
}

#[test]
fn w19_matmul_production_shape_matches_oracle() {
    let ev = LlvmAotEvaluator::from_source(W19_SRC).expect(
        "W19 production matmul (materialised c + c.reduce(...) checksum) compiles via LLVM AOT",
    );
    // The full production shape: materialise a / b / c (all
    // List<List<Int>>) and fold c via the reduce-over-materialised-list
    // path. n=16 is the production size; the smaller sizes pin the
    // empty / singleton / small cases.
    for n in [0i64, 1, 2, 3, 8, 16] {
        let got = llvm_n(&ev, n);
        let want = oracle_n(W19_SRC, n);
        assert_eq!(
            got, want,
            "W19 production matmul LLVM AOT mismatches tree-walker oracle for n={n}"
        );
    }
}
