//! Phase 1 Stage 2.① — Float native-signature marshalling.
//!
//! Before this lane the LLVM AOT-binding signature surface
//! (`emitted_field_type_for` / `lower_field_descriptors`) rejected
//! `Float`: `#main(Float) -> Float` could JIT through MCJIT but could
//! not be lowered to a native `.o` for the `relon-rs-build` binding
//! generator. This test pins the two halves of the fix:
//!
//!   1. **Native AOT-binding path** — `emit_object` now lowers a
//!      Float-typed `#main` through the buffer protocol, emitting an
//!      8/8-inline f64 slot in the per-field metadata the binding
//!      generator consumes (the descriptor surface that used to error).
//!
//!   2. **Real value e2e** — driving the same source with concrete
//!      Float (and mixed Float/Int) inputs produces real f64 values,
//!      cross-checked **bit-for-bit** (`f64::to_bits`) against the
//!      `TreeWalkEvaluator` gold standard and the cranelift backend.
//!      `run_main` for a Float-param schema routes through
//!      `run_main_buffer`, which shares `write_value_into_builder` /
//!      `read_value_from_reader` byte-for-byte with the native binary's
//!      JIT body — so a buffer-path value match is the body half of the
//!      native e2e.
//!
//! No algorithm substitution, no tolerance fudge: a NaN-payload or
//! signed-zero divergence surfaces as a bit mismatch.

use std::collections::HashMap;
use std::sync::Arc;

use ordered_float::OrderedFloat;
use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::{EmittedEntryShape, EmittedFieldType, LlvmAotEvaluator};
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// `#main(Float) -> Float`: pure-Float scalar arithmetic.
const FLOAT_SRC: &str = "#main(Float x) -> Float\nx * 2.5 + 1.0\n";

/// Mixed Float/Int args + Float return — exercises the I64↔F64 promotion
/// path alongside the new Float marshalling on both ends.
const MIXED_SRC: &str = "#main(Float x, Int n) -> Float\nx + n / 2.0\n";

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Float(f) => f.into_inner(),
        Value::Int(n) => *n as f64,
        other => panic!("expected Float result, got {other:?}"),
    }
}

/// Tree-walk gold standard for `src` on the given arg map.
fn oracle(src: &str, args: HashMap<String, Value>) -> f64 {
    let node = parse_document(src).expect("parse src");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope = Arc::new(Scope::default());
    as_f64(&walker.run_main(&scope, args).expect("tree-walker run_main"))
}

fn f(x: f64) -> Value {
    Value::Float(OrderedFloat(x))
}

