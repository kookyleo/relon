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
//! Likewise the llvm side cannot run a *full* schema-field workload
//! yet: with `LoadSchemaPtr` now lowered, `from_source` advances past
//! it and stops at `LoadFieldAtAbsolute` — which lives in the `mem`
//! family (`mem.rs::lower_mem_rest`), still a Phase 0b stub and out of
//! scope for this task. `llvm_lowers_load_schema_ptr_blocks_on_field`
//! pins that the failure has moved off `LoadSchemaPtr`, proving this
//! seam is implemented.

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

/// The llvm seam now lowers `LoadSchemaPtr` (ip=0): `from_source`
/// advances past it and fails on the next op, `LoadFieldAtAbsolute`
/// (the `mem` family stub, out of scope here). The failure moving off
/// `LoadSchemaPtr` is the positive signal that this seam is wired.
#[test]
fn llvm_lowers_load_schema_ptr_blocks_on_field() {
    let err = match LlvmAotEvaluator::from_source(SCHEMA_FIELD_SRC) {
        Ok(_) => {
            // If a future change lands `LoadFieldAtAbsolute` too, the
            // build succeeds — then assert the end-to-end value matches
            // the tree-walk reference instead.
            let ev = LlvmAotEvaluator::from_source(SCHEMA_FIELD_SRC).expect("rebuild");
            let out = ev.run_main(point_arg(3, 4)).expect("llvm run_main");
            assert_eq!(out, Value::Int(7), "llvm schema-field value mismatch");
            return;
        }
        Err(e) => e.to_string(),
    };
    assert!(
        !err.contains("LoadSchemaPtr"),
        "LoadSchemaPtr should be lowered now; failure must have moved past it: {err}"
    );
    assert!(
        err.contains("LoadFieldAtAbsolute"),
        "expected the residual block to be LoadFieldAtAbsolute (mem-family stub), got: {err}"
    );
}
