//! Phase 0b — LLVM-AOT control-family lowering (`Op::Select`,
//! `Op::BrTable`) differential coverage.
//!
//! Three-way alignment, cranelift is the gold standard:
//!
//! - **Raw-IR backends** (`Op::Select` / `Op::BrTable`): the same
//!   hand-built `Module` is fed through `from_ir_direct` to both
//!   cranelift and the LLVM JIT, and the two results are asserted
//!   byte-identical against each other and a hand-computed oracle.
//!   These ops are produced by the IR lowering / stdlib bodies
//!   (`min` / `max` -> `Select`, multi-way dispatch -> `BrTable`)
//!   rather than by surface syntax, so the raw-IR path is how the
//!   cranelift suite exercises them too (see
//!   `relon-codegen-cranelift/tests/{select_and_block,
//!   control_flow_extended}.rs`).
//! - **Tree-walk oracle** (`Select` only): the stdlib `min` / `max`
//!   surfaces lower to `Op::Select`, so a `from_source` program that
//!   calls them runs the full parse -> lower -> LLVM JIT path and is
//!   cross-checked against the tree-walking interpreter, closing the
//!   third leg for the `Select` semantics.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::{AotEvaluator, SandboxConfig};
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::{parse_document, TokenRange};

fn t(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

fn legacy_module(body: Vec<TaggedOp>, params: Vec<IrType>) -> IrModule {
    let func = Func {
        name: "run_main".to_string(),
        params,
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

/// Compile `ir` through both backends, run with the given i64 args,
/// assert the two agree, and return the shared value. cranelift is the
/// gold standard — any divergence fails here.
fn run_both(ir: &IrModule, params: &[&str], args: &[i64]) -> i64 {
    let cl = AotEvaluator::from_ir_direct(
        ir.clone(),
        SandboxConfig::default(),
        params.iter().map(|s| s.to_string()).collect(),
    )
    .expect("cranelift compile");
    let llvm = LlvmAotEvaluator::from_ir_direct(
        ir.clone(),
        params.iter().map(|s| s.to_string()).collect(),
    )
    .expect("llvm compile");

    let cl_v = cl.run_main_legacy_i64(args).expect("cranelift run");
    let llvm_v = llvm.run_main_legacy_i64(args).expect("llvm run");
    assert_eq!(
        cl_v, llvm_v,
        "cranelift / llvm divergence for args {args:?}: cranelift={cl_v}, llvm={llvm_v}"
    );
    cl_v
}

// ---------------------------------------------------------------------
// Op::Select
// ---------------------------------------------------------------------

/// `min(a, b)` via the stdlib body's exact wasm shape: push
/// `[a, b, a < b]`, then `Select` — returns `a` when `a < b`, else `b`.
fn min_select_body() -> Vec<TaggedOp> {
    vec![
        t(Op::LocalGet(0)),
        t(Op::LocalGet(1)),
        t(Op::LocalGet(0)),
        t(Op::LocalGet(1)),
        t(Op::Lt(IrType::I64)),
        t(Op::Select { ty: IrType::I64 }),
        t(Op::Return),
    ]
}

#[test]
fn select_min_three_way_consistent() {
    let ir = legacy_module(min_select_body(), vec![IrType::I64, IrType::I64]);
    // (x, y, expected min)
    let cases = [
        (3, 10, 3),
        (10, 3, 3),
        (-5, -10, -10),
        (7, 7, 7),
        (i64::MIN, 0, i64::MIN),
        (0, i64::MAX, 0),
    ];
    for (x, y, expected) in cases {
        let got = run_both(&ir, &["x", "y"], &[x, y]);
        assert_eq!(got, expected, "min({x}, {y})");
    }
}

#[test]
fn select_i32_discriminant_picks_arm() {
    // Push two i64 values, then an i32 condition produced by an i32
    // comparison, then Select. Exercises the i32-cond -> i1 narrowing
    // in the LLVM emitter against cranelift's `select`.
    //   cond = (x as-i32-const 1) == 1  -> always true here; pick a.
    let body = vec![
        t(Op::LocalGet(0)),     // a
        t(Op::LocalGet(1)),     // b
        t(Op::ConstI32(1)),     // cond lhs
        t(Op::ConstI32(1)),     // cond rhs
        t(Op::Eq(IrType::I32)), // cond: 1 == 1 -> true (i32 1)
        t(Op::Select { ty: IrType::I64 }),
        t(Op::Return),
    ];
    let ir = legacy_module(body, vec![IrType::I64, IrType::I64]);
    let got = run_both(&ir, &["a", "b"], &[111, 222]);
    assert_eq!(got, 111);
}

// ---------------------------------------------------------------------
// Op::Select via the full from_source pipeline + tree-walk oracle
// ---------------------------------------------------------------------

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

fn llvm_source_two_i64(src: &str, a: i64, b: i64) -> i64 {
    let ev = LlvmAotEvaluator::from_source(src).expect("LLVM from_source");
    let mut args = HashMap::new();
    args.insert("a".to_string(), Value::Int(a));
    args.insert("b".to_string(), Value::Int(b));
    match ev.run_main(args).expect("LLVM run_main") {
        Value::Int(n) => n,
        other => panic!("unexpected LLVM return {other:?}"),
    }
}

fn oracle_source_two_i64(src: &str, a: i64, b: i64) -> i64 {
    let (walker, scope) = build_tree_walker(src);
    let mut args = HashMap::new();
    args.insert("a".to_string(), Value::Int(a));
    args.insert("b".to_string(), Value::Int(b));
    match walker.run_main(&scope, args).expect("tree-walk run_main") {
        Value::Int(n) => n,
        other => panic!("unexpected tree-walk return {other:?}"),
    }
}

#[test]
fn select_min_max_from_source_matches_tree_walker() {
    // `min` / `max` lower to `Op::Select`; the full source pipeline
    // exercises the LLVM emitter and is cross-checked against the
    // tree-walking interpreter (the third leg for Select semantics).
    let min_src = "#main(Int a, Int b) -> Int\nmin(a, b)";
    let max_src = "#main(Int a, Int b) -> Int\nmax(a, b)";
    for (a, b) in [(3, 10), (10, 3), (-5, -10), (7, 7)] {
        let llvm_min = llvm_source_two_i64(min_src, a, b);
        let oracle_min = oracle_source_two_i64(min_src, a, b);
        assert_eq!(llvm_min, oracle_min, "min({a}, {b}) llvm vs tree-walk");

        let llvm_max = llvm_source_two_i64(max_src, a, b);
        let oracle_max = oracle_source_two_i64(max_src, a, b);
        assert_eq!(llvm_max, oracle_max, "max({a}, {b}) llvm vs tree-walk");
    }
}

// ---------------------------------------------------------------------
// Op::BrTable
// ---------------------------------------------------------------------

/// Three-nested-block multi-way dispatch, mirroring the cranelift
/// `control_flow_extended` BrTable suite. The discriminant `disc`
/// selects:
///   index 0 -> target #0 (depth 1) -> "case 0" arm returns 100
///   index 1 -> target #1 (depth 0) -> "case 1" arm returns 200
///   otherwise -> default (depth 2)              returns 999
fn br_table_dispatch_body(disc: i32) -> Vec<TaggedOp> {
    vec![
        t(Op::ConstI32(disc)),
        t(Op::Block {
            result_ty: None,
            body: vec![
                t(Op::Block {
                    result_ty: None,
                    body: vec![
                        t(Op::Block {
                            result_ty: None,
                            body: vec![t(Op::BrTable {
                                default: 2,
                                targets: vec![1, 0],
                            })],
                        }),
                        // case 1 arm
                        t(Op::ConstI64(200)),
                        t(Op::Return),
                    ],
                }),
                // case 0 arm
                t(Op::ConstI64(100)),
                t(Op::Return),
            ],
        }),
        // default arm
        t(Op::ConstI64(999)),
        t(Op::Return),
    ]
}

#[test]
fn br_table_three_way_consistent() {
    // (discriminant, expected result)
    let cases = [
        (0_i32, 100_i64), // target #0 -> case 0
        (1, 200),         // target #1 -> case 1
        (2, 999),         // out of range -> default
        (5, 999),         // out of range -> default
        (-1, 999),        // negative (huge unsigned) -> default
    ];
    for (disc, expected) in cases {
        let ir = legacy_module(br_table_dispatch_body(disc), vec![IrType::I64]);
        let got = run_both(&ir, &["x"], &[0]);
        assert_eq!(got, expected, "BrTable discriminant {disc}");
    }
}

/// BrTable into a `Loop` header (back-edge) target. A `Br`/`BrTable`
/// arm pointing at a `Loop` frame jumps to its header (continue);
/// arms pointing at a `Block` jump to its tail (exit). This shape
/// uses a let-counter so the back-edge actually advances and the
/// loop terminates via the block-exit arm, exercising the
/// `LabelKind::Loop -> header_bb` resolution in the LLVM emitter.
#[test]
fn br_table_loop_back_edge_three_way_consistent() {
    // Pseudo:
    //   let i = 0
    //   block {           ; depth (from BrTable) = 1  -> exit
    //     loop {          ; depth (from BrTable) = 0  -> continue
    //       i = i + 1
    //       ; discriminant = (i < 3) ? 0 : 1
    //       ;   i<3  -> target #0 = depth 0 -> loop header (continue)
    //       ;   else -> default  = depth 1 -> block tail   (exit)
    //       BrTable { default: 1, targets: [0] }  with index = (i<3 ? 0 : 1)
    //     }
    //   }
    //   return i
    let body = vec![
        t(Op::ConstI64(0)),
        t(Op::LetSet {
            idx: 0,
            ty: IrType::I64,
        }),
        t(Op::Block {
            result_ty: None,
            body: vec![t(Op::Loop {
                result_ty: None,
                body: vec![
                    // i = i + 1
                    t(Op::LetGet {
                        idx: 0,
                        ty: IrType::I64,
                    }),
                    t(Op::ConstI64(1)),
                    t(Op::Add(IrType::I64)),
                    t(Op::LetSet {
                        idx: 0,
                        ty: IrType::I64,
                    }),
                    // index = (i < 3) ? 0 : 1, as i32
                    //   compute (i < 3) -> i32 bool, then it is already
                    //   0/1 — but we want 0 to mean "continue". i<3 is
                    //   1 when we should continue, so the discriminant
                    //   for "continue" must be 0. Flip: index = (i >= 3)
                    //   -> 0 while looping? No: use index = (i < 3) is 1
                    //   while looping. We want target #0 while looping,
                    //   so index must be 0 while looping. Use (i >= 3).
                    t(Op::LetGet {
                        idx: 0,
                        ty: IrType::I64,
                    }),
                    t(Op::ConstI64(3)),
                    t(Op::Ge(IrType::I64)), // i >= 3 -> 1 when done, 0 while looping
                    // index now: 0 while looping -> target#0 (loop header)
                    //            1 when done     -> default? no, 1 -> target#1
                    // We only have target #0; index 1 is out of range ->
                    // default (depth 1 = block tail = exit).
                    t(Op::BrTable {
                        default: 1,
                        targets: vec![0],
                    }),
                ],
            })],
        }),
        t(Op::LetGet {
            idx: 0,
            ty: IrType::I64,
        }),
        t(Op::Return),
    ];
    let ir = legacy_module(body, vec![IrType::I64]);
    // Loop runs while i < 3: i goes 1 (idx0->cont), 2 (idx0->cont),
    // 3 (i>=3 -> idx1 -> out of range -> default -> exit). Final i = 3.
    let got = run_both(&ir, &["x"], &[0]);
    assert_eq!(got, 3, "BrTable loop back-edge final counter");
}
