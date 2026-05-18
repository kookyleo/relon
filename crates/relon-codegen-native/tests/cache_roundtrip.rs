//! End-to-end: synthesise IR -> serialize to cache bytes ->
//! deserialize -> build evaluator -> invoke -> result matches the
//! freshly-built evaluator.
//!
//! Covers the v5-beta-1 HelloWorld scenario #6 from the brief.

use std::collections::HashMap;

use relon_codegen_native::{
    deserialize_cache, serialize_cache, CacheEntry, CraneliftAotEvaluator, SandboxConfig,
};
use relon_eval_api::{Evaluator, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::TokenRange;

fn build_mul_ir() -> IrModule {
    IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body: vec![
                TaggedOp {
                    op: Op::LocalGet(0),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::LocalGet(1),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Mul(IrType::I64),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Return,
                    range: TokenRange::default(),
                },
            ],
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

#[test]
fn cache_round_trip_preserves_run_main_result() {
    let ir = build_mul_ir();
    let sandbox = SandboxConfig::default();

    // Fresh build for the baseline answer.
    let fresh = CraneliftAotEvaluator::from_ir_direct(
        ir.clone(),
        sandbox.clone(),
        vec!["x".to_string(), "y".to_string()],
    )
    .expect("fresh compile");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(6));
    args.insert("y".to_string(), Value::Int(7));
    let fresh_result = fresh.run_main(args.clone()).expect("fresh run_main");
    assert_eq!(fresh_result, Value::Int(42));

    // Round-trip through the cache.
    let entry = CacheEntry {
        ir: ir.clone(),
        sandbox: sandbox.clone(),
    };
    let bytes = serialize_cache(&entry).expect("serialize");
    assert!(bytes.len() > 32, "cache blob should be non-trivial");
    let decoded = deserialize_cache(&bytes).expect("deserialize");
    let cached = CraneliftAotEvaluator::from_cache(decoded).expect("from_cache");

    // The cache layer drops parameter names (they're presentation
    // metadata, not part of the IR); rebuild the arg map keyed by
    // the synthetic positional names.
    let mut cached_args = HashMap::new();
    cached_args.insert("arg0".to_string(), Value::Int(6));
    cached_args.insert("arg1".to_string(), Value::Int(7));
    let cached_result = cached.run_main(cached_args).expect("cached run_main");
    assert_eq!(cached_result, fresh_result);
}

#[test]
fn cache_blob_corruption_is_caught_at_deserialize_time() {
    let entry = CacheEntry {
        ir: build_mul_ir(),
        sandbox: SandboxConfig::default(),
    };
    let mut bytes = serialize_cache(&entry).expect("serialize");
    // Flip a byte in the IR payload area to invalidate the sha256.
    bytes[20] ^= 0xff;
    let err = deserialize_cache(&bytes).expect_err("must reject");
    assert!(format!("{err}").contains("sha256"));
}

#[test]
fn cache_preserves_sandbox_config_flags() {
    let entry = CacheEntry {
        ir: build_mul_ir(),
        sandbox: SandboxConfig {
            bounds_check: true,
            deadline_check: false,
            capability_check: true,
            div_check: false,
            trace_jit_fn_id: None,
        },
    };
    let bytes = serialize_cache(&entry).expect("serialize");
    let decoded = deserialize_cache(&bytes).expect("deserialize");
    assert!(decoded.sandbox.bounds_check);
    assert!(!decoded.sandbox.deadline_check);
    assert!(decoded.sandbox.capability_check);
    assert!(!decoded.sandbox.div_check);
}
