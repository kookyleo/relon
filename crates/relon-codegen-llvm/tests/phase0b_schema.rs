//! Phase 0b — LLVM-AOT `Op::LoadSchemaPtr` lowering.
//!
//! `Op::LoadSchemaPtr { offset }` lifts a schema-typed `#main`
//! parameter's pointer slot (a 4-byte buffer-relative offset stored at
//! `in_ptr + offset`) to the absolute (arena-relative) address of the
//! schema instance's fixed area, by reading the slot and adding
//! `in_ptr`. The result feeds `Op::LoadFieldAtAbsolute`, which then
//! composes `arena_base + base + field_offset`.
//!
//! Three-way alignment caveat (verified, not assumed): the task brief
//! names cranelift the gold standard, but cranelift does NOT lower
//! `LoadSchemaPtr` — `AotEvaluator::from_source` on a schema-field
//! workload fails with "unsupported op in v5-beta-2 stage 3:
//! LoadSchemaPtr" (see `cranelift_declines_load_schema_ptr`). The
//! canonical reference for the end-to-end value is therefore the
//! tree-walker (`reference_tree_walk_value`).
//!
//! With both `LoadSchemaPtr` (this seam) and `LoadFieldAtAbsolute` (the
//! `mem` family) now lowered, the schema-field workload *compiles*:
//! `from_source` succeeds. The end-to-end *run* is still blocked, but
//! downstream of codegen — in the shared Phase-B host arg marshaller
//! (`evaluator.rs::write_value_into_builder` has only scalar arms, no
//! Schema-typed `#main` param support). `llvm_schema_field_compiles_blocked_on_arg_marshalling`
//! pins that boundary: build OK, run fails at the marshalling envelope
//! with no unsupported-op error.

use std::collections::HashMap;
use std::sync::Arc;

use relon_analyzer::analyze;
use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value, ValueDict};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// `#main(Point p) -> Int : p.x + p.y` — the minimal workload whose
/// lowering emits `LoadSchemaPtr` (schema-typed param) followed by
/// `LoadFieldAtAbsolute` (the field reads).
const SCHEMA_FIELD_SRC: &str = "\
#schema Point {
    Int x: *,
    Int y: *
}
#main(Point p) -> Int
p.x + p.y
";

fn point_arg(x: i64, y: i64) -> HashMap<String, Value> {
    let dict = ValueDict::with_brand(
        [
            ("x".to_string(), Value::Int(x)),
            ("y".to_string(), Value::Int(y)),
        ],
        Some("Point".to_string()),
    );
    let mut args = HashMap::new();
    args.insert("p".to_string(), Value::Dict(Arc::new(dict)));
    args
}

/// Canonical reference: the tree-walker evaluates the schema-field
/// workload end to end. `Point { x: 3, y: 4 }.x + .y == 7`.
#[test]
fn reference_tree_walk_value() {
    let node = parse_document(SCHEMA_FIELD_SRC).expect("parse");
    let analyzed = Arc::new(analyze(&node));
    let ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    let ev = TreeWalkEvaluator::new(Arc::new({
        let mut ctx = ctx;
        TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    }));
    let out = ev
        .run_main(&Arc::new(Scope::default()), point_arg(3, 4))
        .expect("tree-walk run_main");
    assert_eq!(out, Value::Int(7));
}

/// Differential note: cranelift declines `LoadSchemaPtr`, so it cannot
/// serve as a gold standard for this op. Pinned so a future cranelift
/// implementation flips this test (and we revisit the three-way story).
#[test]
fn cranelift_declines_load_schema_ptr() {
    let err = match AotEvaluator::from_source(SCHEMA_FIELD_SRC) {
        Ok(_) => panic!("cranelift unexpectedly accepted LoadSchemaPtr workload"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("LoadSchemaPtr"),
        "expected cranelift to decline at LoadSchemaPtr, got: {err}"
    );
}

/// With `LoadSchemaPtr` (this seam) and `LoadFieldAtAbsolute` (mem
/// family) both lowered, the schema-field workload compiles —
/// `from_source` succeeds, proving this seam is wired. The end-to-end
/// run is blocked solely by the shared Phase-B arg marshaller (no
/// Schema-typed `#main` param arm). Asserts that boundary: build OK,
/// run fails at marshalling, no unsupported-op error. When Phase 1
/// wires schema-arg marshalling this must upgrade to
/// `assert_eq!(out, Value::Int(7))` (the tree-walk reference above).
#[test]
fn llvm_schema_field_compiles_blocked_on_arg_marshalling() {
    let ev = LlvmAotEvaluator::from_source(SCHEMA_FIELD_SRC)
        .expect("LoadSchemaPtr + LoadFieldAtAbsolute both lower: must compile");
    let err = ev
        .run_main(point_arg(3, 4))
        .expect_err("schema-typed #main arg is not marshalled in Phase B yet");
    let msg = err.to_string();
    assert!(
        msg.contains("schema expects Schema"),
        "block must be the arg-marshalling envelope, not codegen: {msg}"
    );
    assert!(
        !msg.contains("LoadSchemaPtr") && !msg.contains("LoadFieldAtAbsolute"),
        "neither op may surface as unsupported anymore: {msg}"
    );
}