/// Emit a native object for `src` to a unique tmp path and return the
/// build.rs-facing metadata. Proves the AOT-binding signature surface
/// (the path that used to reject Float).
fn emit_to_tmp(name: &str, src: &str) -> Result<relon_codegen_llvm::EmitObjectInfo, String> {
    let tmp_dir =
        std::env::temp_dir().join(format!("relon_aot_float_{name}_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("create tmp dir: {e}"))?;
    let out = tmp_dir.join(format!("{name}.o"));
    let symbol = format!("__test_aot_float_{name}");
    let info = LlvmAotEvaluator::emit_object(src, &symbol, &out).map_err(|e| format!("{e:?}"))?;
    // The emitter must have written a real ELF object — not just metadata.
    let bytes = std::fs::metadata(&out)
        .map_err(|e| format!("stat .o: {e}"))?
        .len();
    if bytes == 0 {
        return Err("emit_object produced an empty .o".to_string());
    }
    Ok(info)
}

/// The native AOT-binding path lowers a Float-param / Float-return
/// `#main` through the buffer protocol with 8/8-inline f64 slots — the
/// descriptor surface `relon-rs-build` stamps into the generated shim.
#[test]
fn float_emit_object_native_descriptors() {
    let info = emit_to_tmp("pure_float", FLOAT_SRC).expect("emit_object accepts Float now");
    assert_eq!(info.shape, EmittedEntryShape::Buffer);
    assert_eq!(info.param_names, vec!["x".to_string()]);
    assert_eq!(info.main_fields.len(), 1);
    assert_eq!(info.main_fields[0].ty, EmittedFieldType::Float);
    assert_eq!(info.return_fields.len(), 1);
    assert_eq!(info.return_fields[0].ty, EmittedFieldType::Float);
    assert!(info.main_root_size >= 8, "f64 slot is 8 bytes inline");
    assert!(info.return_root_size >= 8);
}

/// Mixed Float/Int signature also lowers natively: two main fields
/// (Float + Int), one Float return field.
#[test]
fn mixed_float_int_emit_object_native_descriptors() {
    let info = emit_to_tmp("mixed", MIXED_SRC).expect("emit_object accepts mixed Float/Int");
    assert_eq!(info.shape, EmittedEntryShape::Buffer);
    assert_eq!(info.main_fields.len(), 2);
    let by_name: HashMap<&str, EmittedFieldType> = info
        .main_fields
        .iter()
        .map(|fd| (fd.name.as_str(), fd.ty))
        .collect();
    assert_eq!(by_name.get("x"), Some(&EmittedFieldType::Float));
    assert_eq!(by_name.get("n"), Some(&EmittedFieldType::Int));
    assert_eq!(info.return_fields.len(), 1);
    assert_eq!(info.return_fields[0].ty, EmittedFieldType::Float);
}

/// Three-way bit-identical value e2e for `#main(Float) -> Float`: the
/// LLVM buffer body and the cranelift backend must each match the
/// tree-walk oracle bit-for-bit over a spread of inputs (including the
/// NaN / signed-zero edge cases the `to_bits` compare is there to catch).
#[test]
fn float_value_e2e_three_way_bit_identical() {
    let llvm = LlvmAotEvaluator::from_source(FLOAT_SRC)
        .unwrap_or_else(|e| panic!("LLVM from_source: {e:?}"));
    let cl = AotEvaluator::from_source(FLOAT_SRC)
        .unwrap_or_else(|e| panic!("cranelift from_source: {e:?}"));

    let inputs = [
        0.0f64,
        -0.0,
        1.0,
        -3.5,
        2.5,
        1e9,
        -1e-9,
        f64::INFINITY,
        f64::NEG_INFINITY,
        std::f64::consts::PI,
    ];
    for &x in &inputs {
        let mut a = HashMap::new();
        a.insert("x".to_string(), f(x));

        let want = oracle(FLOAT_SRC, a.clone());
        let got_llvm = as_f64(&llvm.run_main(a.clone()).expect("llvm run_main"));
        let got_cl = as_f64(&cl.run_main(a.clone()).expect("cranelift run_main"));

        assert_eq!(
            got_llvm.to_bits(),
            want.to_bits(),
            "LLVM Float e2e diverged at x={x}: llvm={got_llvm} oracle={want}"
        );
        assert_eq!(
            got_cl.to_bits(),
            want.to_bits(),
            "cranelift Float e2e diverged at x={x}: cl={got_cl} oracle={want}"
        );
    }
}

/// Three-way bit-identical value e2e for the mixed Float/Int signature.
#[test]
fn mixed_float_int_value_e2e_three_way_bit_identical() {
    let llvm = LlvmAotEvaluator::from_source(MIXED_SRC)
        .unwrap_or_else(|e| panic!("LLVM from_source: {e:?}"));
    let cl = AotEvaluator::from_source(MIXED_SRC)
        .unwrap_or_else(|e| panic!("cranelift from_source: {e:?}"));

    for &(x, n) in &[(0.0f64, 0i64), (1.5, 4), (-2.25, 7), (100.0, -3), (0.1, 1)] {
        let mut a = HashMap::new();
        a.insert("x".to_string(), f(x));
        a.insert("n".to_string(), Value::Int(n));

        let want = oracle(MIXED_SRC, a.clone());
        let got_llvm = as_f64(&llvm.run_main(a.clone()).expect("llvm run_main"));
        let got_cl = as_f64(&cl.run_main(a.clone()).expect("cranelift run_main"));

        assert_eq!(
            got_llvm.to_bits(),
            want.to_bits(),
            "LLVM mixed e2e diverged at x={x} n={n}: llvm={got_llvm} oracle={want}"
        );
        assert_eq!(
            got_cl.to_bits(),
            want.to_bits(),
            "cranelift mixed e2e diverged at x={x} n={n}: cl={got_cl} oracle={want}"
        );
    }
}
