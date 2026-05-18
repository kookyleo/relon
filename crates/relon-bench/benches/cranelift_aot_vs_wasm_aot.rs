//! v5-beta-1 closeout bench: pit the cranelift-native AOT backend
//! against the wasm-AOT backend across the narrow arithmetic
//! scenario both can express today.
//!
//! Each scenario splits into:
//!
//! * `cranelift_cold` — `CraneliftAotEvaluator::from_ir_direct` from
//!   synthetic IR. Cranelift JIT compile + finalize.
//! * `cranelift_warm` — preassembled evaluator, time only
//!   `run_main(args)`. The single-call latency target the brief
//!   mentions ("LuaJIT trace tier", roughly < 3 μs).
//! * `wasm_cold` — `WasmAotEvaluator::from_source`. Includes parse +
//!   analyze + lower + codegen + wasmtime module new.
//! * `wasm_warm` — preassembled wasm evaluator, time only
//!   `run_main(args)`. The wasm-AOT baseline the cranelift path is
//!   benchmarked against.
//!
//! Scope: the cranelift backend's narrow envelope (Int->Int arithmetic
//! only) means the wasm side runs an equivalent source. The relon-ir
//! crate isn't directly exposing IR shapes for the wasm path so we
//! drive both backends from their natural entry points (synthetic IR
//! for cranelift; source code for wasm) — the arithmetic semantics
//! match.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use relon_codegen_native::{CraneliftAotEvaluator, SandboxConfig};
use relon_codegen_wasm::WasmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::TokenRange;

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

fn wasm_src() -> &'static str {
    "#main(Int x, Int y) -> Int\nx + y"
}

fn args_with(x: i64, y: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(2);
    m.insert("x".to_string(), Value::Int(x));
    m.insert("y".to_string(), Value::Int(y));
    m
}

fn args_with_arg(x: i64, y: i64) -> HashMap<String, Value> {
    // Cranelift backend uses synthetic param names when constructed
    // from raw IR.
    let mut m = HashMap::with_capacity(2);
    m.insert("arg0".to_string(), Value::Int(x));
    m.insert("arg1".to_string(), Value::Int(y));
    m
}

fn bench_arithmetic(c: &mut Criterion) {
    let mut group = c.benchmark_group("v5b1_arithmetic");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(5));

    // Cranelift cold start.
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

    // Cranelift warm invoke. Reuse one preassembled evaluator across
    // every iter.
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

    // Wasm-AOT cold start.
    group.bench_function(BenchmarkId::new("wasm", "cold"), |b| {
        b.iter(|| {
            let aot = WasmAotEvaluator::from_source(wasm_src()).expect("wasm compile");
            black_box(aot);
        });
    });

    // Wasm-AOT warm invoke.
    let wasm = WasmAotEvaluator::from_source(wasm_src()).expect("wasm preassemble");
    group.bench_function(BenchmarkId::new("wasm", "warm"), |b| {
        b.iter(|| {
            let r = wasm
                .run_main(args_with(black_box(40), black_box(2)))
                .expect("wasm run_main");
            black_box(r);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_arithmetic);
criterion_main!(benches);
