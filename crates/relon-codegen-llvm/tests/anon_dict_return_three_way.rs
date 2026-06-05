//! `#main() -> Dict { ... }` anon-Dict return marshalling — cross-backend
//! parity + silent-miscompile guards.
//!
//! The anon-Dict return path (`anon_dict_return_plan` /
//! `lower_anon_dict_body`) lifts a bare `-> Dict { ... }` body into a
//! synthesised return record. Pointer-indirect fields (String,
//! `List<scalar>`, `List<String>`) must be copied into the return
//! buffer's tail via `EmitTailRecordFromAbsoluteAddr` before their
//! fixed-area slot can hold the (buffer-relative) offset — without that
//! copy the slot held a raw arena pointer and the host reader decoded
//! garbage (the W7 silent empty-String mis-compile).
//!
//! These tests pin the marshalling against the tree-walk gold standard
//! (bit-equal, field-by-field including String contents) on both native
//! compiled backends, and assert that every still-unsupported return
//! shape fails *loudly* at compile time rather than returning wrong data.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// Tree-walk gold standard for a `#main()` source (no params).
fn run_tree_walk(src: &str) -> Value {
    use relon_evaluator::{Context, TreeWalkEvaluator};
    use relon_parser::parse_document;
    let node = parse_document(src).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    let ctx = Arc::new({
        let mut ctx = ctx;
        TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    });
    TreeWalkEvaluator::new(Arc::clone(&ctx))
        .run_main(
            &Arc::new(relon_eval_api::scope::Scope::default()),
            HashMap::new(),
        )
        .expect("tree-walk run_main")
}

/// Normalise a returned Dict into a sorted `(key, Value)` list so the
/// brand / map-iteration-order differences between executors do not
/// matter — only the field set + per-field value content does.
fn fields_of(v: &Value) -> Vec<(String, Value)> {
    match v {
        Value::Dict(d) => {
            let mut out: Vec<(String, Value)> = d
                .map
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }
        other => panic!("expected Dict return, got {other:?}"),
    }
}

/// Assert tree-walk == cranelift == llvm field-by-field for a source.
fn assert_three_way(src: &str) -> Vec<(String, Value)> {
    let tw = fields_of(&run_tree_walk(src));

    let cl = AotEvaluator::from_source(src).expect("cranelift from_source");
    let cl_v = fields_of(&cl.run_main(HashMap::new()).expect("cranelift run_main"));
    assert_eq!(cl_v, tw, "cranelift vs tree-walk mismatch for `{src}`");

    let llvm = LlvmAotEvaluator::from_source(src).expect("llvm from_source");
    let llvm_v = fields_of(&llvm.run_main(HashMap::new()).expect("llvm run_main"));
    assert_eq!(llvm_v, tw, "llvm vs tree-walk mismatch for `{src}`");

    tw
}

/// The original silent-miscompile: a String field of an anon-Dict
/// return was stored as a raw arena pointer and decoded to "". Must now
/// be the literal `"alpha"` on every backend.
#[test]
fn anon_dict_string_field_is_not_silently_empty() {
    const SRC: &str = "#main() -> Dict\n{ host: \"alpha\", port: 1 }";
    let fields = assert_three_way(SRC);
    assert_eq!(
        fields,
        vec![
            ("host".to_string(), Value::String("alpha".into())),
            ("port".to_string(), Value::Int(1)),
        ],
        "host must marshal as the literal \"alpha\", not an empty string"
    );
}

/// `List<scalar>` fields (Int / Float / Bool) marshal field-by-field.
#[test]
fn anon_dict_scalar_list_fields() {
    assert_three_way("#main() -> Dict\n{ name: \"x\", nums: [1, 2, 3] }");
    assert_three_way("#main() -> Dict\n{ flags: [true, false], fs: [1.5, 2.5] }");
}

/// `List<String>` field marshals through the pointer-array copy +
/// inner-offset relocation.
#[test]
fn anon_dict_list_string_field() {
    let fields =
        assert_three_way("#main() -> Dict\n{ name: \"x\", keys: [\"a\", \"bb\", \"ccc\"] }");
    let keys = fields
        .iter()
        .find(|(k, _)| k == "keys")
        .map(|(_, v)| v.clone())
        .expect("keys field present");
    assert_eq!(
        keys,
        Value::List(Arc::new(vec![
            Value::String("a".into()),
            Value::String("bb".into()),
            Value::String("ccc".into()),
        ]))
    );
}

