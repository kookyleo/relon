//! v5-β-2 stage 4 closeout bench: pit the cranelift-native AOT
//! backend against the tree-walking interpreter on the narrow
//! arithmetic envelope both can express today.
//!
//! Each scenario probes two points along the cost curve:
//!
//! * `cranelift_cold` — `CraneliftAotEvaluator::from_ir_direct` from
//!   synthetic IR. Cranelift JIT compile + finalize. The cost we'd
//!   pay on first call before any caching.
//! * `cranelift_warm` — preassembled cranelift evaluator, time only
//!   `run_main(args)`. The single-call latency target (LuaJIT trace
//!   tier territory, < 1 μs).
//! * `tree_walk_total` — tree-walker baseline. Every iter rebuilds
//!   the `Context` + `TreeWalkEvaluator` and runs `run_main`. The
//!   tree-walker doesn't have a separable "compile" phase the way
//!   the AOT backend does; this number is the apples-to-apples
//!   competitor for `cranelift_cold` on the assumption a host
//!   re-evaluates a fresh source per call (the typical
//!   configuration-reload workload).
//!
//! Scope: the cranelift backend's natural entry shape for arithmetic
//! benches is synthetic IR (no analyzer / lowering overhead). The
//! tree-walker side runs the equivalent source so the answers
//! match.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use relon_codegen_native::{CraneliftAotEvaluator, SandboxConfig};
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::{parse_document, TokenRange};

fn synth_add_ir() -> IrModule {
    let body = vec![
        TaggedOp {
            op: Op::LocalGet(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::LocalGet(1),
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
    IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body,
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

fn tree_walk_src() -> &'static str {
    "#main(Int x, Int y) -> Int\nx + y"
}

fn args_with_arg(x: i64, y: i64) -> HashMap<String, Value> {
    // Cranelift backend uses synthetic param names when constructed
    // from raw IR.
    let mut m = HashMap::with_capacity(2);
    m.insert("arg0".to_string(), Value::Int(x));
    m.insert("arg1".to_string(), Value::Int(y));
    m
}

fn args_with(x: i64, y: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(2);
    m.insert("x".to_string(), Value::Int(x));
    m.insert("y".to_string(), Value::Int(y));
    m
}

fn build_tree_walker(src: &str) -> TreeWalkEvaluator {
    let node = parse_document(src).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    TreeWalkEvaluator::new(Arc::new(ctx))
}

fn bench_arithmetic(c: &mut Criterion) {
    let mut group = c.benchmark_group("v5b2_stage4_arithmetic");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(5));

    // Cranelift cold start: build IR + JIT compile + finalize.
    group.bench_function(BenchmarkId::new("cranelift", "cold"), |b| {
        b.iter(|| {
            let ir = synth_add_ir();
            let aot = CraneliftAotEvaluator::from_ir_direct(
                ir,
                SandboxConfig::default(),
                vec!["arg0".to_string(), "arg1".to_string()],
            )
            .expect("cranelift compile");
            black_box(aot);
        });
    });

    // Cranelift warm invoke: reuse one preassembled evaluator across
    // every iter — the target "trace tier" probe.
    let cranelift = Arc::new(
        CraneliftAotEvaluator::from_ir_direct(
            synth_add_ir(),
            SandboxConfig::default(),
            vec!["arg0".to_string(), "arg1".to_string()],
        )
        .expect("cranelift preassemble"),
    );
    group.bench_function(BenchmarkId::new("cranelift", "warm"), |b| {
        b.iter(|| {
            let r = cranelift
                .run_main(args_with_arg(black_box(40), black_box(2)))
                .expect("cranelift run_main");
            black_box(r);
        });
    });

    // Tree-walker baseline: rebuild context + walker per iter, then
    // run_main. Compares against `cranelift_cold` for cold workloads.
    group.bench_function(BenchmarkId::new("tree_walk", "total"), |b| {
        b.iter(|| {
            let walker = build_tree_walker(tree_walk_src());
            let r = walker
                .run_main(
                    &Arc::new(Scope::default()),
                    args_with(black_box(40), black_box(2)),
                )
                .expect("tree-walk run_main");
            black_box(r);
        });
    });

    // Tree-walker warm: preassembled walker, only `run_main`. The
    // apples-to-apples competitor for `cranelift_warm`.
    let tree_walker = build_tree_walker(tree_walk_src());
    group.bench_function(BenchmarkId::new("tree_walk", "warm"), |b| {
        b.iter(|| {
            let r = tree_walker
                .run_main(
                    &Arc::new(Scope::default()),
                    args_with(black_box(40), black_box(2)),
                )
                .expect("tree-walk warm run_main");
            black_box(r);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_arithmetic);
criterion_main!(benches);