/// Mixed scalar + String + list fields in one record.
#[test]
fn anon_dict_mixed_fields() {
    assert_three_way(
        "#main() -> Dict\n{ host: \"alpha\", port: 8080, tags: [\"a\", \"b\"], scores: [10, 20] }",
    );
}

/// A `#internal` collection field stays off the host-visible surface on
/// every backend (the tree-walk oracle drops it too), while the
/// non-internal sibling is still marshalled.
#[test]
fn anon_dict_internal_field_dropped_consistently() {
    let fields =
        assert_three_way("#main() -> Dict\n{ name: \"x\", #internal\n keys: [\"a\", \"b\"] }");
    assert!(
        fields.iter().all(|(k, _)| k != "keys"),
        "#internal keys must not surface to the host"
    );
    assert_eq!(fields.len(), 1, "only `name` should survive");
}

/// A `#internal` *scalar* field must also be dropped from the host
/// surface — the compiled backends used to keep it (surfacing a field
/// the tree-walk oracle omits), a silent field-set divergence.
#[test]
fn anon_dict_internal_scalar_dropped_consistently() {
    let fields = assert_three_way("#main() -> Dict\n{ #internal\n tmp: 5, result: 10 }");
    assert_eq!(
        fields,
        vec![("result".to_string(), Value::Int(10))],
        "#internal scalar `tmp` must not surface to the host"
    );
}

/// Red-line guard: every still-unsupported anon-Dict return shape MUST
/// fail loudly at compile time (`from_source` Err) rather than return
/// wrong/partial data. A silent miscompile here is the bug class this
/// whole change set exists to kill, so the assertion is on *both*
/// compiled backends.
#[test]
fn unsupported_return_shapes_fail_loudly_not_silently() {
    const UNSUPPORTED: &[&str] = &[
        // Plain (non-#internal) Dict-valued field — Dict is not a buffer
        // return type; tree-walk would surface it, so it must error.
        "#main() -> Dict\n{ name: \"x\", m: { a: 1, b: 2 } }",
        // List<Schema> field.
        "#schema P {\n    Int x: *\n}\n#main() -> Dict\n{ name: \"x\", ps: [{x:1},{x:2}] }",
        // List<List<scalar>> field.
        "#main() -> Dict\n{ name: \"x\", nested: [[1, 2], [3, 4]] }",
        // Empty list — element type cannot be inferred for marshalling.
        "#main() -> Dict\n{ name: \"x\", empty: [] }",
        // Heterogeneous list.
        "#main() -> Dict\n{ name: \"x\", mixed: [1, \"a\"] }",
        // Top-level List<Schema> / List<List> returns.
        "#schema P {\n    Int x: *\n}\n#main() -> List<P>\n[{x:1},{x:2}]",
        "#main() -> List<List<Int>>\n[[1, 2], [3, 4]]",
        // Pointer-array list returns sourced from a parameter (NOT a
        // const-pool list literal) inside an OBJECT field. The top-level
        // `#main(List<String> ss) -> List<String> = ss` identity is now
        // lifted by the S3 in-place region-walk return (proven bit-equal in
        // `relon-test-harness/tests/return_inplace_list_string.rs`), so it
        // is no longer in this cap list. The branded-struct *field* surface
        // (`-> S { tags: t }`) is object-field marshalling, a later step,
        // and must still fail loudly at lowering.
        "#schema S {\n    tags: List<String>\n}\n#main(List<String> t) -> S\n{ tags: t }",
    ];
    for src in UNSUPPORTED {
        let cl = AotEvaluator::from_source(src);
        assert!(
            cl.is_err(),
            "cranelift must reject unsupported return shape loudly, but compiled: `{src}`"
        );
        let llvm = LlvmAotEvaluator::from_source(src);
        assert!(
            llvm.is_err(),
            "llvm must reject unsupported return shape loudly, but compiled: `{src}`"
        );
    }
}
